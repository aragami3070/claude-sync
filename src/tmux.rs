//! Обёртка над tmux. Все вызовы идут в отдельный сервер (`tmux -L <socket>`),
//! чтобы не смешиваться с личным tmux.

use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Result, bail};
use uuid::Uuid;

use crate::util::{self, Output};

const PREFIX: &str = "cs-";
const TIMEOUT: Duration = Duration::from_secs(10);

pub fn session_name(id: &Uuid) -> String {
    format!("{PREFIX}{id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pane {
    /// claude завершился, окно осталось из-за `remain-on-exit`
    pub dead: bool,
    pub exit_code: Option<i32>,
}

pub struct Tmux {
    socket: String,
}

impl Tmux {
    pub fn new(socket: impl Into<String>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    async fn cmd(&self, args: &[&str]) -> Result<Output> {
        // -u: UTF-8 даже без локали (systemd); -f /dev/null: без личного tmux.conf
        let mut full = vec!["-u", "-L", self.socket.as_str(), "-f", "/dev/null"];
        full.extend_from_slice(args);
        util::run("tmux", &full, None, &[], TIMEOUT).await
    }

    async fn cmd_ok(&self, args: &[&str]) -> Result<String> {
        let out = self.cmd(args).await?;
        if !out.ok() {
            bail!("tmux {}: {}", args.join(" "), out.stderr.trim());
        }
        Ok(out.stdout)
    }

    /// Запущенные сессии морды. Нет сервера = пустой список.
    pub async fn list(&self) -> Result<HashMap<Uuid, Pane>> {
        let out = self
            .cmd(&[
                "list-panes",
                "-a",
                "-F",
                // не табуляция: без UTF-8 локали (systemd) tmux заменяет её на `_`
                "#{session_name}|#{pane_dead}|#{pane_dead_status}",
            ])
            .await?;
        if !out.ok() {
            let err = out.stderr.to_lowercase();
            if err.contains("no server running")
                || err.contains("error connecting")
                || err.contains("no such file")
            {
                return Ok(HashMap::new());
            }
            bail!("tmux list-panes: {}", out.stderr.trim());
        }
        Ok(parse_list(&out.stdout))
    }

    /// Запускает `command` (строка для `sh -c`) в новой сессии и включает
    /// `remain-on-exit`, чтобы при падении claude остался его вывод.
    pub async fn start(&self, id: &Uuid, cwd: &Path, command: &str) -> Result<()> {
        let name = session_name(id);
        let cwd = cwd.to_string_lossy();
        self.cmd_ok(&[
            "new-session",
            "-d",
            "-s",
            &name,
            "-c",
            &cwd,
            "-x",
            "200",
            "-y",
            "50",
            command,
            ";",
            "set-option",
            "-w",
            "-t",
            &name,
            "remain-on-exit",
            "on",
        ])
        .await?;
        Ok(())
    }

    /// Экран окна. У мёртвого окна вывод уезжает в историю, поэтому для него
    /// берём и последние строки истории.
    pub async fn capture(&self, id: &Uuid, with_history: bool) -> Result<String> {
        let name = session_name(id);
        let mut args = vec!["capture-pane", "-p", "-t", name.as_str()];
        if with_history {
            args.extend(["-S", "-200"]);
        }
        let out = self.cmd_ok(&args).await?;
        Ok(out.trim_end().to_string())
    }

    /// Мягкая остановка: Escape прерывает текущий ответ, затем `/exit`.
    pub async fn send_exit(&self, id: &Uuid) -> Result<()> {
        let name = session_name(id);
        self.cmd_ok(&["send-keys", "-t", &name, "Escape"]).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.cmd_ok(&["send-keys", "-t", &name, "-l", "/exit"])
            .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.cmd_ok(&["send-keys", "-t", &name, "Enter"]).await?;
        Ok(())
    }

    pub async fn kill(&self, id: &Uuid) -> Result<()> {
        self.cmd_ok(&["kill-session", "-t", &session_name(id)])
            .await?;
        Ok(())
    }
}

fn parse_list(stdout: &str) -> HashMap<Uuid, Pane> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('|');
            let id = parts.next()?.strip_prefix(PREFIX)?.parse().ok()?;
            let dead = parts.next()? == "1";
            let exit_code = parts.next().and_then(|s| s.parse().ok());
            Some((id, Pane { dead, exit_code }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let id = "09df6a91-6bb3-49d7-a350-17a48b802364";
        let out = format!("cs-{id}|0|\nother|0|\ncs-{id}x|1|3\n");
        let map = parse_list(&out);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map[&id.parse::<Uuid>().unwrap()],
            Pane {
                dead: false,
                exit_code: None
            }
        );
        let map = parse_list(&format!("cs-{id}|1|2\n"));
        assert_eq!(map.values().next().unwrap().exit_code, Some(2));
    }

    /// Живой tmux: старт, вывод, падение с remain-on-exit, kill.
    #[tokio::test]
    async fn live_roundtrip() {
        if which("tmux").is_none() {
            return;
        }
        let tmux = Tmux::new(format!("claude-sync-test-{}", std::process::id()));
        let id = Uuid::from_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(tmux.list().await.unwrap().is_empty());

        tmux.start(&id, dir.path(), "echo hello-from-pane; sleep 1; exit 3")
            .await
            .unwrap();
        let pane = tmux.list().await.unwrap()[&id];
        assert!(!pane.dead);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let pane = tmux.list().await.unwrap()[&id];
        assert!(pane.dead);
        assert_eq!(pane.exit_code, Some(3));
        assert!(
            tmux.capture(&id, true)
                .await
                .unwrap()
                .contains("hello-from-pane")
        );
        tmux.kill(&id).await.unwrap();
        assert!(tmux.list().await.unwrap().is_empty());
    }

    fn which(bin: &str) -> Option<std::path::PathBuf> {
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(bin))
                .find(|p| p.exists())
        })
    }
}
