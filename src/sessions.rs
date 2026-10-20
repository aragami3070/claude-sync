//! Чтение транскриптов `~/.claude/projects/<dir>/<session-id>.jsonl`.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

use serde::Deserialize;
use uuid::Uuid;

use crate::util::{find_git_root, truncate_chars};

const TITLE_MAX: usize = 80;

/// То, что удалось вытащить из одного jsonl.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub custom_title: Option<String>,
    pub ai_title: Option<String>,
    pub last_prompt: Option<String>,
    pub first_user_text: Option<String>,
    pub user_count: usize,
    pub bridge_id: Option<String>,
    pub continued_in: Option<Uuid>,
}

impl Parsed {
    pub fn title(&self) -> String {
        [
            &self.custom_title,
            &self.ai_title,
            &self.last_prompt,
            &self.first_user_text,
        ]
        .into_iter()
        .flatten()
        .map(|s| truncate_chars(s, TITLE_MAX))
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "(без названия)".into())
    }
}

/// Только нужные поля записи; остальное serde пропускает.
#[derive(Deserialize)]
struct Rec {
    #[serde(rename = "type", default)]
    ty: String,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "agentName")]
    agent_name: Option<String>,
    #[serde(rename = "lastPrompt")]
    last_prompt: Option<String>,
    #[serde(rename = "bridgeSessionId")]
    bridge_session_id: Option<String>,
    #[serde(rename = "continuedInSessionId")]
    continued_in: Option<String>,
}

#[derive(Deserialize)]
struct UserLine {
    message: Option<UserMessage>,
}

#[derive(Deserialize)]
struct UserMessage {
    content: Option<serde_json::Value>,
}

/// Текст пользовательского сообщения без служебных вставок claude.
fn user_text(line: &[u8]) -> Option<String> {
    let content = serde_json::from_slice::<UserLine>(line)
        .ok()?
        .message?
        .content?;
    let text = match content {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(items) => items
            .iter()
            .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') || text.starts_with("Caveat:") {
        return None;
    }
    Some(text.to_string())
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

impl Parsed {
    fn feed(&mut self, line: &[u8]) {
        let Ok(rec) = serde_json::from_slice::<Rec>(line) else {
            return; // битая или недописанная строка
        };
        match rec.ty.as_str() {
            "user" | "assistant" => {
                if rec.is_sidechain == Some(true) {
                    return;
                }
                if let Some(cwd) = non_empty(rec.cwd) {
                    self.cwd = Some(PathBuf::from(cwd));
                }
                if let Some(b) = non_empty(rec.git_branch) {
                    self.git_branch = Some(b);
                }
                if rec.ty == "user" {
                    self.user_count += 1;
                    if self.first_user_text.is_none() && rec.is_meta != Some(true) {
                        self.first_user_text = user_text(line);
                    }
                }
            }
            "ai-title" => {
                if let Some(t) = non_empty(rec.ai_title) {
                    self.ai_title = Some(t);
                }
            }
            "custom-title" | "agent-name" => {
                if let Some(t) = non_empty(rec.custom_title).or(non_empty(rec.agent_name)) {
                    self.custom_title = Some(t);
                }
            }
            "last-prompt" => {
                if let Some(t) = non_empty(rec.last_prompt) {
                    self.last_prompt = Some(t);
                }
            }
            "bridge-session" => {
                if let Some(id) = non_empty(rec.bridge_session_id) {
                    self.bridge_id = Some(id);
                }
            }
            "continued-in" => {
                if let Some(id) = rec.continued_in.and_then(|s| s.parse().ok()) {
                    self.continued_in = Some(id);
                }
            }
            _ => {}
        }
    }

    pub fn from_reader(mut r: impl BufRead) -> Self {
        let mut p = Parsed::default();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => p.feed(&buf),
            }
        }
        p
    }

    pub fn from_file(path: &Path) -> std::io::Result<Self> {
        Ok(Self::from_reader(BufReader::new(File::open(path)?)))
    }
}

/// Ищет запись `bridge-session`, дописанную после `offset`. Нужна при старте:
/// так видно, что claude зарегистрировал Remote Control.
pub fn bridge_after(path: &Path, offset: u64) -> Option<String> {
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    Parsed::from_reader(BufReader::new(f)).bridge_id
}

pub fn rc_url(bridge_id: &str) -> Option<String> {
    bridge_id
        .strip_prefix("cse_")
        .filter(|s| !s.is_empty())
        .map(|s| format!("https://claude.ai/code/session_{s}"))
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: Uuid,
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub parsed: Parsed,
    pub has_conflicts: bool,
}

