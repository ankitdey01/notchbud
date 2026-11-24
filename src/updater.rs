// updater.rs — free-for-all in-island updates from GitHub Releases.
//
// Source of truth is tags `v*.*.*` published by release.yml (must match
// Cargo.toml). The checker hits `/releases/latest`, picks the Inno
// `Notchbud-Setup-*.exe` asset, and the island glows until applied.
// Clicking Update downloads the Setup exe to %TEMP% and launches it,
// then quits so Inno (`CloseApplications=yes`) can replace the install.
// Per-user install (PrivilegesRequired=lowest): no UAC, no admin.

pub const REPO: &str = "ankitdey01/notchbud";
pub const CHECK_INTERVAL_SECS: u64 = 3600;
pub const SETUP_MARKER: &str = "setup";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

/// Strip leading `v`/`V` + whitespace: `v0.1.4` -> `0.1.4`.
pub fn clean_tag(tag: &str) -> String {
    tag.trim()
        .trim_start_matches(['v', 'V'])
        .trim()
        .to_owned()
}

/// True when `latest` is newer than `current` (numeric dot comparison,
/// missing parts are 0: `0.1.4` > `0.1.3`, `0.2` > `0.1.9`).
pub fn version_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| {
        clean_tag(s)
            .split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>()
    };
    let (mut l, mut c) = (parse(latest), parse(current));
    let n = l.len().max(c.len()).max(1);
    l.resize(n, 0);
    c.resize(n, 0);
    l > c
}

/// Pick the Inno Setup asset from a `/releases/latest` payload.
/// The asset named for this tag (`Notchbud-Setup-{version}.exe`) wins:
/// a release can carry a stale setup from an older tag (glob upload),
/// and installing that re-triggers the same update forever. Falls back
/// to the first setup exe only when nothing is versioned.
pub fn pick_setup_asset(release: &serde_json::Value) -> Option<(String, String)> {
    let tag = release.get("tag_name")?.as_str()?;
    let version = clean_tag(tag);
    if version.is_empty() {
        return None;
    }
    let assets = release.get("assets")?.as_array()?;
    let wanted = format!("notchbud-setup-{version}.exe");
    let mut fallback: Option<String> = None;
    for a in assets {
        let name = a.get("name")?.as_str().unwrap_or("");
        let url = a.get("browser_download_url")?.as_str().unwrap_or("");
        if url.is_empty() {
            continue;
        }
        let lower = name.to_lowercase();
        if !(lower.contains(SETUP_MARKER) && lower.ends_with(".exe")) {
            continue;
        }
        if lower == wanted {
            return Some((version, url.to_owned()));
        }
        if fallback.is_none() {
            fallback = Some(url.to_owned());
        }
    }
    fallback.map(|url| (version, url))
}

/// One synchronous check against the GitHub API.
/// `Ok(None)` = up to date (or no Setup asset); `Err` = the check itself
/// failed (offline, rate-limited, …) so the caller can say so honestly.
pub fn check_for_update(current: &str) -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = ureq::get(&url)
        .set("User-Agent", "notchbud-updater")
        .set("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .map_err(|e| e.to_string())?;
    if resp.status() != 200 {
        return Err(format!("GitHub API HTTP {}", resp.status()));
    }
    let body = resp.into_string().map_err(|e| e.to_string())?;
    let release: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let Some((version, dl)) = pick_setup_asset(&release) else {
        return Ok(None);
    };
    if !version_newer(&version, current) {
        return Ok(None);
    }
    Ok(Some(UpdateInfo { version, url: dl }))
}

