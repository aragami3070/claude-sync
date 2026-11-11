//! Рабочая папка: сама не git-репо, но в дочерних папках есть репо.
//! Какие репо вывозить на Stop, берётся из `permissions.additionalDirectories`
//! в `<папка>/.claude/settings.local.json`.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::util::find_git_root;

/// Насколько глубоко искать репо в дочерних папках.
const DEPTH: usize = 3;
const SKIP: &[&str] = &["node_modules", "target"];

pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join(".claude/settings.local.json")
}

/// git-репо в дочерних папках (в сами репо не спускаемся). Пусто, если их нет.
pub fn child_repos(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, DEPTH, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
            continue;
        }
        // file_type не идёт по симлинкам: не уходим из папки.
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let p = e.path();
        if p.join(".git").exists() {
            out.push(p);
        } else {
            walk(&p, depth - 1, out);
        }
    }
}

/// Рабочая папка: не внутри git-репо, но с репо в дочерних.
pub fn is_workspace(dir: &Path) -> bool {
    dir.is_dir() && find_git_root(dir).is_none() && !child_repos(dir).is_empty()
}

/// Репо из `settings.local.json`.
#[derive(Debug, Default, PartialEq)]
pub struct Listed {
    /// Корни репо, без повторов.
    pub repos: Vec<PathBuf>,
    /// Записи, которые не стали репо: с причиной.
    pub skipped: Vec<String>,
}

/// `None`, если файла нет. Пути: `~/…`, абсолютные или относительно `dir`.
pub fn listed_repos(dir: &Path, home: &Path) -> Result<Option<Listed>> {
    let path = settings_path(dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("не прочитать {}", path.display())),
    };
    let json: Value =
        serde_json::from_str(&raw).with_context(|| format!("{} — не JSON", path.display()))?;
    let entries = json
        .pointer("/permissions/additionalDirectories")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = Listed::default();
    for v in entries {
        let Some(s) = v.as_str() else { continue };
        let raw = match s.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if Path::new(s).is_absolute() => PathBuf::from(s),
            None => dir.join(s),
        };
        let Some(p) = normalize(&raw) else {
            out.skipped.push(format!("{s}: плохой путь"));
            continue;
        };
        if !p.starts_with(home) {
            out.skipped.push(format!("{s}: вне {}", home.display()));
            continue;
        }
        match find_git_root(&p) {
            Some(root) if p.is_dir() => {
                if !out.repos.contains(&root) {
                    out.repos.push(root);
                }
            }
            _ if !p.is_dir() => out.skipped.push(format!("{s}: нет такой папки")),
            _ => out.skipped.push(format!("{s}: не git-репо")),
        }
    }
    Ok(Some(out))
}

/// Убирает `.` и `..` без обращения к диску; `None`, если `..` выходит за корень.
fn normalize(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            Component::Normal(x) => out.push(x),
            Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(p: &Path) {
        std::fs::create_dir_all(p.join(".git")).unwrap();
    }

    #[test]
    fn finds_child_repos() {
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().join("ws");
        repo(&ws.join("a"));
        repo(&ws.join("group/b"));
        repo(&ws.join("a/nested")); // внутрь репо не спускаемся
        repo(&ws.join("node_modules/x"));
        repo(&ws.join(".hidden/y"));
        repo(&ws.join("1/2/3/deep"));
        assert_eq!(child_repos(&ws), [ws.join("a"), ws.join("group/b")]);
        assert!(is_workspace(&ws));
        assert!(!is_workspace(&ws.join("a")));
        // из `1` репо на глубине 3 уже видно
        assert!(is_workspace(&ws.join("1")));
    }

    #[test]
    fn reads_listed_repos() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let ws = home.join("ws");
        repo(&ws.join("a"));
        repo(&home.join("other"));
        std::fs::create_dir_all(ws.join("plain")).unwrap();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        assert_eq!(listed_repos(&ws, home).unwrap(), None);

        std::fs::write(
            settings_path(&ws),
            r#"{"permissions": {"allow": [], "additionalDirectories":
                ["a", "./a/", "a/src/..", "~/other", "plain", "missing", "/etc", "../../.."]}}"#,
        )
        .unwrap();
        let l = listed_repos(&ws, home).unwrap().unwrap();
        assert_eq!(l.repos, [ws.join("a"), home.join("other")]);
        assert_eq!(l.skipped.len(), 4, "{:?}", l.skipped);

        std::fs::write(settings_path(&ws), "{not json").unwrap();
        assert!(listed_repos(&ws, home).is_err());
        std::fs::write(settings_path(&ws), r#"{"permissions": {}}"#).unwrap();
        assert_eq!(listed_repos(&ws, home).unwrap().unwrap(), Listed::default());
    }
}
