use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use stepper_provider::Message;

/// One completed turn: the user's input, each layer's free-text outcome (the
/// human-readable digest for pickers), and the full normalized message
/// transcript (assistant blocks, tool calls/results) for full-fidelity resume.
/// `messages` defaults empty so session files saved before it existed still load.
/// The trailing `usage`/`cost_usd`/`model_ref`/`ended_at` fields feed cross-session
/// `stepper stats`; all default so older session files keep loading (they simply
/// contribute zeros and no timestamp).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnRecord {
    pub user: String,
    pub summaries: Vec<(String, String)>,
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Token accounting for this turn (summed across its layers/steps).
    #[serde(default)]
    pub usage: stepper_provider::Usage,
    /// USD cost of this turn (0 for local/keyless models).
    #[serde(default)]
    pub cost_usd: f64,
    /// The turn's primary model ref (`provider/model-id`); empty for old files.
    #[serde(default)]
    pub model_ref: String,
    /// Turn-completion time (unix epoch seconds); `None` for old files.
    #[serde(default)]
    pub ended_at: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    /// Optional human-given name (`--name`), shown by session pickers.
    #[serde(default)]
    pub name: Option<String>,
    pub turns: Vec<TurnRecord>,
}

impl SessionRecord {
    /// A new, empty session with a random id.
    pub fn fresh() -> Self {
        SessionRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: None,
            turns: Vec::new(),
        }
    }

    /// A branch of this session under a fresh id, with its turns preserved — for
    /// `--fork`, which continues a resumed session without touching the original.
    /// The name (if any) gets a `(fork)` marker so the two stay distinguishable.
    pub fn forked(&self) -> Self {
        SessionRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: self.name.as_ref().map(|n| format!("{n} (fork)")),
            turns: self.turns.clone(),
        }
    }

    /// Whether any turn carries a real message transcript. Old-format files
    /// (digest only) return false and resume falls back to `resume_context`.
    pub fn has_messages(&self) -> bool {
        self.turns.iter().any(|t| !t.messages.is_empty())
    }

    /// The real prior messages to seed a resumed run. A turn saved by an older
    /// version (no transcript) contributes a synthesized user/assistant pair
    /// built from its digest, so mixed-format sessions stay coherent.
    pub fn seed_messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        for turn in &self.turns {
            if turn.messages.is_empty() {
                out.push(Message::user(turn.user.clone()));
                let digest = turn
                    .summaries
                    .iter()
                    .map(|(layer, summary)| format!("[{layer}] {summary}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !digest.is_empty() {
                    out.push(Message::assistant(digest));
                }
            } else {
                out.extend(turn.messages.iter().cloned());
            }
        }
        out
    }

    /// Render prior turns as resume context to seed a continued session.
    pub fn resume_context(&self) -> String {
        if self.turns.is_empty() {
            return String::new();
        }
        let mut text = String::from("# Earlier in this session\n\n");
        for (i, turn) in self.turns.iter().enumerate() {
            text.push_str(&format!("## Turn {}\n\nRequest: {}\n\n", i + 1, turn.user));
            for (layer, summary) in &turn.summaries {
                text.push_str(&format!("- {layer}: {summary}\n"));
            }
            text.push('\n');
        }
        text
    }
}

