//! Разбор sync-conflict копий транскрипта: какие сообщения есть только в основном
//! файле, какие только в копии. Сравнение по `uuid` записей, порядок как в файле.

use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::util::truncate_chars;

const TEXT_MAX: usize = 400;

/// Сообщение, видимое человеку: промпт, текст claude или вызов инструмента.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub uuid: String,
    pub from_user: bool,
    /// `2026-10-19 15:19` из ISO-времени записи.
    pub time: String,
    pub text: String,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    ty: String,
    uuid: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    message: Option<Body>,
}

#[derive(Deserialize)]
struct Body {
    content: Option<Value>,
}

/// Текст сообщения; `None` — служебное (thinking, результат инструмента, вставки claude).
fn visible_text(from_user: bool, content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                    Some("text") => i.get("text").and_then(Value::as_str).map(str::to_string),
                    Some("tool_use") if !from_user => Some(format!(
                        "🔧 {}",
                        i.get("name").and_then(Value::as_str).unwrap_or("?")
                    )),
                    _ => None,
                })
                .collect();
            parts.join(" ")
        }
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() || (from_user && (text.starts_with('<') || text.starts_with("Caveat:"))) {
        return None;
    }
    Some(truncate_chars(text, TEXT_MAX))
}

fn short_time(ts: Option<&str>) -> String {
    // 2026-10-19T15:19:21.123Z → 2026-10-19 15:19
    ts.map(|t| t.get(..16).unwrap_or(t).replace('T', " "))
        .unwrap_or_default()
}

pub fn messages_from(r: impl BufRead) -> Vec<Msg> {
    let mut out = Vec::new();
    for line in r.split(b'\n').map_while(Result::ok) {
        let Ok(l) = serde_json::from_slice::<Line>(&line) else {
            continue;
        };
        let from_user = match l.ty.as_str() {
            "user" => true,
            "assistant" => false,
            _ => continue,
        };
        if l.is_sidechain == Some(true) || l.is_meta == Some(true) {
            continue;
        }
        let (Some(uuid), Some(content)) = (l.uuid, l.message.and_then(|m| m.content)) else {
            continue;
        };
        let Some(text) = visible_text(from_user, &content) else {
            continue;
        };
        out.push(Msg {
            uuid,
            from_user,
            time: short_time(l.timestamp.as_deref()),
            text,
        });
    }
    out
}

pub fn messages(path: &Path) -> Result<Vec<Msg>> {
    let f = File::open(path).with_context(|| format!("не открыть {}", path.display()))?;
    Ok(messages_from(BufReader::new(f)))
}

/// Сравнение основного транскрипта с одной конфликтной копией.
#[derive(Debug, Clone)]
pub struct Diff {
    pub file_name: String,
    /// Когда и с какого устройства Syncthing сохранил копию: из имени файла.
    pub label: String,
    pub mtime: SystemTime,
    pub common: usize,
    pub only_main: Vec<Msg>,
    pub only_copy: Vec<Msg>,
}

/// `<id>.sync-conflict-20261019-151921-OHXWZ6E.jsonl` → `2026-10-19 15:19:21, устройство OHXWZ6E`.
pub fn conflict_label(file_name: &str) -> String {
    let parsed = file_name
        .split_once(".sync-conflict-")
        .and_then(|(_, rest)| rest.strip_suffix(".jsonl"))
        .and_then(|rest| {
            let mut it = rest.splitn(3, '-');
            let (date, time, dev) = (it.next()?, it.next()?, it.next()?);
            (date.len() == 8 && time.len() == 6 && date.is_ascii() && time.is_ascii()).then(|| {
                format!(
                    "{}-{}-{} {}:{}:{}, устройство {dev}",
                    &date[..4],
                    &date[4..6],
                    &date[6..],
                    &time[..2],
                    &time[2..4],
                    &time[4..]
                )
            })
        });
    parsed.unwrap_or_else(|| file_name.to_string())
}

pub fn diff_messages(main: &[Msg], copy: &[Msg]) -> (usize, Vec<Msg>, Vec<Msg>) {
    let in_main: HashSet<&str> = main.iter().map(|m| m.uuid.as_str()).collect();
    let in_copy: HashSet<&str> = copy.iter().map(|m| m.uuid.as_str()).collect();
    let common = main
        .iter()
        .filter(|m| in_copy.contains(m.uuid.as_str()))
        .count();
    let only_main = main
        .iter()
        .filter(|m| !in_copy.contains(m.uuid.as_str()))
        .cloned()
        .collect();
    let only_copy = copy
        .iter()
        .filter(|m| !in_main.contains(m.uuid.as_str()))
        .cloned()
        .collect();
    (common, only_main, only_copy)
}

/// Diff основного файла с каждой копией.
pub fn diff_all(main: &Path, copies: &[PathBuf]) -> Result<Vec<Diff>> {
    let main_msgs = messages(main)?;
    copies
        .iter()
        .map(|copy| -> Result<Diff> {
            let copy_msgs = messages(copy)?;
            let (common, only_main, only_copy) = diff_messages(&main_msgs, &copy_msgs);
            let file_name = copy
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok(Diff {
                label: conflict_label(&file_name),
                file_name,
                mtime: std::fs::metadata(copy)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                common,
                only_main,
                only_copy,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-10-19T15:19:21.000Z","message":{"content":"привет"}}
{"type":"user","uuid":"m1","isMeta":true,"message":{"content":"meta"}}
{"type":"assistant","uuid":"a0","message":{"content":[{"type":"thinking","thinking":"..."}]}}
{"type":"assistant","uuid":"a1","timestamp":"2026-10-19T15:19:30.000Z","message":{"content":[{"type":"text","text":"ответ"}]}}
{"type":"assistant","uuid":"a2","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}
{"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","content":"ok"}]}}
{"type":"user","uuid":"c1","message":{"content":"<command-name>/model</command-name>"}}
{"type":"user","uuid":"s1","isSidechain":true,"message":{"content":"sub"}}
{"type":"bridge-session","bridgeSessionId":"cse_1"}
{"type":"user","uuid":"u2","message":{"content":[{"type":"text","text":"только в основном"}]}}
"#;

    const COPY: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-10-19T15:19:21.000Z","message":{"content":"привет"}}
{"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"ответ"}]}}
{"type":"user","uuid":"u3","message":{"content":"только в копии"}}
"#;

    #[test]
    fn visible_messages_only() {
        let m = messages_from(MAIN.as_bytes());
        let texts: Vec<&str> = m.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["привет", "ответ", "🔧 Bash", "только в основном"]);
        assert!(m[0].from_user && !m[1].from_user);
        assert_eq!(m[0].time, "2026-10-19 15:19");
        assert_eq!(m[2].time, "");
    }

    #[test]
    fn diff_by_uuid() {
        let main = messages_from(MAIN.as_bytes());
        let copy = messages_from(COPY.as_bytes());
        let (common, only_main, only_copy) = diff_messages(&main, &copy);
        assert_eq!(common, 2);
        let ids = |v: &[Msg]| v.iter().map(|m| m.uuid.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&only_main), ["a2", "u2"]);
        assert_eq!(ids(&only_copy), ["u3"]);
    }

    #[test]
    fn labels() {
        assert_eq!(
            conflict_label("e2aed103.sync-conflict-20261019-151921-OHXWZ6E.jsonl"),
            "2026-10-19 15:19:21, устройство OHXWZ6E"
        );
        assert_eq!(conflict_label("weird.jsonl"), "weird.jsonl");
    }
}