impl Session {
    pub fn title(&self) -> String {
        self.parsed.title()
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.parsed.cwd.as_deref()
    }

    pub fn git_root(&self) -> Option<PathBuf> {
        self.cwd().filter(|c| c.is_dir()).and_then(find_git_root)
    }

    pub fn rc_url(&self) -> Option<String> {
        self.parsed.bridge_id.as_deref().and_then(rc_url)
    }
}

struct Cached {
    mtime: SystemTime,
    len: u64,
    parsed: Parsed,
}

/// Сканер с кэшем: перечитываются только изменившиеся файлы.
pub struct Scanner {
    projects: PathBuf,
    cache: Mutex<HashMap<PathBuf, Cached>>,
}

const CONFLICT_MARK: &str = ".sync-conflict-";

impl Scanner {
    pub fn new(claude_home: &Path) -> Self {
        Self {
            projects: claude_home.join("projects"),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Все сессии, где есть хотя бы одно сообщение пользователя, свежие первыми.
    pub fn scan(&self) -> Vec<Session> {
        let mut files: Vec<(Uuid, PathBuf)> = Vec::new();
        let mut conflicts: HashSet<Uuid> = HashSet::new();

        let dirs = std::fs::read_dir(&self.projects).into_iter().flatten().flatten();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let Some(stem) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                if let Some(pos) = stem.find(CONFLICT_MARK) {
                    if let Ok(id) = stem[..pos].parse() {
                        conflicts.insert(id);
                    }
                    continue;
                }
                if let Ok(id) = stem.parse() {
                    files.push((id, entry.path()));
                }
            }
        }

        let mut cache = self.cache.lock().unwrap();
        let mut seen = HashSet::new();
        let mut by_id: HashMap<Uuid, Session> = HashMap::new();
        for (id, path) in files {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let len = meta.len();
            seen.insert(path.clone());
            let fresh = matches!(cache.get(&path), Some(c) if c.mtime == mtime && c.len == len);
            if !fresh {
                let Ok(parsed) = Parsed::from_file(&path) else {
                    continue;
                };
                cache.insert(path.clone(), Cached { mtime, len, parsed });
            }
            let parsed = cache[&path].parsed.clone();
            if parsed.user_count == 0 {
                continue;
            }
            let s = Session {
                id,
                path,
                mtime,
                parsed,
                has_conflicts: conflicts.contains(&id),
            };
            // Один id в двух папках проектов: берём более свежий файл.
            match by_id.get(&id) {
                Some(old) if old.mtime >= s.mtime => {}
                _ => {
                    by_id.insert(id, s);
                }
            }
        }
        cache.retain(|p, _| seen.contains(p));

        let mut out: Vec<Session> = by_id.into_values().collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        out
    }

    pub fn get(&self, id: Uuid) -> Option<Session> {
        self.scan().into_iter().find(|s| s.id == id)
    }

    /// Файл транскрипта по id, даже без сообщений (новая сессия до первого промпта).
    pub fn find_file(&self, id: Uuid) -> Option<PathBuf> {
        let name = format!("{id}.jsonl");
        std::fs::read_dir(&self.projects)
            .ok()?
            .flatten()
            .map(|d| d.path().join(&name))
            .filter(|p| p.is_file())
            .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
    }

