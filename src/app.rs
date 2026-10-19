//! Общее состояние и сценарии Resume / Stop. Оба идут в фоне, UI опрашивает строку.

use std::{
    collections::HashMap,
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

pub struct App {
    pub cfg: Config,
    pub scanner: Arc<Scanner>,
    pub tmux: Tmux,
    pub git: GitCtx,
    ops: Mutex<HashMap<Uuid, Op>>,
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

    /// Запускает Resume в фоне. `false`, если по сессии уже идёт операция.
    /// `&Arc<Self>`: фоновая задача переживает запрос и держит весь App.
    pub fn spawn_start(self: &Arc<Self>, id: Uuid) -> bool {
        if !self.try_begin(id, Op::Starting) {
            return false;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let res = me.start(id).await;
            if let Err(e) = &res {
                tracing::warn!(%id, "resume: {e:#}");
            }
            me.finish(id, res);
        });
        true
    }

    pub fn spawn_stop(self: &Arc<Self>, id: Uuid) -> bool {
        if !self.try_begin(id, Op::Stopping) {
            return false;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let res = me.stop(id).await;
            if let Err(e) = &res {
                tracing::warn!(%id, "stop: {e:#}");
            }
            me.finish(id, res);
        });
        true
    }

    /// Занят ли git root другой сессией: запущенной в tmux или не вывезенной.
    async fn root_busy(
        &self,
        id: Uuid,
        root: &Path,
        panes: &HashMap<Uuid, Pane>,
    ) -> Option<String> {
        let sessions = self.sessions().await;
        let title = |oid: &Uuid| {
            sessions
                .iter()
                .find(|s| s.id == *oid)
                .map(|s| s.title())
                .unwrap_or_else(|| oid.to_string())
        };
        for (oid, pane) in panes {
            if *oid == id || pane.dead {
                continue;
            }
            let other_root = sessions
                .iter()
                .find(|s| s.id == *oid)
                .and_then(Session::git_root);
            if other_root.as_deref() == Some(root) {
                return Some(format!("в этом репо уже работает сессия «{}»", title(oid)));
            }
        }
        for (oid, st) in self.git.all_states() {
            if oid != id && find_git_root(&st.cwd).as_deref() == Some(root) {
                return Some(format!(
                    "в этом репо не вывезена сессия «{}»: сначала Stop у неё",
                    title(&oid)
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
        let root = find_git_root(&cwd).context("в cwd нет git-репозитория")?;

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

        let git_msg = self.git.prepare(&id, &cwd).await?;

        let claude_json = self.cfg.claude_json.clone();
        let trust_cwd = cwd.clone();
        tokio::task::spawn_blocking(move || trust::ensure_trusted(&claude_json, &trust_cwd))
            .await??;

        let offset = std::fs::metadata(&s.path).map(|m| m.len()).unwrap_or(0);
        let command = format!(
            "exec {} --resume {id} --remote-control",
            shell_quote(&self.cfg.claude_bin)
        );
        self.tmux.start(&id, &cwd, &command).await?;

        let deadline = Instant::now() + self.cfg.start_timeout;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let path = s.path.clone();
            let bridge = tokio::task::spawn_blocking(move || sessions::bridge_after(&path, offset))
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
        let title = session.as_ref().map(Session::title).unwrap_or_default();
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
