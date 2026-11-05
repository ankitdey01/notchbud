// installer.rs — guards the Inno Setup output in installer/Output.
// Windows-only. Default `cargo test` runs STATIC checks only (read files,
// never install): version sync, per-user directives, freshness, manifest.
// The live install/uninstall cycle is opt-in: #[ignore] + NOTCHBUD_E2E_INSTALL=1.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn cargo_version() -> String {
    let text = fs::read_to_string(root().join("Cargo.toml")).unwrap();
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[package]" {
            in_package = true;
            continue;
        }
        if line.starts_with('[') {
            in_package = false;
        }
        if in_package && line.starts_with("version") {
            let v = line.split('=').nth(1).unwrap().trim().trim_matches('"');
            return v.to_owned();
        }
    }
    panic!("version not found in Cargo.toml [package]");
}

fn iss() -> String {
    fs::read_to_string(root().join("installer/notchbud.iss")).unwrap()
}

fn setup_path() -> PathBuf {
    root().join(format!(
        "installer/Output/Notchbud-Setup-{}.exe",
        cargo_version()
    ))
}

fn target_exe() -> PathBuf {
    root().join("target/release/notchbud.exe")
}

// --- the iss must describe the per-user install we ship ---

#[test]
fn iss_version_matches_cargo() {
    let v = cargo_version();
    assert!(
        iss().contains(&format!("#define MyAppVersion \"{v}\"")),
        "bump MyAppVersion in notchbud.iss alongside Cargo.toml"
    );
}

#[test]
fn iss_stays_per_user() {
    let s = iss();
    // These three are the scars: shipping admin/ProgramFiles/HKLM broke installs.
    assert!(s.contains("PrivilegesRequired=lowest"), "no UAC, ever");
    assert!(
        s.contains("{localappdata}") || s.contains("{userappdata}"),
        "installs into the user profile, not Program Files"
    );
    assert!(s.contains("Root: HKCU"), "autostart must be current-user");
    assert!(s.contains("uninsdeletevalue"), "uninstall must clean the Run value");
    assert!(s.contains("AppId="), "stable upgrade identity required");
}

// --- Output/Setup.exe must be rebuilt after every change ---

#[test]
fn output_setup_is_fresh() {
    if !target_exe().exists() {
        println!("SKIP: target/release/notchbud.exe not built yet — run cargo build --release first");
        return;
    }
    let setup = setup_path();
    assert!(setup.exists(), "missing {} — build it: ISCC installer\\notchbud.iss", setup.display());
    let sm = fs::metadata(&setup).unwrap().modified().unwrap();
    for src in [root().join("installer/notchbud.iss"), target_exe()] {
        let tm = fs::metadata(&src).unwrap().modified().unwrap();
        assert!(
            sm >= tm,
            "stale installer: {} is newer than {} — rebuild with ISCC",
            src.display(),
            setup.display()
        );
    }
}

// --- Setup.exe must not request elevation ---

#[test]
fn setup_manifest_is_as_invoker() {
    let setup = setup_path();
    if !setup.exists() {
        println!("SKIP: installer not built yet");
        return;
    }
    let bytes = fs::read(&setup).unwrap();
    let needle = b"requestedExecutionLevel level=\"asInvoker\"";
    assert!(
        bytes.windows(needle.len()).any(|w| w == needle),
        "Setup.exe must carry asInvoker (a requireAdministrator stub breaks per-user installs)"
    );
}

// --- release binary must be GUI-subsystem (no console flash) ---

fn pe_subsystem(exe: &Path) -> Option<u16> {
    let b = fs::read(exe).ok()?;
    if b.len() < 64 || &b[0..2] != b"MZ" {
        return None;
    }
    let e = u32::from_le_bytes(b[0x3C..0x40].try_into().ok()?) as usize;
    let sig = b.get(e..e + 6)?;
    if &sig[0..4] != b"PE\0\0" || u16::from_le_bytes([sig[4], sig[5]]) != 0x8664 {
        return None; // not an x64 PE; don't judge
    }
    let off = e + 92;
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

#[test]
fn release_binary_has_no_console() {
    let exe = target_exe();
    if !exe.exists() {
        println!("SKIP: target/release/notchbud.exe not built yet");
        return;
    }
    assert_eq!(
        pe_subsystem(&exe),
        Some(2),
        "release exe must be GUI-subsystem (2); 3 = console flash. See windows_subsystem in main.rs"
    );
}

// --- live cycle: install /SILENT to scratch, then uninstall. Opt-in. ---
//
// Run: NOTCHBUD_E2E_INSTALL=1 cargo test -- --ignored --test-threads=1
// WARNING: CloseApplications may close YOUR running notchbud during install.

fn run_value_present() -> bool {
    use windows::Win32::System::Registry::*;
    use windows::core::w;
    unsafe {
        let mut hk = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            0,
            KEY_READ,
            &mut hk,
        )
        .is_err()
        {
            return false;
        }
        let mut ty = REG_VALUE_TYPE(0);
        let r = RegQueryValueExW(hk, w!("Notchbud"), None, Some(&mut ty), None, None);
        let _ = RegCloseKey(hk);
        r.is_ok()
    }
}

#[test]
#[ignore]
fn e2e_silent_install_uninstall() {
    if std::env::var("NOTCHBUD_E2E_INSTALL").as_deref() != Ok("1") {
        println!("SKIP: set NOTCHBUD_E2E_INSTALL=1 to run the live installer cycle");
        return;
    }
    let setup = setup_path();
    assert!(setup.exists(), "build the installer first");
    println!("WARNING: this will close any running notchbud (CloseApplications)");

    let scratch = std::env::temp_dir().join(format!("notchbud-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    let log = scratch.with_extension("log");
    let st = std::process::Command::new(&setup)
        .args([
            "/SILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            &format!("/LOG={}", log.display()),
            &format!("/DIR={}", scratch.display()),
        ])
        .status()
        .expect("launch setup");
    assert!(st.success(), "silent install failed — see {}", log.display());
    assert!(scratch.join("notchbud.exe").exists());
    assert!(scratch.join("unins000.exe").exists());
    // default silent tasks leave autostart off
    assert!(!run_value_present(), "default install must not set autostart");

    // The uninstaller asks about saved login data when auth.bin exists, and a
    // modal box would hang headless runs — tear down manually in that case.
    let authbin = dirs_appdata_notchbud_authbin();
    if authbin.exists() {
        println!("auth.bin present: manual teardown to avoid the login-data prompt");
        teardown_manual(&scratch);
        return;
    }
    let st = std::process::Command::new(scratch.join("unins000.exe"))
        .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .status()
        .expect("launch uninstaller");
    assert!(st.success(), "silent uninstall failed");
    assert!(!scratch.join("notchbud.exe").exists(), "exe must be gone");
    let _ = fs::remove_dir_all(&scratch);
}

fn dirs_appdata_notchbud_authbin() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
        .join("notchbud/auth.bin")
}

fn teardown_manual(scratch: &Path) {
    let _ = fs::remove_dir_all(scratch);
    let menu = PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
        .join("Microsoft/Windows/Start Menu/Programs/Notchbud");
    let _ = fs::remove_dir_all(menu);
    // best-effort removal of this installer's Apps entry (name carries the
    // historical AppId; ignore failure — a stale entry is cosmetic)
    let _ = std::process::Command::new("reg")
        .args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{8E3C9A1B-4F2D-4C7A-9E1B-0A1B2C3D4E50}}_is1",
            "/f",
        ])
        .status();
}
