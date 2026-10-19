use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};

pub struct Config {
    pub bind: SocketAddr,
    pub auth_user: String,
    pub auth_pass: String,
    pub home: PathBuf,
    pub claude_home: PathBuf,
    pub claude_json: PathBuf,
    pub claude_bin: String,
    pub tmux_socket: String,
    pub git_remote: String,
    pub wip_prefix: String,
    pub node_name: String,
    pub start_timeout: Duration,
    pub stop_timeout: Duration,
    pub state_dir: PathBuf,
    pub list_limit: usize,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn secs(key: &str, default: u64) -> Result<Duration> {
    match var(key) {
        Some(v) => Ok(Duration::from_secs(
            v.parse().with_context(|| format!("{key}: ожидается число секунд"))?,
        )),
        None => Ok(Duration::from_secs(default)),
    }
}

impl Config {
    /// Читает `.env` из текущей папки (если есть) и переменные окружения.
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let home = PathBuf::from(var("HOME").context("не задан HOME")?);
        let auth_user = var("AUTH_USER").unwrap_or_default();
        let auth_pass = var("AUTH_PASS").unwrap_or_default();
        if auth_user.is_empty() || auth_pass.is_empty() {
            bail!("AUTH_USER и AUTH_PASS обязательны");
        }
        let claude_home = var("CLAUDE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let node_name = var("NODE_NAME").unwrap_or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "vps".into())
        });

        Ok(Self {
            bind: var("BIND_ADDR")
                .unwrap_or_else(|| "127.0.0.1:8080".into())
                .parse()
                .context("BIND_ADDR: ожидается ip:port")?,
            auth_user,
            auth_pass,
            claude_json: var("CLAUDE_JSON")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude.json")),
            claude_bin: var("CLAUDE_BIN").unwrap_or_else(|| {
                let local = home.join(".local/bin/claude");
                if local.exists() {
                    local.display().to_string()
                } else {
                    "claude".into()
                }
            }),
            tmux_socket: var("TMUX_SOCKET").unwrap_or_else(|| "claude-sync".into()),
            git_remote: var("GIT_REMOTE").unwrap_or_else(|| "origin".into()),
            wip_prefix: var("WIP_PREFIX").unwrap_or_else(|| "vps-wip/".into()),
            node_name,
            start_timeout: secs("START_TIMEOUT_SECS", 20)?,
            stop_timeout: secs("STOP_TIMEOUT_SECS", 20)?,
            state_dir: var("STATE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/state/claude-sync")),
            list_limit: var("LIST_LIMIT")
                .map(|v| v.parse())
                .transpose()
                .context("LIST_LIMIT: ожидается число")?
                .unwrap_or(50),
            claude_home,
            home,
        })
    }
}
