//! Отметка «доверяю папке» в `~/.claude.json`, чтобы claude не показывал
//! trust-диалог, на который нельзя ответить с телефона.

use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// Возвращает `true`, если файл пришлось поменять.
pub fn ensure_trusted(claude_json: &Path, cwd: &Path) -> Result<bool> {
    let raw = fs::read_to_string(claude_json).with_context(|| {
        format!(
            "нет {}: claude не залогинен на этой машине?",
            claude_json.display()
        )
    })?;
    let mut root: Value =
        serde_json::from_str(&raw).with_context(|| format!("{} не JSON", claude_json.display()))?;
    let Some(obj) = root.as_object_mut() else {
        bail!("{}: ожидается объект", claude_json.display());
    };

    let key = cwd.to_string_lossy().into_owned();
    let projects = obj
        .entry("projects")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(projects) = projects.as_object_mut() else {
        bail!("{}: projects не объект", claude_json.display());
    };
    let project = projects
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(project) = project.as_object_mut() else {
        bail!("{}: запись проекта не объект", claude_json.display());
    };
    if project.get("hasTrustDialogAccepted") == Some(&Value::Bool(true)) {
        return Ok(false);
    }
    project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));

    // Атомарная запись: временный файл рядом, права 600, rename.
    let dir = claude_json.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".claude.json.claude-sync-{}.tmp",
        std::process::id()
    ));
    let mut f = fs::File::create(&tmp)?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(serde_json::to_string_pretty(&root)?.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, claude_json)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_trust_and_keeps_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        fs::write(
            &path,
            r#"{"numStartups":5,"projects":{"/a":{"hasTrustDialogAccepted":true,"x":1},"/b":{"allowedTools":[]}},"zzz":true}"#,
        )
        .unwrap();

        assert!(!ensure_trusted(&path, Path::new("/a")).unwrap());
        assert!(ensure_trusted(&path, Path::new("/b")).unwrap());
        assert!(ensure_trusted(&path, Path::new("/new dir")).unwrap());
        assert!(!ensure_trusted(&path, Path::new("/new dir")).unwrap());

        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 5);
        assert_eq!(v["zzz"], true);
        assert_eq!(v["projects"]["/a"]["x"], 1);
        assert_eq!(v["projects"]["/b"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"]["/b"]["allowedTools"], Value::Array(vec![]));
        assert_eq!(v["projects"]["/new dir"]["hasTrustDialogAccepted"], true);
        // порядок ключей сохраняется
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["numStartups", "projects", "zzz"]);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn missing_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_trusted(&dir.path().join("nope.json"), Path::new("/a")).is_err());
    }
}
