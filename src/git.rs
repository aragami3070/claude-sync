//! Git на VPS. Правило: в чужие ветки не пишем и никогда не делаем force.
//! Работа VPS уезжает только в `<wip_prefix><session-id>`.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::util::{self, Output};

const TIMEOUT: Duration = Duration::from_secs(120);
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);
/// GitHub отбивает файлы больше 100 МБ; берём с запасом.
const BIG_FILE: u64 = 50 * 1024 * 1024;
const JUNK_SHOWN: usize = 10;

/// Что запомнили при resume; нужно при stop, переживает рестарт морды.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunState {
    /// Где работает git: корень репо (у рабочей папки — одно из её репо).
    pub cwd: PathBuf,
    /// Папка сессии, если это рабочая папка с несколькими репо.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    pub base_branch: String,
    pub started_at: u64,
    pub node: String,
}

pub struct GitCtx {
    pub remote: String,
    pub wip_prefix: String,
    pub node: String,
    pub state_dir: PathBuf,
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<Output> {
    run_git_t(cwd, args, TIMEOUT).await
}

async fn run_git_t(cwd: &Path, args: &[&str], timeout: Duration) -> Result<Output> {
    let mut envs = vec![("GIT_TERMINAL_PROMPT", "0")];
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        // accept-new: незнакомый хост запоминается сам, сменившийся ключ по-прежнему отвергается.
        envs.push((
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
        ));
    }
    util::run("git", args, Some(cwd), &envs, timeout).await
}

/// Только ssh: `user@host:path` или `ssh://…`. https не годится, по нему не запушить.
pub fn check_ssh_url(url: &str) -> Result<()> {
    let clean = !url.starts_with('-') && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    let scp_like = url.split_once(':').is_some_and(|(left, path)| {
        !path.is_empty()
            && !left.contains('/')
            && left
                .split_once('@')
                .is_some_and(|(user, host)| !user.is_empty() && !host.is_empty())
    });
    let ssh_scheme = url
        .strip_prefix("ssh://")
        .is_some_and(|rest| rest.contains('/'));
    if clean && (scp_like || ssh_scheme) {
        Ok(())
    } else {
        bail!("нужна ssh-ссылка вида git@host:user/repo.git или ssh://git@host/user/repo.git")
    }
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = run_git(cwd, args).await?;
    if !out.ok() {
        let msg = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        bail!("git {}: {msg}", args.join(" "));
    }
    Ok(out.stdout.trim().to_string())
}

async fn ref_exists(cwd: &Path, r: &str) -> Result<bool> {
    Ok(run_git(cwd, &["rev-parse", "--verify", "--quiet", r])
        .await?
        .ok())
}

