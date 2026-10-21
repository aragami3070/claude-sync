//! Общее состояние и сценарии Resume / New / Stop. Все идут в фоне, UI опрашивает строку.

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::{
    cleanup,
    config::Config,
    git::GitCtx,
    sessions::{self, Scanner, Session},
    tmux::{Pane, Tmux},
    trust,
    util::{find_git_root, shell_quote, tail_lines},
};

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Starting,
    Stopping,
    Done(String),
    Failed(String),
}

impl Op {
    pub fn in_progress(&self) -> bool {
        matches!(self, Op::Starting | Op::Stopping)
    }
}

/// Новая сессия из формы.
#[derive(Debug, Clone)]
pub struct NewReq {
    /// Рабочая папка claude; при `url` — куда клонировать.
    pub cwd: PathBuf,
    pub url: Option<String>,
    pub name: Option<String>,
}

/// Новая сессия, у которой ещё нет транскрипта с сообщениями: по ней рисуется строка.
#[derive(Debug, Clone)]
pub struct Fresh {
    pub cwd: PathBuf,
    pub name: Option<String>,
}

pub struct App {
    pub cfg: Config,
    pub scanner: Arc<Scanner>,
    pub tmux: Tmux,
    pub git: GitCtx,
    ops: Mutex<HashMap<Uuid, Op>>,
    fresh: Mutex<HashMap<Uuid, Fresh>>,
}