    /// Ссылка RC из транскрипта, даже без сообщений.
    pub fn rc_url_any(&self, id: Uuid) -> Option<String> {
        let parsed = Parsed::from_file(&self.find_file(id)?).ok()?;
        parsed.bridge_id.as_deref().and_then(rc_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"type":"bridge-session","sessionId":"x","bridgeSessionId":"","lastSequenceNum":0}
{"type":"user","cwd":"/home/u/p","gitBranch":"master","isSidechain":false,"isMeta":true,"message":{"role":"user","content":"<local-command-caveat>Caveat</local-command-caveat>"}}
{"type":"user","cwd":"/home/u/p","gitBranch":"master","isSidechain":false,"message":{"role":"user","content":"<command-name>/model</command-name>"}}
{"type":"user","cwd":"/home/u/p","gitBranch":"master","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"почитай   md файл"}]}}
{"type":"assistant","cwd":"/home/u/p","gitBranch":"feat","isSidechain":false,"message":{"content":[{"type":"text","text":"ok"}]}}
{"type":"user","cwd":"/home/u/other","isSidechain":true,"message":{"content":"sub"}}
{"type":"ai-title","aiTitle":"Первое","sessionId":"x"}
{"type":"last-prompt","lastPrompt":"последний","sessionId":"x"}
{"type":"bridge-session","sessionId":"x","bridgeSessionId":"cse_01ABC","lastSequenceNum":3}
{"type":"ai-title","aiTitle":"Второе","sessionId":"x"}
{"type":"user","cwd":"/home/u/p","mess
"#;

    #[test]
    fn parses_sample() {
        let p = Parsed::from_reader(SAMPLE.as_bytes());
        assert_eq!(p.cwd.as_deref(), Some(Path::new("/home/u/p")));
        assert_eq!(p.git_branch.as_deref(), Some("feat"));
        assert_eq!(p.user_count, 3);
        assert_eq!(p.first_user_text.as_deref(), Some("почитай   md файл"));
        assert_eq!(p.ai_title.as_deref(), Some("Второе"));
        assert_eq!(p.bridge_id.as_deref(), Some("cse_01ABC"));
        assert_eq!(p.title(), "Второе");
    }

    #[test]
    fn title_priority() {
        let mut p = Parsed {
            first_user_text: Some("первый   текст".into()),
            ..Default::default()
        };
        assert_eq!(p.title(), "первый текст");
        p.last_prompt = Some("prompt".into());
        assert_eq!(p.title(), "prompt");
        p.ai_title = Some("ai".into());
        assert_eq!(p.title(), "ai");
        p.feed(br#"{"type":"agent-name","agentName":"my-name"}"#);
        assert_eq!(p.title(), "my-name");
        assert_eq!(Parsed::default().title(), "(без названия)");
    }

    #[test]
    fn continued_in() {
        let p = Parsed::from_reader(
            &br#"{"type":"continued-in","continuedInSessionId":"a70ce33e-164a-455c-bbee-6782e9b458d1"}"#[..],
        );
        assert!(p.continued_in.is_some());
    }

    #[test]
    fn rc_url_from_bridge() {
        assert_eq!(
            rc_url("cse_0186QtNpbX95mWBYAvhiPETc").as_deref(),
            Some("https://claude.ai/code/session_0186QtNpbX95mWBYAvhiPETc")
        );
        assert_eq!(rc_url(""), None);
        assert_eq!(rc_url("cse_"), None);
    }

    #[test]
    fn scanner_filters_and_flags() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("projects/-home-u-p");
        std::fs::create_dir_all(&dir).unwrap();
        let a = "09df6a91-6bb3-49d7-a350-17a48b802364";
        let stub = "33f922ea-9d5f-41f1-8eb5-60c7f80afe9c";
        std::fs::write(dir.join(format!("{a}.jsonl")), SAMPLE).unwrap();
        std::fs::write(
            dir.join(format!("{a}.sync-conflict-20261019-143304-XHJSKQ5.jsonl")),
            SAMPLE,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{stub}.jsonl")),
            r#"{"type":"bridge-session","bridgeSessionId":"cse_1"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("not-a-uuid.jsonl"), SAMPLE).unwrap();
        std::fs::create_dir_all(dir.join(a).join("subagents")).unwrap();

        let sc = Scanner::new(home.path());
        let list = sc.scan();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id.to_string(), a);
        assert!(list[0].has_conflicts);
        // второй проход идёт из кэша и даёт то же самое
        assert_eq!(sc.scan()[0].parsed, list[0].parsed);

        // заглушку без сообщений в списке не видно, но по id она находится
        let stub_id: Uuid = stub.parse().unwrap();
        assert!(sc.get(stub_id).is_none());
        assert_eq!(sc.find_file(stub_id), Some(dir.join(format!("{stub}.jsonl"))));
        assert_eq!(
            sc.rc_url_any(stub_id).as_deref(),
            Some("https://claude.ai/code/session_1")
        );
        assert!(sc.find_file(Uuid::from_u128(1)).is_none());
    }

    #[test]
    fn bridge_after_offset() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), SAMPLE).unwrap();
        let len = std::fs::metadata(f.path()).unwrap().len();
        assert_eq!(bridge_after(f.path(), len), None);
        let mut s = SAMPLE.to_string();
        s.push_str("\n{\"type\":\"bridge-session\",\"bridgeSessionId\":\"cse_NEW\"}\n");
        std::fs::write(f.path(), s).unwrap();
        assert_eq!(bridge_after(f.path(), len).as_deref(), Some("cse_NEW"));
    }
}