async fn current_branch(cwd: &Path) -> Result<Option<String>> {
    let out = run_git(cwd, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
    Ok(out.ok().then(|| out.stdout.trim().to_string()))
}

/// Неотслеживаемое, что нельзя коммитить на Stop: venv, `node_modules`, большие файлы.
/// Каждая находка — путь относительно репо с причиной.
pub async fn junk_untracked(cwd: &Path) -> Result<Vec<String>> {
    let raw = run_git(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    if !raw.ok() {
        bail!("git ls-files: {}", raw.stderr.trim());
    }
    let mut found: Vec<String> = Vec::new();
    let mut push = |item: String| {
        if !found.contains(&item) {
            found.push(item);
        }
    };
    for rel in raw.stdout.split('\0').filter(|p| !p.is_empty()) {
        let path = Path::new(rel);
        if let Some(i) = rel.split('/').position(|c| c == "node_modules") {
            let dir: Vec<&str> = rel.split('/').take(i + 1).collect();
            push(format!("{}/ (node_modules)", dir.join("/")));
            continue;
        }
        if path.file_name().is_some_and(|n| n == "pyvenv.cfg") {
            let dir = path
                .parent()
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            push(format!(
                "{}/ (python venv)",
                if dir.is_empty() { ".".into() } else { dir }
            ));
            continue;
        }
        if let Ok(m) = std::fs::symlink_metadata(cwd.join(path))
            && m.is_file()
            && m.len() > BIG_FILE
        {
            push(format!("{rel} ({} МБ)", m.len() / (1024 * 1024)));
        }
    }
    // Файлы внутри venv тоже не показываем по отдельности.
    let venvs: Vec<String> = found
        .iter()
        .filter_map(|f| f.strip_suffix(" (python venv)").map(str::to_string))
        .collect();
    found.retain(|f| {
        f.ends_with(" (python venv)") || !venvs.iter().any(|v| f.starts_with(v.as_str()))
    });
    Ok(found)
}

fn read_json(path: &Path) -> Option<RunState> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

impl GitCtx {
    pub fn wip_branch(&self, id: &Uuid) -> String {
        format!("{}{id}", self.wip_prefix)
    }

    /// `{id}.json` у обычной сессии, `{id}@{slug}.json` у каждого репо рабочей папки.
    fn state_path(&self, id: &Uuid, repo: Option<&Path>) -> PathBuf {
        match repo {
            None => self.state_dir.join(format!("{id}.json")),
            Some(r) => {
                let slug: String = r
                    .to_string_lossy()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                self.state_dir.join(format!("{id}@{slug}.json"))
            }
        }
    }

    /// Состояние обычной (одно-репо) сессии.
    pub fn read_state(&self, id: &Uuid) -> Option<RunState> {
        read_json(&self.state_path(id, None))
    }

    /// Есть ли у сессии хоть один не вывезенный запуск.
    pub fn has_state(&self, id: &Uuid) -> bool {
        !self.states_of(id).is_empty()
    }

    /// Все состояния сессии: обычное и по репо рабочей папки.
    pub fn states_of(&self, id: &Uuid) -> Vec<RunState> {
        self.all_states()
            .into_iter()
            .filter(|(oid, _)| oid == id)
            .map(|(_, st)| st)
            .collect()
    }

    /// Все незавершённые запуски (id → состояние); у рабочей папки id повторяется.
    pub fn all_states(&self) -> Vec<(Uuid, RunState)> {
        let Ok(dir) = std::fs::read_dir(&self.state_dir) else {
            return Vec::new();
        };
        let mut out: Vec<(Uuid, RunState)> = dir
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?.strip_suffix(".json")?;
                let id: Uuid = name.split('@').next()?.parse().ok()?;
                Some((id, read_json(&e.path())?))
            })
            .collect();
        out.sort_by(|a, b| a.1.cwd.cmp(&b.1.cwd));
        out
    }

    fn write_state(&self, id: &Uuid, repo: Option<&Path>, st: &RunState) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("не создать {}", self.state_dir.display()))?;
        let path = self.state_path(id, repo);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    fn remove_state(&self, id: &Uuid, repo: Option<&Path>) {
        let _ = std::fs::remove_file(self.state_path(id, repo));
    }

    /// Клонирует `url` в `dest`. Если там уже клон того же репо, ничего не делает.
    pub async fn clone_into(&self, url: &str, dest: &Path) -> Result<String> {
        if dest.join(".git").exists() {
            // Из конфига как есть: `remote get-url` раскрывает insteadOf.
            let key = format!("remote.{}.url", self.remote);
            let have = git(dest, &["config", "--get", &key])
                .await
                .unwrap_or_default();
            if have == url {
                return Ok("репо уже склонирован".into());
            }
            bail!(
                "в {} уже другой репо ({} = {have})",
                dest.display(),
                self.remote
            );
        }
        if dest.exists() && (!dest.is_dir() || std::fs::read_dir(dest)?.next().is_some()) {
            bail!("{} уже есть и это не пустая папка", dest.display());
        }
        let parent = dest.parent().context("у папки нет родителя")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("не создать {}", parent.display()))?;
        let dest_str = dest.to_str().context("путь не в UTF-8")?;
        let out = run_git_t(
            parent,
            &[
                "clone",
                "--quiet",
                "--origin",
                &self.remote,
                "--",
                url,
                dest_str,
            ],
            CLONE_TIMEOUT,
        )
        .await?;
        if !out.ok() {
            let _ = std::fs::remove_dir(dest); // только если git оставил пустую папку
            bail!("git clone не прошёл: {}", out.stderr.trim());
        }
        Ok(format!("склонировано из {url}"))
    }

    /// Перед resume: подтянуть код. Если сессию уже возобновляли на VPS и
    /// её wip-ветка не влита, продолжаем с неё.
    pub async fn prepare(&self, id: &Uuid, cwd: &Path) -> Result<String> {
        self.prepare_at(id, cwd, None).await
    }

    /// Одно репо рабочей папки `workspace`.
    pub async fn prepare_repo(&self, id: &Uuid, repo: &Path, workspace: &Path) -> Result<String> {
        self.prepare_at(id, repo, Some(workspace)).await
    }

    async fn prepare_at(&self, id: &Uuid, cwd: &Path, workspace: Option<&Path>) -> Result<String> {
        let slot = workspace.map(|_| cwd);
        git(cwd, &["rev-parse", "--show-toplevel"])
            .await
            .context("нет git-репозитория в cwd")?;
        if read_json(&self.state_path(id, slot)).is_some() {
            bail!("прошлый запуск этой сессии не завершён: сначала нажми Stop");
        }
        let Some(branch) = current_branch(cwd).await? else {
            bail!("в клоне detached HEAD, разберись руками");
        };
        let wip = self.wip_branch(id);
        if branch.starts_with(&self.wip_prefix) {
            bail!("клон стоит на ветке {branch}, а записи о запуске нет: разберись руками");
        }
        if !git(cwd, &["status", "--porcelain"]).await?.is_empty() {
            bail!("в клоне на VPS есть незакоммиченные изменения, разберись руками");
        }
        git(cwd, &["fetch", &self.remote, "--prune"]).await?;

        let remote_wip = format!("{}/{wip}", self.remote);
        let msg = if ref_exists(cwd, &format!("refs/remotes/{remote_wip}")).await? {
            git(cwd, &["switch", "-C", &wip, &remote_wip]).await?;
            format!("продолжаю с невлитой ветки {wip}")
        } else {
            let upstream = run_git(
                cwd,
                &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
            )
            .await?;
            let target = if upstream.ok() {
                upstream.stdout.trim().to_string()
            } else {
                let guess = format!("{}/{branch}", self.remote);
                if !ref_exists(cwd, &format!("refs/remotes/{guess}")).await? {
                    bail!("у ветки {branch} нет upstream на {}", self.remote);
                }
                guess
            };
            git(cwd, &["merge", "--ff-only", &target])
                .await
                .with_context(|| format!("ветка {branch} разошлась с {target}"))?;
            format!("{branch} обновлена до {target}")
        };

        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.write_state(
            id,
            slot,
            &RunState {
                cwd: cwd.to_path_buf(),
                workspace: workspace.map(Path::to_path_buf),
                base_branch: branch,
                started_at,
                node: self.node.clone(),
            },
        )?;
        Ok(msg)
    }

    async fn default_branch(&self, cwd: &Path) -> Result<String> {
        let head = git(
            cwd,
            &[
                "symbolic-ref",
                "--short",
                &format!("refs/remotes/{}/HEAD", self.remote),
            ],
        )
        .await
        .context("не удалось узнать основную ветку remote")?;
        Ok(head
            .strip_prefix(&format!("{}/", self.remote))
            .unwrap_or(&head)
            .to_string())
    }

    /// После остановки claude: увезти изменения в wip-ветку и вернуть клон
    /// в состояние remote. `reset --hard` только после успешного push.
    pub async fn finalize(&self, id: &Uuid, cwd: &Path, title: &str) -> Result<String> {
        self.finalize_at(id, cwd, title, false).await
    }

    /// Одно репо рабочей папки (state по этому репо может и не быть).
    pub async fn finalize_repo(&self, id: &Uuid, repo: &Path, title: &str) -> Result<String> {
        self.finalize_at(id, repo, title, true).await
    }

    async fn finalize_at(&self, id: &Uuid, cwd: &Path, title: &str, ws: bool) -> Result<String> {
        let slot = ws.then_some(cwd);
        let base = match read_json(&self.state_path(id, slot)) {
            Some(st) => st.base_branch,
            None => match current_branch(cwd).await? {
                Some(b) if !b.starts_with(&self.wip_prefix) => b,
                _ => self.default_branch(cwd).await?,
            },
        };
        let wip = self.wip_branch(id);
        let remote_wip = format!("{}/{wip}", self.remote);
        let remote_base = format!("{}/{base}", self.remote);

        git(cwd, &["fetch", &self.remote, "--prune"]).await?;

        if !git(cwd, &["status", "--porcelain"]).await?.is_empty() {
            let junk = junk_untracked(cwd).await?;
            if !junk.is_empty() {
                let more = junk.len().saturating_sub(JUNK_SHOWN);
                let mut list: Vec<String> = junk.into_iter().take(JUNK_SHOWN).collect();
                if more > 0 {
                    list.push(format!("…и ещё {more}"));
                }
                bail!(
                    "в {} неотслеживаемые файлы, которые нельзя коммитить:\n  {}\n\
                     Добавь их в .gitignore (или .git/info/exclude на VPS) и нажми Stop ещё раз; \
                     клон не тронут",
                    cwd.display(),
                    list.join("\n  ")
                );
            }
            git(cwd, &["add", "-A"]).await?;
            git(
                cwd,
                &[
                    "commit",
                    "--no-verify",
                    "-m",
                    &format!("vps-wip({}): {title}", self.node),
                    "-m",
                    &format!("session: {id}"),
                ],
            )
            .await?;
        }

        let compare = if ref_exists(cwd, &format!("refs/remotes/{remote_wip}")).await? {
            remote_wip.clone()
        } else {
            if !ref_exists(cwd, &format!("refs/remotes/{remote_base}")).await? {
                bail!("нет ветки {remote_base}");
            }
            remote_base.clone()
        };
        let ahead: u32 = git(cwd, &["rev-list", "--count", &format!("{compare}..HEAD")])
            .await?
            .parse()
            .unwrap_or(0);

        let msg = if ahead > 0 {
            git(
                cwd,
                &["push", &self.remote, &format!("HEAD:refs/heads/{wip}")],
            )
            .await
            .context("push не прошёл, клон оставлен как есть")?;
            format!("изменения увезены в {wip} (новых коммитов: {ahead})")
        } else {
            "изменений нет".to_string()
        };

        if current_branch(cwd).await?.as_deref() != Some(base.as_str()) {
            git(cwd, &["switch", &base]).await?;
        }
        git(cwd, &["reset", "--hard", &remote_base]).await?;
        if ref_exists(cwd, &format!("refs/heads/{wip}")).await? {
            git(cwd, &["branch", "-D", &wip]).await?;
        }
        self.remove_state(id, slot);
        Ok(msg)
    }
}
