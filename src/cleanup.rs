use std::{path::Path, time::Duration};

use anyhow::{Result, bail};

use crate::util;

/// `cargo clean`, если в корне репо есть `Cargo.toml` и `target`. Возвращает сводку cargo.
pub async fn cargo_clean(root: &Path) -> Result<Option<String>> {
    if !root.join("Cargo.toml").exists() || !root.join("target").exists() {
        return Ok(None);
    }
    let out = util::run(
        "cargo",
        &["clean"],
        Some(root),
        &[],
        Duration::from_secs(300),
    )
    .await?;
    if !out.ok() {
        bail!("cargo clean: {}", out.stderr.trim());
    }
    let summary = util::tail_lines(&out.stderr, 1);
    Ok(Some(if summary.is_empty() {
        "cargo clean".into()
    } else {
        format!("cargo clean: {}", summary.trim())
    }))
}
