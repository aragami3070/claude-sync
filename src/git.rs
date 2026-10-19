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

/// Что запомнили при resume; нужно при stop, переживает рестарт морды.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunState {
    pub cwd: PathBuf,
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
    let mut envs = vec![("GIT_TERMINAL_PROMPT", "0")];
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        envs.push(("GIT_SSH_COMMAND", "ssh -o BatchMode=yes"));
    }
    util::run("git", args, Some(cwd), &envs, TIMEOUT).await
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

impl GitCtx {
    pub fn wip_branch(&self, id: &Uuid) -> String {
        format!("{}{id}", self.wip_prefix)
    }

    fn state_path(&self, id: &Uuid) -> PathBuf {
        self.state_dir.join(format!("{id}.json"))
    }

    pub fn read_state(&self, id: &Uuid) -> Option<RunState> {
        let raw = std::fs::read_to_string(self.state_path(id)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Все незавершённые запуски (id → состояние).
    pub fn all_states(&self) -> Vec<(Uuid, RunState)> {
        let Ok(dir) = std::fs::read_dir(&self.state_dir) else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let id: Uuid = name.to_str()?.strip_suffix(".json")?.parse().ok()?;
                Some((id, self.read_state(&id)?))
            })
            .collect()
    }

    fn write_state(&self, id: &Uuid, st: &RunState) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("не создать {}", self.state_dir.display()))?;
        let tmp = self.state_dir.join(format!(".{id}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
        std::fs::rename(tmp, self.state_path(id))?;
        Ok(())
    }

    fn remove_state(&self, id: &Uuid) {
        let _ = std::fs::remove_file(self.state_path(id));
    }

    /// Перед resume: подтянуть код. Если сессию уже возобновляли на VPS и
    /// её wip-ветка не влита, продолжаем с неё.
    pub async fn prepare(&self, id: &Uuid, cwd: &Path) -> Result<String> {
        git(cwd, &["rev-parse", "--show-toplevel"])
            .await
            .context("нет git-репозитория в cwd")?;
        if self.read_state(id).is_some() {
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
            &RunState {
                cwd: cwd.to_path_buf(),
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
            &["symbolic-ref", "--short", &format!("refs/remotes/{}/HEAD", self.remote)],
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
        let base = match self.read_state(id) {
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
        self.remove_state(id);
        Ok(msg)
    }
}
