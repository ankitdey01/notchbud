// user.rs — first-run identity: the name the island greets with.
//
// Stored as plain JSON at %APPDATA%\notchbud\user.json (not a credential):
//   {"v":1,"name":"Ankit"}
// Missing file, unreadable file, invalid JSON, or a blank name all mean the
// same thing to the UI: onboarding (ask for the name). Deleting the file
// manually just re-runs onboarding next launch.

use std::path::{Path, PathBuf};

/// Canonical onboarding limit: the input box stops accepting here, and saves
/// trim to this width. Long enough for any real name, short enough to fit the
/// greeting line inside the 420u island.
pub const NAME_MAX_CHARS: usize = 30;

/// Where the profile lives (same folder as auth.bin / host_id).
pub fn profile_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|base| Path::new(&base).join("notchbud").join("user.json"))
}

/// Trim + validate a raw name draft. Returns the canonical name to store, or
/// None when there is nothing worth storing (empty / whitespace-only /
/// control characters only).
pub fn normalize_name(raw: &str) -> Option<String> {
    // A pasted line break is not a name — reject before the whitespace
    // squeeze below would silently launder it into a space.
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    // Collapse surrounding whitespace; keep inner spacing but squeeze runs so
    // "  Ankit   Kumar  " greets as "Ankit Kumar".
    let mut parts: Vec<&str> = Vec::new();
    for word in raw.split_whitespace() {
        if !word.is_empty() {
            parts.push(word);
        }
    }
    let name = parts.join(" ");
    if name.is_empty() {
        return None;
    }
    // Anything else — any script, hyphen, apostrophe, period — is a valid
    // name.
    if name.chars().count() > NAME_MAX_CHARS {
        return None;
    }
    Some(name)
}

/// User-facing reason a name draft was rejected (shown under the input box).
pub fn name_error(raw: &str) -> &'static str {
    if raw.trim().is_empty() {
        "Please type your name, then press Enter."
    } else if raw.chars().any(|c| c.is_control()) {
        "That name has a line break in it — one line is enough."
    } else {
        "Keep it to 30 characters so it fits the island."
    }
}

pub fn load_name_from(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // Preferred shape: {"name":"Ankit",...}. A leading '{' or '"' that fails
    // to parse is corruption, not a name — never greet with broken JSON.
    // Anything else is treated as a bare name file (hand edits, older
    // builds) so we never re-ask needlessly.
    if text.starts_with('{') || text.starts_with('"') {
        let v = serde_json::from_str::<serde_json::Value>(text).ok()?;
        if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
            return normalize_name(n);
        }
        if let Some(s) = v.as_str() {
            return normalize_name(s);
        }
        return None;
    }
    normalize_name(text)
}

/// Load the saved name, if any. Anything unusable -> None (onboarding).
pub fn load_name() -> Option<String> {
    let path = profile_path()?;
    load_name_from(&path)
}

/// A failed save: `ui` is the one-liner for the island, `detail` carries the
/// path/os error for the debug log. Validation rejects show their reason in
/// both; only filesystem failures use the generic UI line.
#[derive(Debug, PartialEq)]
pub struct SaveError {
    pub ui: &'static str,
    pub detail: String,
}

pub fn save_name_to(path: &Path, raw: &str) -> Result<String, SaveError> {
    let name = normalize_name(raw).ok_or_else(|| {
        let m = name_error(raw);
        SaveError { ui: m, detail: m.to_owned() }
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SaveError {
            ui: "Could not save your name. Try again.",
            detail: format!("mkdir {parent:?}: {e}"),
        })?;
    }
    let body = serde_json::json!({"v": 1, "name": name}).to_string();
    // Atomic write: a torn user.json must never strand onboarding.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| SaveError {
        ui: "Could not save your name. Try again.",
        detail: format!("write {tmp:?}: {e}"),
    })?;
    std::fs::rename(&tmp, path).map_err(|e| SaveError {
        ui: "Could not save your name. Try again.",
        detail: format!("rename {tmp:?}: {e}"),
    })?;
    Ok(name)
}

