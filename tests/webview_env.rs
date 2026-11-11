// tests/webview_env.rs — TDD file for the black-island issue.
//
// Symptom: `cargo run --release` as a standard user shows a solid black
// island (shell only, no web content), while an elevated run works.
// Root-cause class: WebView2 environment creation fails — the user-data
// dir is unusable (held by another live WebView2 instance, or not
// writable) — and `island_web::init` silently returns early, leaving
// READY=false forever: no navigation, no state pushes, and (release
// builds have no console) zero diagnostics.
//
// The fix, pinned here first:
//   1. Probe the picked dir for writability before handing it to WebView2;
//      an unusable dir must yield None so the caller falls back instead
//      of producing a black island.
//   2. Attempt order is explicit-dir-then-default: a failed explicit
//      environment must fall back to the default environment, never abort.

use notchbud::island_web::{env_attempt_order, pick_user_data_dir, pick_user_data_dir_with, EnvDir};

/// Writable scratch base for probe tests. NOTE: this deliberately avoids
/// %TEMP%: child test binaries in this environment get
/// "Access is denied (os error 5)" creating files outside the workspace,
/// while `target/` is proven writable (cargo itself works there).
fn scratch_base(name: &str) -> std::path::PathBuf {
    let mut base = std::env::current_dir().expect("test working dir");
    base.push("target");
    base.push("tmp-webview-probe");
    base.push(name);
    base
}

#[test]
fn picker_returns_absolute_dir_under_base() {
    // Arrange: a usable base dir.
    let base = scratch_base("absolute");
    // Act.
    let dir = pick_user_data_dir(Some(base.as_os_str()));
    // Assert: explicit profile dir, absolute.
    assert!(dir.is_some());
    let dir = dir.unwrap();
    assert!(dir.is_absolute());
    assert!(dir.ends_with(std::path::Path::new("notchbud").join("EBWebView")));
}

#[test]
fn picker_returns_none_without_base() {
    // No LOCALAPPDATA (or equivalent): fall back to the default env.
    assert!(pick_user_data_dir(None).is_none());
}

#[test]
fn picker_rejects_dir_that_fails_the_write_probe() {
    // Arrange: a probe reporting "not writable" — e.g. the dir exists but
    // is held by another live WebView2 instance or carries foreign ACLs.
    let failing_probe = |_dir: &std::path::Path| false;
    // Act.
    let dir = pick_user_data_dir_with(Some(scratch_base("rejected").as_os_str()), &failing_probe);
    // Assert: reject it so init falls back instead of a black island.
    assert!(dir.is_none());
}

#[test]
fn picker_accepts_dir_that_passes_the_write_probe() {
    // Arrange: a probe reporting "writable".
    let passing_probe = |_dir: &std::path::Path| true;
    // Act.
    let dir = pick_user_data_dir_with(Some(scratch_base("accepted").as_os_str()), &passing_probe);
    // Assert.
    assert!(dir.is_some());
}

#[test]
fn env_attempt_order_tries_explicit_then_default() {
    // Arrange.
    let primary = scratch_base("primary").join("EBWebView");
    // Act.
    let order = env_attempt_order(Some(primary.clone()));
    // Assert: explicit first, default as the fallback that avoids black.
    assert_eq!(order.len(), 2);
    assert!(matches!(&order[0], EnvDir::Explicit(p) if p == &primary));
    assert!(matches!(&order[1], EnvDir::Default));
}

#[test]
fn env_attempt_order_is_default_only_without_primary() {
    // Act.
    let order = env_attempt_order(None);
    // Assert.
    assert_eq!(order.len(), 1);
    assert!(matches!(&order[0], EnvDir::Default));
}