impl App {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            scanner: Arc::new(Scanner::new(&cfg.claude_home)),
            tmux: Tmux::new(cfg.tmux_socket.clone()),
            git: GitCtx {
                remote: cfg.git_remote.clone(),
                wip_prefix: cfg.wip_prefix.clone(),
                node: cfg.node_name.clone(),
                state_dir: cfg.state_dir.clone(),
            },
            ops: Mutex::new(HashMap::new()),
            fresh: Mutex::new(HashMap::new()),
            cfg,
        })
    }

    /// Сканирование читает файлы, поэтому в blocking-пуле; туда уходит только сканер.
    pub async fn sessions(&self) -> Vec<Session> {
        let scanner = self.scanner.clone();
        tokio::task::spawn_blocking(move || scanner.scan())
            .await
            .unwrap_or_default()
    }

    pub async fn session(&self, id: Uuid) -> Option<Session> {
        let scanner = self.scanner.clone();
        tokio::task::spawn_blocking(move || scanner.get(id))
            .await
            .ok()
            .flatten()
    }

    pub fn op(&self, id: &Uuid) -> Option<Op> {
        self.ops.lock().unwrap().get(id).cloned()
    }

    pub fn ops_snapshot(&self) -> HashMap<Uuid, Op> {
        self.ops.lock().unwrap().clone()
    }

    pub fn clear_op(&self, id: &Uuid) {
        let mut ops = self.ops.lock().unwrap();
        if ops.get(id).is_some_and(|o| !o.in_progress()) {
            ops.remove(id);
        }
    }

    pub fn fresh(&self, id: &Uuid) -> Option<Fresh> {
        self.fresh.lock().unwrap().get(id).cloned()
    }

    pub fn fresh_snapshot(&self) -> HashMap<Uuid, Fresh> {
        self.fresh.lock().unwrap().clone()
    }

    pub fn forget_fresh(&self, id: &Uuid) {
        self.fresh.lock().unwrap().remove(id);
    }

    /// Ставит операцию, если по этой сессии ничего не идёт.
    fn try_begin(&self, id: Uuid, op: Op) -> bool {
        let mut ops = self.ops.lock().unwrap();
        if ops.get(&id).is_some_and(Op::in_progress) {
            return false;
        }
        ops.insert(id, op);
        true
    }

    fn finish(&self, id: Uuid, res: Result<String>) {
        let op = match res {
            Ok(msg) => Op::Done(msg),
            Err(e) => Op::Failed(format!("{e:#}")),
        };
        self.ops.lock().unwrap().insert(id, op);
    }

    /// Фоновая операция над сессией. `false`, если по ней уже что-то идёт.
    /// `&Arc<Self>`: задача переживает запрос и держит весь App.
    fn spawn_op<F, Fut>(self: &Arc<Self>, id: Uuid, op: Op, what: &'static str, f: F) -> bool
    where
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        if !self.try_begin(id, op) {
            return false;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let res = f(me.clone()).await;
            if let Err(e) = &res {
                tracing::warn!(%id, "{what}: {e:#}");
            }
            me.finish(id, res);
        });
        true
    }

    pub fn spawn_start(self: &Arc<Self>, id: Uuid) -> bool {
        self.spawn_op(id, Op::Starting, "resume", move |me| async move {
            me.start(id).await
        })
    }

    pub fn spawn_stop(self: &Arc<Self>, id: Uuid) -> bool {
        self.spawn_op(id, Op::Stopping, "stop", move |me| async move {
            me.stop(id).await
        })
    }

    /// Новая сессия с заранее выбранным id: по нему строка в UI найдёт её до первого промпта.
    pub fn spawn_new(self: &Arc<Self>, req: NewReq) -> Uuid {
        let id = Uuid::new_v4();
        self.fresh.lock().unwrap().insert(
            id,
            Fresh {
                cwd: req.cwd.clone(),
                name: req.name.clone(),
            },
        );
        self.spawn_op(id, Op::Starting, "new", move |me| async move {
            me.new_session(id, req).await
        });
        id
    }

    /// Занят ли git root другой сессией: запущенной в tmux или не вывезенной.
    async fn root_busy(
        &self,
        id: Uuid,
        root: &Path,
        panes: &HashMap<Uuid, Pane>,
    ) -> Option<String> {
        let sessions = self.sessions().await;
        let states: HashMap<Uuid, PathBuf> = self
            .git
            .all_states()
            .into_iter()
            .map(|(oid, st)| (oid, st.cwd))
            .collect();
        let title = |oid: &Uuid| {
            sessions
                .iter()
                .find(|s| s.id == *oid)
                .map(|s| s.title())
                .or_else(|| {
                    self.fresh(oid)
                        .map(|f| f.name.unwrap_or_else(|| "новая сессия".into()))
                })
                .unwrap_or_else(|| oid.to_string())
        };
        // У новой сессии до первого промпта нет транскрипта, её папка есть только в state.
        let root_of = |oid: &Uuid| {
            sessions
                .iter()
                .find(|s| s.id == *oid)
                .and_then(Session::git_root)
                .or_else(|| states.get(oid).and_then(|c| find_git_root(c)))
        };
        for (oid, pane) in panes {
            if *oid != id && !pane.dead && root_of(oid).as_deref() == Some(root) {
                return Some(format!("в этом репо уже работает сессия «{}»", title(oid)));
            }
        }
        for (oid, cwd) in &states {
            if *oid != id && find_git_root(cwd).as_deref() == Some(root) {
                return Some(format!(
                    "в этом репо не вывезена сессия «{}»: сначала Stop у неё",
                    title(oid)
                ));
            }
        }
        None
    }

    async fn start(&self, id: Uuid) -> Result<String> {
        let s = self.session(id).await.context("сессия не найдена")?;
        let cwd: PathBuf = s.cwd().context("в транскрипте нет cwd")?.to_path_buf();
        if !cwd.is_dir() {
            bail!("на этой машине нет папки {}", cwd.display());
        }
        let git_msg = self.prepare_dir(id, &cwd).await?;
        let offset = std::fs::metadata(&s.path).map(|m| m.len()).unwrap_or(0);
        self.launch(
            id,
            &cwd,
            &format!("--resume {id}"),
            Some((s.path, offset)),
            git_msg,
        )
        .await
    }

    async fn new_session(&self, id: Uuid, req: NewReq) -> Result<String> {
        let mut notes = Vec::new();
        if let Some(url) = &req.url {
            notes.push(self.git.clone_into(url, &req.cwd).await?);
        }
        if !req.cwd.is_dir() {
            bail!("на этой машине нет папки {}", req.cwd.display());
        }
        notes.push(self.prepare_dir(id, &req.cwd).await?);
        let mut args = format!("--session-id {id}");
        if let Some(name) = &req.name {
            args.push_str(&format!(" -n {}", shell_quote(name)));
        }
        self.launch(id, &req.cwd, &args, None, notes.join("; "))
            .await
    }

    /// Общее для Resume и New: проверки, git, trust.
    async fn prepare_dir(&self, id: Uuid, cwd: &Path) -> Result<String> {
        let root = find_git_root(cwd).context("в папке нет git-репозитория")?;
        let panes = self.tmux.list().await?;
        match panes.get(&id) {
            Some(p) if !p.dead => bail!("сессия уже запущена"),
            Some(_) => {
                let _ = self.tmux.kill(&id).await; // остался мёртвый pane
            }
            None => {}
        }
        if let Some(why) = self.root_busy(id, &root, &panes).await {
            bail!(why);
        }

        let git_msg = self.git.prepare(&id, cwd).await?;

        let claude_json = self.cfg.claude_json.clone();
        let trust_cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || trust::ensure_trusted(&claude_json, &trust_cwd))
            .await??;
        Ok(git_msg)
    }

    /// Запускает claude в tmux и ждёт регистрации Remote Control.
    /// `transcript`: файл и смещение, после которого искать `bridge-session`;
    /// `None` — файла ещё нет, ищем по id.
    async fn launch(
        &self,
        id: Uuid,
        cwd: &Path,
        args: &str,
        transcript: Option<(PathBuf, u64)>,
        git_msg: String,
    ) -> Result<String> {
        let command = format!(
            "exec {} {args} --remote-control",
            shell_quote(&self.cfg.claude_bin)
        );
        self.tmux.start(&id, cwd, &command).await?;

        let deadline = Instant::now() + self.cfg.start_timeout;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let scanner = self.scanner.clone();
            let tr = transcript.clone();
            let bridge = tokio::task::spawn_blocking(move || {
                let (path, offset) = match tr {
                    Some(t) => t,
                    None => (scanner.find_file(id)?, 0),
                };
                sessions::bridge_after(&path, offset)
            })
            .await
            .ok()
            .flatten();
            if bridge.is_some() {
                return Ok(format!("{git_msg}; Remote Control подключён"));
            }
            match self.tmux.list().await?.get(&id) {
                None => bail!("tmux-сессия пропала сразу после запуска"),
                Some(p) if p.dead => {
                    let screen = self.tmux.capture(&id, true).await.unwrap_or_default();
                    let _ = self.tmux.kill(&id).await;
                    bail!(
                        "claude завершился (код {}). Git уже подготовлен, нажми Stop, чтобы вернуть клон.\n{}",
                        p.exit_code.map_or("?".into(), |c| c.to_string()),
                        tail_lines(&screen, 15)
                    );
                }
                Some(_) => {}
            }
            if Instant::now() > deadline {
                return Ok(format!(
                    "{git_msg}; claude запущен, но подключение RC не видно за {} с: посмотри экран",
                    self.cfg.start_timeout.as_secs()
                ));
            }
        }
    }

    async fn stop(&self, id: Uuid) -> Result<String> {
        if let Some(pane) = self.tmux.list().await?.get(&id).copied() {
            if !pane.dead {
                self.tmux.send_exit(&id).await?;
                let deadline = Instant::now() + self.cfg.stop_timeout;
                while Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    match self.tmux.list().await?.get(&id) {
                        Some(p) if !p.dead => {}
                        _ => break,
                    }
                }
            }
            let _ = self.tmux.kill(&id).await;
        }

        let session = self.session(id).await;
        let title = match &session {
            Some(s) => s.title(),
            None => self
                .fresh(&id)
                .and_then(|f| f.name)
                .unwrap_or_else(|| "новая сессия".into()),
        };
        let cwd = match self.git.read_state(&id) {
            Some(st) => st.cwd,
            None => session
                .as_ref()
                .and_then(|s| s.cwd().map(Path::to_path_buf))
                .context("неизвестна папка сессии")?,
        };
        let mut msg = self.git.finalize(&id, &cwd, &title).await?;

        if let Some(root) = find_git_root(&cwd) {
            let panes = self.tmux.list().await.unwrap_or_default();
            if self.root_busy(id, &root, &panes).await.is_none() {
                match cleanup::cargo_clean(&root).await {
                    Ok(Some(s)) => msg = format!("{msg}; {s}"),
                    Ok(None) => {}
                    Err(e) => msg = format!("{msg}; чистка не удалась: {e:#}"),
                }
            }
        }
        Ok(msg)
    }
}