/// Download the Setup exe to a file. Streams to disk (no whole-file RAM).
pub fn download_setup(url: &str, dest: &std::path::Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", "notchbud-updater")
        .timeout(std::time::Duration::from_secs(120))
        .call()
        .map_err(|e| e.to_string())?;
    if resp.status() != 200 {
        return Err(format!("download HTTP {}", resp.status()));
    }
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_cleaning_strips_prefix_and_space() {
        assert_eq!(clean_tag("v0.1.4"), "0.1.4");
        assert_eq!(clean_tag("V0.1.4"), "0.1.4");
        assert_eq!(clean_tag("  v0.1.4  "), "0.1.4");
        assert_eq!(clean_tag("0.1.4"), "0.1.4");
        assert_eq!(clean_tag("v"), "");
    }

    #[test]
    fn newer_compares_numerically_not_lexically() {
        assert!(version_newer("0.1.4", "0.1.3"));
        // Lexical compare would call 0.1.9 > 0.1.10 — numeric must not.
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(version_newer("v0.2.0", "0.1.9"));
        assert!(version_newer("1.0.0", "0.9.9"));
        assert!(!version_newer("0.1.3", "0.1.3"));
        assert!(!version_newer("0.1.2", "0.1.3"));
        // Missing parts are 0: 0.1 == 0.1.0, never newer.
        assert!(!version_newer("0.1", "0.1.0"));
        assert!(version_newer("0.1.0.1", "0.1"));
    }

    #[test]
    fn picks_setup_exe_over_portable() {
        let v = serde_json::json!({
            "tag_name": "v0.1.4",
            "body": "fixes",
            "assets": [
                {"name": "notchbud.exe", "browser_download_url": "https://x/notchbud.exe"},
                {"name": "Notchbud-Setup-0.1.4.exe", "browser_download_url": "https://x/setup.exe"},
            ]
        });
        let (ver, url) = pick_setup_asset(&v).expect("setup must win");
        assert_eq!(ver, "0.1.4");
        assert_eq!(url, "https://x/setup.exe");
    }

    #[test]
    fn setup_match_is_case_insensitive() {
        let v = serde_json::json!({
            "tag_name": "v0.1.4",
            "assets": [{"name": "NOTCHBUD-SETUP-0.1.4.EXE", "browser_download_url": "https://x/s.exe"}]
        });
        assert!(pick_setup_asset(&v).is_some());
    }

    #[test]
    fn rejects_non_exe_setup_artifacts() {
        // A "setup" zip must not install: only .exe glows the button.
        let v = serde_json::json!({
            "tag_name": "v0.1.4",
            "assets": [
                {"name": "notchbud.exe", "browser_download_url": "https://x/e.exe"},
                {"name": "Notchbud-Setup-0.1.4.zip", "browser_download_url": "https://x/s.zip"},
            ]
        });
        assert!(pick_setup_asset(&v).is_none());
    }

    #[test]
    fn portable_only_release_stays_silent() {
        // No Setup asset at all: swapping the raw exe would orphan the
        // Inno uninstall entries, so the island must not glow.
        let v = serde_json::json!({
            "tag_name": "v0.1.4",
            "assets": [{"name": "notchbud.exe", "browser_download_url": "https://x/e.exe"}]
        });
        assert!(pick_setup_asset(&v).is_none());
    }

    #[test]
    fn malformed_releases_are_none() {
        // No tag at all.
        let no_tag = serde_json::json!({
            "assets": [{"name": "Notchbud-Setup-0.1.4.exe", "browser_download_url": "https://x/s.exe"}]
        });
        assert!(pick_setup_asset(&no_tag).is_none());
        // Tag with no version in it.
        let empty_ver = serde_json::json!({
            "tag_name": "v",
            "assets": [{"name": "Notchbud-Setup-0.1.4.exe", "browser_download_url": "https://x/s.exe"}]
        });
        assert!(pick_setup_asset(&empty_ver).is_none());
        // No assets key.
        let no_assets = serde_json::json!({"tag_name": "v0.1.4"});
        assert!(pick_setup_asset(&no_assets).is_none());
        // Setup asset with an empty URL is skipped, not picked.
        let empty_url = serde_json::json!({
            "tag_name": "v0.1.4",
            "assets": [{"name": "Notchbud-Setup-0.1.4.exe", "browser_download_url": ""}]
        });
        assert!(pick_setup_asset(&empty_url).is_none());
    }

    #[test]
    fn ignores_stale_setup_from_an_older_tag() {
        // The v0.1.6 release carried a leftover 0.1.5 setup (glob
        // upload): installing it re-offered the same update forever.
        let v = serde_json::json!({
            "tag_name": "v0.1.6",
            "assets": [
                {"name": "Notchbud-Setup-0.1.5.exe", "browser_download_url": "https://x/stale.exe"},
                {"name": "notchbud.exe", "browser_download_url": "https://x/e.exe"},
                {"name": "Notchbud-Setup-0.1.6.exe", "browser_download_url": "https://x/fresh.exe"},
            ]
        });
        let (ver, url) = pick_setup_asset(&v).expect("versioned setup must win");
        assert_eq!(ver, "0.1.6");
        assert_eq!(url, "https://x/fresh.exe");
    }

    #[test]
    fn skips_unusable_assets_and_takes_first_good_setup() {
        let v = serde_json::json!({
            "tag_name": "v0.1.4",
            "assets": [
                {"name": "Notchbud-Setup-0.1.4.exe", "browser_download_url": ""},
                {"name": "notchbud.exe", "browser_download_url": "https://x/e.exe"},
                {"name": "Notchbud-Setup-0.1.4.exe", "browser_download_url": "https://x/good.exe"},
            ]
        });
        let (_, url) = pick_setup_asset(&v).expect("must skip to the good asset");
        assert_eq!(url, "https://x/good.exe");
    }

    #[test]
    fn download_fails_fast_without_network() {
        // Loopback port 1 refuses immediately: fully offline, no panic.
        let dest = std::env::temp_dir().join("notchbud-test-unreachable.exe");
        assert!(download_setup("http://127.0.0.1:1/Notchbud-Setup-0.0.0.exe", &dest).is_err());
        assert!(!dest.exists(), "a failed download must not leave a file");
    }
}
