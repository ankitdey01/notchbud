// prefs.rs — the HKCU Run key for "start with windows".

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_ROUTINE_FLAGS, REG_SAM_FLAGS, REG_SZ,
    RRF_RT_REG_SZ,
};

// --- "start with windows" (HKCU\...\Run value "Notchbud") ---
const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Notchbud");

fn win32(err: WIN32_ERROR) -> windows::core::Result<()> {
    if err.is_ok() {
        Ok(())
    } else {
        Err(windows::core::Error::from(err))
    }
}

// current exe, quoted — the exact text we store under the Run key
pub fn current_run_cmd() -> String {
    unsafe {
        let mut buf = [0u16; 512];
        let n = GetModuleFileNameW(None, &mut buf);
        format!("\"{}\"", String::from_utf16_lossy(&buf[..n as usize]))
    }
}

// stored Run-value text; None when absent or unreadable
// (which includes a path too long for the buffer)
fn run_value() -> Option<String> {
    unsafe {
        let mut data = [0u16; 512];
        let mut size = (data.len() * 2) as u32;
        win32(RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            RUN_VALUE,
            REG_ROUTINE_FLAGS(RRF_RT_REG_SZ.0),
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        ))
        .ok()?;
        let end = data.iter().position(|&c| c == 0).unwrap_or(data.len());
        Some(String::from_utf16_lossy(&data[..end]))
    }
}

pub fn is_start_with_windows() -> bool {
    run_value().is_some()
}

// startup self-heal: a Run value pointing at a moved exe is rewritten to the
// current path; an absent or already-current value is left alone.
pub fn heal_start_with_windows() {
    if let Some(v) = run_value() {
        if v != current_run_cmd() {
            let _ = set_start_with_windows(true);
        }
    }
}

pub fn set_start_with_windows(on: bool) -> windows::core::Result<()> {
    unsafe {
        let mut hk = HKEY::default();
        let res = (|| -> windows::core::Result<()> {
            win32(RegOpenKeyExW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY,
                Some(0),
                REG_SAM_FLAGS(KEY_SET_VALUE.0 | KEY_READ.0),
                &mut hk,
            ))?;
            if on {
                let cmd = current_run_cmd();
                let mut wide: Vec<u16> = cmd.encode_utf16().collect();
                wide.push(0);
                win32(RegSetValueExW(
                    hk,
                    RUN_VALUE,
                    Some(0),
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        wide.as_ptr() as *const u8,
                        wide.len() * 2,
                    )),
                ))?;
            } else {
                win32(RegDeleteValueW(hk, RUN_VALUE))?;
            }
            Ok(())
        })();
        if !hk.is_invalid() {
            let _ = RegCloseKey(hk);
        }
        res
    }
}

// --- display name ("What should we call you?") ---
// Plain text at %APPDATA%\notchbud\display_name.txt — not a credential,
// so no DPAPI. Empty (no file) means "not asked yet".
pub const DISPLAY_NAME_MAX: usize = 30;

pub(crate) fn display_name_path() -> Option<std::path::PathBuf> {
    notchbud::data_dir().map(|dir| dir.join("display_name.txt"))
}

/// Trimmed, control-char-free, capped at DISPLAY_NAME_MAX chars, with
/// each word head capitalized ("ankit kumar" -> "Ankit Kumar"). The rest
/// of each word passes through untouched, so "McDonald" survives intact.
// Empty means "nothing to store" — callers ignore it.
pub fn sanitize_display_name(raw: &str) -> String {
    let clean = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(DISPLAY_NAME_MAX)
        .collect::<String>();
    capitalize_words(clean.trim())
}

/// First letter of every word in caps, everything else untouched.
/// Words start after whitespace, hyphens, and apostrophes.
fn capitalize_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut head = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            head = true;
            out.push(ch);
        } else if head && ch.is_alphabetic() {
            // Uppercase the head only (some lowercases expand, e.g. ß->SS).
            let mut up = ch.to_uppercase();
            out.push(up.next().unwrap_or(ch));
            out.extend(up);
            head = false;
        } else {
            if ch == '-' || ch == '\'' || ch == '\u{2019}' {
                head = true;
            } else {
                head = false;
            }
            out.push(ch);
        }
    }
    out
}

pub fn load_display_name() -> String {
    let Some(path) = display_name_path() else {
        return String::new();
    };
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    sanitize_display_name(&s)
}

/// Persist the name; returns what is stored (""). Empty input is a no-op
/// that keeps whatever is on disk.
pub fn save_display_name(raw: &str) -> String {
    let clean = sanitize_display_name(raw);
    if clean.is_empty() {
        return String::new();
    }
    if let Some(path) = display_name_path() {
        if std::fs::create_dir_all(path.parent().unwrap()).is_ok() {
            let tmp = path.with_extension("txt.tmp");
            if std::fs::write(&tmp, clean.as_bytes()).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    // Read-only: the exact text stored under HKCU\...\Run must be a quoted
    // absolute exe path, or the installer and the tray toggle disagree and
    // heal_start_with_windows rewrites it every launch.
    // (Deliberately NOT tested: set_start_with_windows writes the
    // real autostart entry — never touch it from `cargo test`.)
    #[test]
    fn run_cmd_is_quoted_absolute_exe() {
        let cmd = current_run_cmd();
        assert!(
            cmd.starts_with('"') && cmd.ends_with('"'),
            "must be quoted: {cmd}"
        );
        let inner = &cmd[1..cmd.len() - 1];
        assert!(inner.ends_with(".exe"), "must point at an exe: {cmd}");
        assert!(
            inner.len() > 4 && (inner.as_bytes()[1] == b':' || inner.starts_with(r"\\")),
            "must be absolute: {cmd}"
        );
    }

    #[test]
    fn display_name_sanitize_trims_caps_and_strips_controls() {
        assert_eq!(sanitize_display_name("  Alex  "), "Alex");
        assert_eq!(sanitize_display_name(""), "");
        assert_eq!(sanitize_display_name("   "), "");
        assert_eq!(sanitize_display_name("a\nb\tc"), "Abc");
        assert_eq!(sanitize_display_name("ankit kumar"), "Ankit Kumar");
        assert_eq!(sanitize_display_name("o'brien"), "O'Brien");
        assert_eq!(sanitize_display_name("McDonald"), "McDonald");
        assert_eq!(sanitize_display_name("anne-marie"), "Anne-Marie");
        assert_eq!(sanitize_display_name(&"x".repeat(40)).len(), 30);
        // (Deliberately NOT tested: save/load touch %APPDATA% — never
        // touch the real profile from `cargo test`.)
    }
}
