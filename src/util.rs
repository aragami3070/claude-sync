use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Запускает внешнюю команду без stdin, с таймаутом. По таймауту процесс убивается.
pub async fn run(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("не удалось запустить {program}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| {
            format!(
                "{program} {}: таймаут {} с",
                args.join(" "),
                timeout.as_secs()
            )
        })??;
    Ok(Output {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Экранирование для `sh -c`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn find_git_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => "только что".into(),
        60..=3599 => format!("{} мин назад", secs / 60),
        3600..=86399 => format!("{} ч назад", secs / 3600),
        _ => format!("{} дн назад", secs / 86400),
    }
}

/// Путь из формы: `~/…` или абсолютный, без `..`, строго внутри `home`.
pub fn resolve_user_path(input: &str, home: &Path) -> Result<PathBuf> {
    let s = input.trim();
    if s.is_empty() {
        bail!("укажи папку");
    }
    let raw = match s.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(s),
    };
    if !raw.is_absolute() {
        bail!("путь должен начинаться с ~/ или /");
    }
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => bail!("в пути не должно быть .."),
        }
    }
    if out == home || !out.starts_with(home) {
        bail!("папка должна быть внутри {}", home.display());
    }
    Ok(out)
}

/// Последние `n` непустых строк текста.
pub fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn truncate() {
        assert_eq!(truncate_chars("привет  мир", 20), "привет мир");
        assert_eq!(truncate_chars("абвгд", 3), "абв…");
    }

    #[test]
    fn user_paths() {
        let home = Path::new("/home/u");
        let ok = |s: &str| resolve_user_path(s, home).unwrap();
        assert_eq!(ok("~/p/x/"), Path::new("/home/u/p/x"));
        assert_eq!(ok(" /home/u/p/./x "), Path::new("/home/u/p/x"));
        for bad in ["", "~", "~/", "p/x", "/home/u/../v", "~/p/../../v", "/srv/x", "/home/user2/x"] {
            assert!(resolve_user_path(bad, home).is_err(), "{bad}");
        }
    }

    #[test]
    fn tilde_paths() {
        let home = Path::new("/home/u");
        assert_eq!(tilde(Path::new("/home/u/p/x"), home), "~/p/x");
        assert_eq!(tilde(Path::new("/home/u"), home), "~");
        assert_eq!(tilde(Path::new("/srv/x"), home), "/srv/x");
    }
}
