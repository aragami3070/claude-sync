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
    workspace,
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
        let states = self.git.all_states();
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
        // Репо сессии: её cwd и всё, что записано в state (у новой сессии до
        // первого промпта нет транскрипта, у рабочей папки репо несколько).
        let in_root = |oid: &Uuid| {
            sessions
                .iter()
                .find(|s| s.id == *oid)
                .and_then(Session::git_root)
                .is_some_and(|r| r == root)
                || states
                    .iter()
                    .any(|(sid, st)| sid == oid && find_git_root(&st.cwd).as_deref() == Some(root))
        };
        for (oid, pane) in panes {
            if *oid != id && !pane.dead && in_root(oid) {
                return Some(format!(
                    "в {} уже работает сессия «{}»",
                    self.short(root),
                    title(oid)
                ));
            }
        }
        for (oid, st) in &states {
            if *oid != id && find_git_root(&st.cwd).as_deref() == Some(root) {
                return Some(format!(
                    "в {} не вывезена сессия «{}»: сначала Stop у неё",
                    self.short(root),
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
        let root = find_git_root(cwd);
        if root.is_none() && workspace::child_repos(cwd).is_empty() {
            bail!("ни в папке, ни в дочерних нет git-репозитория");
        }
        let panes = self.tmux.list().await?;
        match panes.get(&id) {
            Some(p) if !p.dead => bail!("сессия уже запущена"),
            Some(_) => {
                let _ = self.tmux.kill(&id).await; // остался мёртвый pane
            }
            None => {}
        }

        let git_msg = match root {
            Some(root) => {
                if let Some(why) = self.root_busy(id, &root, &panes).await {
                    bail!(why);
                }
                self.git.prepare(&id, cwd).await?
            }
            None => self.prepare_workspace(id, cwd, &panes).await?,
        };

        let claude_json = self.cfg.claude_json.clone();
        let trust_cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || trust::ensure_trusted(&claude_json, &trust_cwd))
            .await??;
        Ok(git_msg)
    }

    /// Рабочая папка: готовим репо из `settings.local.json`; нет файла — git не трогаем.
    async fn prepare_workspace(
        &self,
        id: Uuid,
        cwd: &Path,
        panes: &HashMap<Uuid, Pane>,
    ) -> Result<String> {
        if self.git.has_state(&id) {
            bail!("прошлый запуск этой сессии не завершён: сначала нажми Stop");
        }
        let Some(listed) = workspace::listed_repos(cwd, &self.cfg.home)? else {
            return Ok(
                "рабочая папка без .claude/settings.local.json: git не трогаю, \
                       на Stop вывозить нечего"
                    .into(),
            );
        };
        let mut notes: Vec<String> = listed
            .skipped
            .iter()
            .map(|s| format!("пропущено {s}"))
            .collect();
        if listed.repos.is_empty() {
            notes.insert(
                0,
                "в settings.local.json нет репо в additionalDirectories".into(),
            );
            return Ok(notes.join("; "));
        }
        // Сначала проверяем все, чтобы не подготовить половину и упасть.
        for repo in &listed.repos {
            if let Some(why) = self.root_busy(id, repo, panes).await {
                bail!(why);
            }
        }
        for repo in &listed.repos {
            let m = self
                .git
                .prepare_repo(&id, repo, cwd)
                .await
                .with_context(|| {
                    format!(
                        "{}: подготовленные до него репо вернёт Stop",
                        repo.display()
                    )
                })?;
            notes.push(format!("{}: {m}", self.short(repo)));
        }
        Ok(notes.join("; "))
    }

    fn short(&self, p: &Path) -> String {
        match p.strip_prefix(&self.cfg.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        }
    }

    /// Папка сессии: из транскрипта, новой сессии или state.
    async fn session_cwd(&self, id: Uuid, session: Option<&Session>) -> Option<PathBuf> {
        if let Some(c) = session.and_then(Session::cwd) {
            return Some(c.to_path_buf());
        }
        if let Some(f) = self.fresh(&id) {
            return Some(f.cwd);
        }
        if let Some(st) = self.git.read_state(&id) {
            return Some(st.cwd);
        }
        self.git
            .states_of(&id)
            .into_iter()
            .find_map(|st| st.workspace)
    }

    /// Что сказать перед Stop рабочей папки, у которой сейчас нет списка репо.
    /// `None` — можно останавливать без вопросов.
    pub async fn stop_warning(&self, id: Uuid) -> Option<String> {
        let session = self.session(id).await;
        let cwd = self.session_cwd(id, session.as_ref()).await?;
        if find_git_root(&cwd).is_some() {
            return None;
        }
        let why = match workspace::listed_repos(&cwd, &self.cfg.home) {
            Ok(Some(_)) => return None,
            Ok(None) => format!("нет {}", self.short(&workspace::settings_path(&cwd))),
            Err(e) => format!("{e:#}"),
        };
        let started: Vec<String> = self
            .git
            .states_of(&id)
            .iter()
            .map(|st| self.short(&st.cwd))
            .collect();
        let tail = if started.is_empty() {
            "Изменения в репо вывезены не будут.".to_string()
        } else {
            format!(
                "Вывезу только репо, подготовленные при старте: {}.",
                started.join(", ")
            )
        };
        Some(format!("{why}. {tail}"))
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
        let cwd = self
            .session_cwd(id, session.as_ref())
            .await
            .context("неизвестна папка сессии")?;
        if find_git_root(&cwd).is_none() {
            return self.stop_workspace(id, &cwd, &title).await;
        }
        let mut msg = self.git.finalize(&id, &cwd, &title).await?;

        if let Some(root) = find_git_root(&cwd) {
            let panes = self.tmux.list().await.unwrap_or_default();
            if self.root_busy(id, &root, &panes).await.is_none() {
                msg = self.clean(&root, msg).await;
            }
        }
        Ok(msg)
    }

    async fn clean(&self, root: &Path, msg: String) -> String {
        match cleanup::cargo_clean(root).await {
            Ok(Some(s)) => format!("{msg}; {s}"),
            Ok(None) => msg,
            Err(e) => format!("{msg}; чистка не удалась: {e:#}"),
        }
    }

    /// Stop рабочей папки: репо из `settings.local.json` на момент Stop плюс
    /// подготовленные при старте. Ошибка в одном репо не мешает остальным.
    async fn stop_workspace(&self, id: Uuid, cwd: &Path, title: &str) -> Result<String> {
        let started: Vec<PathBuf> = self
            .git
            .states_of(&id)
            .into_iter()
            .map(|st| st.cwd)
            .collect();
        let mut notes = Vec::new();
        let listed = match workspace::listed_repos(cwd, &self.cfg.home) {
            Ok(Some(l)) => {
                notes.extend(l.skipped.iter().map(|s| format!("пропущено {s}")));
                l.repos
            }
            Ok(None) => {
                notes.push("нет settings.local.json".into());
                Vec::new()
            }
            Err(e) => {
                notes.push(format!("{e:#}"));
                Vec::new()
            }
        };
        let mut repos = started.clone();
        repos.extend(listed.into_iter().filter(|r| !started.contains(r)));
        if repos.is_empty() {
            notes.push("сессия остановлена, git не трогал".into());
            return Ok(notes.join("; "));
        }

        let panes = self.tmux.list().await.unwrap_or_default();
        let mut failed = Vec::new();
        for repo in &repos {
            let name = self.short(repo);
            // Не подготовленное при старте репо может быть занято другой сессией:
            // finalize сделал бы там reset --hard.
            if !started.contains(repo)
                && let Some(why) = self.root_busy(id, repo, &panes).await
            {
                notes.push(format!("{name}: пропущено, {why}"));
                continue;
            }
            match self.git.finalize_repo(&id, repo, title).await {
                Ok(m) => {
                    let m = if self.root_busy(id, repo, &panes).await.is_none() {
                        self.clean(repo, m).await
                    } else {
                        m
                    };
                    notes.push(format!("{name}: {m}"));
                }
                Err(e) => failed.push(format!("{name}: {e:#}")),
            }
        }
        if failed.is_empty() {
            Ok(notes.join("; "))
        } else {
            notes.extend(failed);
            bail!("{}", notes.join("\n"))
        }
    }
}