/// Persists sessions to `.stepper/sessions/<id>.json`.
#[derive(Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(project_root: &std::path::Path) -> Self {
        SessionStore {
            dir: project_root.join(".stepper").join("sessions"),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn load(&self, id: &str) -> Option<SessionRecord> {
        let raw = std::fs::read_to_string(self.path(id)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// The most recently saved session for this project (by file mtime), for
    /// `--continue`/`-c`. `None` when no session has been saved yet.
    pub fn latest(&self) -> Option<SessionRecord> {
        self.list_recent(1).into_iter().next().map(|(record, _)| record)
    }

    /// Up to `limit` persisted sessions with their file mtimes, newest first
    /// (for the `/resume` picker). Unreadable or unparseable files are skipped.
    pub fn list_recent(&self, limit: usize) -> Vec<(SessionRecord, std::time::SystemTime)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let Some(record) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<SessionRecord>(&raw).ok())
            else {
                continue;
            };
            sessions.push((record, modified));
        }
        sessions.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        sessions.truncate(limit);
        sessions
    }

    /// Delete a session file. Returns whether it existed — a missing file is not
    /// an error so `session delete <id>` can report "not found" itself.
    pub fn delete(&self, id: &str) -> Result<bool, CoreError> {
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(CoreError::Session(e.to_string())),
        }
    }

    pub fn save(&self, record: &SessionRecord) -> Result<(), CoreError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| CoreError::Session(e.to_string()))?;
        let json = serde_json::to_string_pretty(record)
            .map_err(|e| CoreError::Session(e.to_string()))?;
        // Atomic: write to a temp file then rename, so a crash mid-write can't
        // corrupt an existing session.
        let final_path = self.path(&record.id);
        let tmp_path = self.dir.join(format!("{}.json.tmp", record.id));
        std::fs::write(&tmp_path, json).map_err(|e| CoreError::Session(e.to_string()))?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| CoreError::Session(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stepper_provider::{ContentBlock, Role, ToolContent};

    #[test]
    fn save_and_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut record = SessionRecord {
            id: "abc".into(),
            name: Some("my session".into()),
            turns: vec![],
        };
        record.turns.push(TurnRecord {
            user: "do a thing".into(),
            summaries: vec![("plan".into(), "planned it".into())],
            messages: vec![
                Message::user("do a thing"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "list_dir".into(),
                        input: serde_json::json!({ "path": "." }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: "c1".into(),
                        content: vec![ToolContent::text("src/")],
                        is_error: false,
                    }],
                },
                Message::assistant("planned it"),
            ],
            ..Default::default()
        });
        store.save(&record).unwrap();

        let loaded = store.load("abc").unwrap();
        assert_eq!(loaded.name.as_deref(), Some("my session"));
        assert_eq!(loaded.turns.len(), 1);
        assert_eq!(loaded.turns[0].user, "do a thing");
        assert!(loaded.resume_context().contains("planned it"));
        assert_eq!(loaded.turns[0].messages.len(), 4);
        assert!(loaded.has_messages());
        let seed = loaded.seed_messages();
        assert_eq!(seed.len(), 4);
        assert!(matches!(
            &seed[1].content[0],
            ContentBlock::ToolUse { name, .. } if name == "list_dir"
        ));
        assert!(matches!(
            &seed[2].content[0],
            ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == "c1"
        ));
    }

    #[test]
    fn old_format_file_without_messages_or_name_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".stepper").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("old.json"),
            r#"{ "id": "old", "turns": [ { "user": "add a flag", "summaries": [["plan", "decided on --verbose"]] } ] }"#,
        )
        .unwrap();

        let loaded = SessionStore::new(dir.path()).load("old").unwrap();
        assert_eq!(loaded.id, "old");
        assert_eq!(loaded.name, None);
        assert_eq!(loaded.turns.len(), 1);
        assert!(loaded.turns[0].messages.is_empty());
        assert!(!loaded.has_messages(), "old files fall back to the digest path");
        assert!(loaded.resume_context().contains("decided on --verbose"));
        // The stats fields (added later) default cleanly on an old file: zero
        // usage/cost, no model, no timestamp — so stats just count it as a turn.
        let t = &loaded.turns[0];
        assert_eq!(t.usage, stepper_provider::Usage::default());
        assert_eq!(t.cost_usd, 0.0);
        assert!(t.model_ref.is_empty());
        assert_eq!(t.ended_at, None);

        // A digest-only turn still seeds a synthesized message pair.
        let seed = loaded.seed_messages();
        assert_eq!(seed.len(), 2);
        assert_eq!(seed[0].text(), "add a flag");
        assert!(seed[1].text().contains("decided on --verbose"));
    }

    #[test]
    fn latest_picks_the_most_recently_modified_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        assert!(store.latest().is_none(), "no sessions yet");

        let older = SessionRecord {
            id: "older".into(),
            name: None,
            turns: vec![],
        };
        let newer = SessionRecord {
            id: "newer".into(),
            name: None,
            turns: vec![],
        };
        store.save(&older).unwrap();
        store.save(&newer).unwrap();

        // Drive mtimes explicitly so the test never depends on save timing.
        let sessions = dir.path().join(".stepper").join("sessions");
        let now = std::time::SystemTime::now();
        set_mtime(&sessions.join("older.json"), now - std::time::Duration::from_secs(60));
        set_mtime(&sessions.join("newer.json"), now);
        assert_eq!(store.latest().unwrap().id, "newer");

        // Flip the mtimes: "older" becomes the most recent.
        set_mtime(&sessions.join("older.json"), now + std::time::Duration::from_secs(60));
        assert_eq!(store.latest().unwrap().id, "older");
    }

    #[test]
    fn delete_removes_a_session_and_reports_existence() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let rec = SessionRecord { id: "gone".into(), name: None, turns: vec![] };
        store.save(&rec).unwrap();
        assert!(store.load("gone").is_some());
        assert!(store.delete("gone").unwrap(), "an existing file → true");
        assert!(store.load("gone").is_none(), "the file is removed");
        assert!(!store.delete("gone").unwrap(), "a missing file → false (not an error)");
    }

    #[test]
    fn forked_copies_turns_under_a_fresh_id_and_marks_the_name() {
        let original = SessionRecord {
            id: "orig".into(),
            name: Some("spike".into()),
            turns: vec![TurnRecord { user: "do a thing".into(), ..Default::default() }],
        };
        let fork = original.forked();
        assert_ne!(fork.id, original.id, "the fork gets a fresh id");
        assert_eq!(fork.name.as_deref(), Some("spike (fork)"));
        assert_eq!(fork.turns.len(), 1, "turns are preserved");
        assert_eq!(original.id, "orig", "the original is untouched");
        // A nameless session forks to a nameless fork.
        assert_eq!(SessionRecord::fresh().forked().name, None);
    }

    fn set_mtime(path: &std::path::Path, to: std::time::SystemTime) {
        std::fs::File::options()
            .append(true)
            .open(path)
            .unwrap()
            .set_modified(to)
            .unwrap();
    }
}
