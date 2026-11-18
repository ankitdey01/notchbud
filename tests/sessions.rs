// tests/sessions.rs — disk contract for local chat sessions.
// Unit tests inside src/sessions.rs cover the pure logic (titles, preview,
// upsert cap); this file pins the file behavior: missing/empty/corrupt
// files never lose the island, and save/load round-trips exactly.

use notchbud::connector::chat::Role;
use notchbud::sessions::{self, Session, StoredTurn};

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point NOTCHBUD_DATA_DIR at a fresh temp dir, holding the lock so parallel
/// tests in this binary can't steal the env var. Restores + cleans up on drop.
struct TempDataDir {
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
    prev: Option<std::ffi::OsString>,
}

impl TempDataDir {
    fn set() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("notchbud-sess-{}-{n}", std::process::id());
        // Prefer the OS temp dir; fall back to the repo target dir when
        // writes outside the workspace are denied (sandboxed runs).
        let ws_fallback =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        let dir = [std::env::temp_dir(), ws_fallback]
            .into_iter()
            .map(|b| b.join(&name))
            .find_map(|d| std::fs::create_dir_all(&d).ok().map(|()| d))
            .expect("no writable temp dir for tests");
        let prev = std::env::var_os("NOTCHBUD_DATA_DIR");
        std::env::set_var("NOTCHBUD_DATA_DIR", &dir);
        TempDataDir { _lock: lock, dir, prev }
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.join("sessions.json")
    }
}

impl Drop for TempDataDir {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("NOTCHBUD_DATA_DIR", v),
            None => std::env::remove_var("NOTCHBUD_DATA_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sess(id: &str, title: &str) -> Session {
    Session {
        id: id.into(),
        title: title.into(),
        updated_ms: 7,
        messages: vec![StoredTurn {
            role: "user".into(),
            text: "hi".into(),
        }],
    }
}

#[test]
fn missing_file_loads_empty() {
    let _tmp = TempDataDir::set();
    assert!(sessions::load_sessions().is_empty());
}

#[test]
fn empty_file_loads_empty() {
    let tmp = TempDataDir::set();
    std::fs::write(tmp.path(), "   \n").unwrap();
    assert!(sessions::load_sessions().is_empty());
}

#[test]
fn save_load_round_trips() {
    let _tmp = TempDataDir::set();
    let made = sessions::snapshot(
        "s1",
        "Trip",
        &[
            (Role::User, "hi".into(), None),
            (Role::Assistant, "hey".into(), None),
        ],
    );
    sessions::save_sessions(&[made]);
    let back = sessions::load_sessions();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, "s1");
    assert_eq!(back[0].title, "Trip");
    assert_eq!(back[0].messages.len(), 2);
    assert_eq!(back[0].messages[0].role, "user");
    assert_eq!(back[0].messages[1].role, "assistant");
    assert_eq!(back[0].display_title(), "Trip");
    assert_eq!(back[0].preview(), "hey");
}

#[test]
fn corrupt_file_quarantines_and_recovers() {
    let tmp = TempDataDir::set();
    std::fs::write(tmp.path(), "{not json").unwrap();
    assert!(sessions::load_sessions().is_empty());
    let bad = tmp.dir.join("sessions.json.bad");
    assert!(bad.exists(), "corrupt file must be kept aside as .bad");
    // Next save starts fresh over the quarantined file.
    sessions::save_sessions(&[sess("a", "A")]);
    let back = sessions::load_sessions();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, "a");
}

#[test]
fn snapshot_keeps_id_title_and_role_strings() {
    let s = sessions::snapshot("id-1", "T", &[(Role::User, "q".into(), None)]);
    assert_eq!(s.id, "id-1");
    assert_eq!(s.title, "T");
    assert_eq!(s.messages.len(), 1);
    assert_eq!(s.messages[0].role, "user");
    assert_eq!(s.messages[0].text, "q");
}

#[test]
fn titles_fall_back_without_whitespace_surprises() {
    // Whitespace-only Groq title falls back to the first user turn.
    let mut s = sess("a", "   ");
    assert_eq!(s.display_title(), "hi");
    // Assistant-only history has no user turn to borrow.
    s.messages = vec![StoredTurn {
        role: "assistant".into(),
        text: "hello".into(),
    }];
    assert_eq!(s.display_title(), "New chat");
    // Snippets cut on chars, not bytes: multibyte stays intact + capped.
    s.messages = vec![StoredTurn {
        role: "user".into(),
        text: "é".repeat(100),
    }];
    let t = s.display_title();
    assert!(t.ends_with('…'));
    assert!(t.chars().count() <= 41);
}
