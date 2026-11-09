// notchbud build script — embeds the cat logo as the native exe icon.
// std-only on purpose: compiles the sibling assets/app.rc with the Windows
// SDK rc.exe (no new crates, works offline) and hands the resulting .res to
// link.exe, which accepts .res inputs directly. Rebuilds when the .rc or the
// .ico changes. Only meaningful for MSVC Windows targets; on any other host
// (or without the SDK) it degrades to a no-op so `cargo check` keeps working.

use std::path::PathBuf;
use std::process::Command;

fn find_rc_exe() -> Option<PathBuf> {
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let entries = std::fs::read_dir(&kits).ok()?;
    let mut best: Option<(String, PathBuf)> = None;
    for entry in entries.flatten() {
        let version = entry.file_name().to_string_lossy().into_owned();
        let candidate = entry.path().join("x64").join("rc.exe");
        if candidate.is_file() && best.as_ref().map_or(true, |(v, _)| version > *v) {
            best = Some((version, candidate));
        }
    }
    best.map(|(_, path)| path)
}

fn main() {
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let rc_file = manifest.join("assets").join("app.rc");
    let ico_file = manifest.join("assets").join("notchbud.ico");
    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!("cargo:rerun-if-changed={}", ico_file.display());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=Windows SDK rc.exe not found; exe ships without the logo icon");
        return;
    };
    let out_res = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("app.res");
    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&out_res)
        .arg(&rc_file)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg-bins={}", out_res.display());
        }
        Ok(s) => panic!("rc.exe failed with status {s}"),
        Err(e) => panic!("could not run {}: {e}", rc_exe.display()),
    }
}