/// Persist the name. Returns the canonical stored form for the UI to greet.
pub fn save_name(raw: &str) -> Result<String, SaveError> {
    let path = profile_path().ok_or(SaveError {
        ui: "Could not save your name. Try again.",
        detail: "no %APPDATA%".to_owned(),
    })?;
    save_name_to(&path, raw)
}

/// Local hour in 0..=23 via the OS clock (no extra deps, respects the user's
/// time zone and DST — the greeting follows "their" morning/evening).
pub fn local_hour() -> u32 {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wHour as u32) % 24
}

/// Pure greeting picker (injected hour -> unit-testable). Boundaries match the
/// everyday sense: 5–11 morning, 12–16 afternoon, 17–21 evening, else night.
pub fn greeting_for_hour(hour: u32, name: &str) -> String {
    let h = hour % 24;
    let part = if (5..12).contains(&h) {
        "Good morning"
    } else if (12..17).contains(&h) {
        "Good afternoon"
    } else if (17..22).contains(&h) {
        "Good evening"
    } else {
        "Good night"
    };
    if name.is_empty() {
        part.to_owned()
    } else {
        format!("{part}, {name}")
    }
}

/// Greeting for right now: "Good evening, Ankit" at 7pm, etc.
pub fn greeting_now(name: &str) -> String {
    greeting_for_hour(local_hour(), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_squeezes_spaces() {
        assert_eq!(normalize_name("  Ankit   Kumar  "), Some("Ankit Kumar".into()));
        assert_eq!(normalize_name("Ankit"), Some("Ankit".into()));
    }

    #[test]
    fn rejects_blank_and_control_and_too_long() {
        assert_eq!(normalize_name(""), None);
        assert_eq!(normalize_name("   "), None);
        assert_eq!(normalize_name("a\nb"), None);
        assert_eq!(normalize_name(&"a".repeat(31)), None);
        assert_eq!(normalize_name(&"a".repeat(30)).unwrap().len(), 30);
    }

    #[test]
    fn accepts_international_names() {
        assert_eq!(normalize_name("José-O'Brien Jr."), Some("José-O'Brien Jr.".into()));
        assert_eq!(normalize_name("अंकित"), Some("अंकित".into()));
    }

    // Scratch space under the workspace target/ dir: spawned test processes
    // cannot necessarily write to the OS temp dir here, but the workspace is
    // always writable.
    #[cfg(test)]
    fn test_scratch_dir(tag: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("test-tmp-{tag}-{}", std::process::id()))
    }

    #[test]
    fn greeting_boundaries() {
        assert_eq!(greeting_for_hour(4, "Ankit"), "Good night, Ankit");
        assert_eq!(greeting_for_hour(5, "Ankit"), "Good morning, Ankit");
        assert_eq!(greeting_for_hour(11, "Ankit"), "Good morning, Ankit");
        assert_eq!(greeting_for_hour(12, "Ankit"), "Good afternoon, Ankit");
        assert_eq!(greeting_for_hour(16, "Ankit"), "Good afternoon, Ankit");
        assert_eq!(greeting_for_hour(17, "Ankit"), "Good evening, Ankit");
        assert_eq!(greeting_for_hour(19, "Ankit"), "Good evening, Ankit");
        assert_eq!(greeting_for_hour(21, "Ankit"), "Good evening, Ankit");
        assert_eq!(greeting_for_hour(22, "Ankit"), "Good night, Ankit");
        assert_eq!(greeting_for_hour(0, "Ankit"), "Good night, Ankit");
    }

    #[test]
    fn roundtrip_through_file() {
        let dir = test_scratch_dir("roundtrip");
        let path = dir.join("user.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(save_name_to(&path, "  Ankit  "), Ok("Ankit".into()));
        assert_eq!(load_name_from(&path), Some("Ankit".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_blank_file_means_onboarding() {
        let dir = test_scratch_dir("bad");
        let path = dir.join("user.json");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_name_from(&path), None);
        std::fs::write(&path, r#"{"v":1,"name":"   "}"#).unwrap();
        assert_eq!(load_name_from(&path), None);
        std::fs::write(&path, "").unwrap();
        assert_eq!(load_name_from(&path), None);
        // Bare-name fallback still works.
        std::fs::write(&path, "Ankit").unwrap();
        assert_eq!(load_name_from(&path), Some("Ankit".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
