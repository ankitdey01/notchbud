// sessions.rs — local chat sessions, plain JSON on disk.
//
// Every conversation survives Back / collapse / restart: Back archives the
// live turns into `sessions.json` instead of dropping them, and the Chats
// page lists them linearly (newest first) with title + last message.

use serde::{Deserialize, Serialize};

use crate::connector::chat::Role;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredTurn {
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub messages: Vec<StoredTurn>,
    #[serde(default)]
    pub unread: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Session {
    /// Last message text for the Chats list preview ("" when empty).
    pub fn preview(&self) -> String {
        let t = self
            .messages
            .last()
            .map(|m| m.text.trim())
            .unwrap_or_default();
        truncate(t, 80)
    }

    /// Display title: the Groq title once set, else a snippet of the first
    /// user turn, else "New chat".
    pub fn display_title(&self) -> String {
        let t = self.title.trim();
        if !t.is_empty() {
            return t.to_owned();
        }
        let first = self
            .messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.text.trim())
            .unwrap_or_default();
        if first.is_empty() {
            return "New chat".to_owned();
        }
        truncate(first, 40)
    }
}

/// First `max` chars plus '…' when cut. Single source for the list snippets.
fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Sessions live next to the name/key files: %APPDATA%\notchbud\sessions.json
/// (or NOTCHBUD_DATA_DIR when set).
pub fn sessions_path() -> Option<std::path::PathBuf> {
    crate::data_dir().map(|dir| dir.join("sessions.json"))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Unique-enough local id without a new dependency: millis + process counter.
pub fn new_id() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}-{:x}-{}", now_ms(), std::process::id(), n)
}

pub fn load_sessions() -> Vec<Session> {
    let Some(path) = sessions_path() else {
        return Vec::new();
    };
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            quarantine(&path);
            return Vec::new();
        }
    };
    if s.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<Session>>(&s) {
        Ok(v) => v,
        Err(_) => {
            quarantine(&path);
            Vec::new()
        }
    }
}

/// Move a broken sessions file aside so the next save starts fresh without
/// destroying evidence. Best-effort.
fn quarantine(path: &std::path::Path) {
    let bad = path.with_extension("json.bad");
    let _ = std::fs::remove_file(&bad);
    let _ = std::fs::rename(path, &bad);
}

/// Atomic save (tmp + rename); best-effort like the other local files.
pub fn save_sessions(sessions: &[Session]) {
    let Some(path) = sessions_path() else {
        return;
    };
    if std::fs::create_dir_all(path.parent().unwrap()).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string(sessions) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

const MAX_SESSIONS: usize = 50;

/// Insert/replace `session` at the front (newest first), capped.
pub fn upsert(sessions: &mut Vec<Session>, session: Session) {
    sessions.retain(|s| s.id != session.id);
    sessions.insert(0, session);
    sessions.truncate(MAX_SESSIONS);
}

/// Drop the session with `id`. True when something was removed.
pub fn remove(sessions: &mut Vec<Session>, id: &str) -> bool {
    let before = sessions.len();
    sessions.retain(|s| s.id != id);
    sessions.len() != before
}

/// Build a Session snapshot from the live chat turns (text only — a
/// turn's thumbnail never lands on disk).
pub fn snapshot(id: &str, title: &str, history: &[(Role, String, Option<String>)]) -> Session {
    Session {
        id: id.to_owned(),
        title: title.to_owned(),
        updated_ms: now_ms(),
        messages: history
            .iter()
            .map(|(role, text, _)| StoredTurn {
                role: role.as_str().to_owned(),
                text: text.clone(),
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_less_records_still_deserialize() {
        // Old/corrupt rows without a title must not kill the whole file.
        let v: Vec<Session> = serde_json::from_str(r#"[{"id":"a","messages":[]}]"#).unwrap();
        assert_eq!(v[0].title, "");
        assert_eq!(v[0].display_title(), "New chat");
        assert!(!v[0].unread);
        assert!(v[0].error.is_none());
    }

    #[test]
    fn display_title_prefers_groq_then_snippet_then_new() {
        let mut s = Session {
            id: "a".into(),
            title: "Trip Ideas".into(),
            updated_ms: 0,
            messages: vec![StoredTurn {
                role: "user".into(),
                text: "hello".into(),
            }],
            ..Default::default()
        };
        assert_eq!(s.display_title(), "Trip Ideas");
        s.title.clear();
        assert_eq!(s.display_title(), "hello");
        s.messages.clear();
        assert_eq!(s.display_title(), "New chat");
    }

    #[test]
    fn preview_is_last_message_truncated() {
        let s = Session {
            id: "a".into(),
            title: String::new(),
            updated_ms: 0,
            messages: vec![
                StoredTurn {
                    role: "user".into(),
                    text: "hi".into(),
                },
                StoredTurn {
                    role: "assistant".into(),
                    text: "hey there".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(s.preview(), "hey there");
        let long = Session {
            id: "b".into(),
            title: String::new(),
            updated_ms: 0,
            messages: vec![StoredTurn {
                role: "user".into(),
                text: "x".repeat(100),
            }],
            ..Default::default()
        };
        assert!(long.preview().ends_with('…'));
        assert!(long.preview().chars().count() <= 81);
    }

    #[test]
    fn upsert_moves_to_front_and_caps() {
        let mut v = Vec::new();
        for i in 0..55 {
            upsert(
                &mut v,
                Session {
                    id: format!("s{i}"),
                    title: String::new(),
                    updated_ms: i,
                    messages: vec![],
                    ..Default::default()
                },
            );
        }
        assert_eq!(v.len(), MAX_SESSIONS);
        assert_eq!(v[0].id, "s54");
        // Re-saving an old id moves it to the front, no duplicate.
        let old = v[10].clone();
        upsert(&mut v, old.clone());
        assert_eq!(v[0].id, old.id);
        assert_eq!(v.iter().filter(|s| s.id == old.id).count(), 1);
    }

    #[test]
    fn new_id_unique() {
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn remove_drops_only_the_named_session() {
        let mut v = vec![
            Session {
                id: "a".into(),
                title: String::new(),
                updated_ms: 0,
                messages: vec![],
                ..Default::default()
            },
            Session {
                id: "b".into(),
                title: "B".into(),
                updated_ms: 0,
                messages: vec![],
                ..Default::default()
            },
        ];
        assert!(remove(&mut v, "a"));
        assert_eq!(v.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["b"]);
        assert!(!remove(&mut v, "missing"));
    }
}
