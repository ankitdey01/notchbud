// tests/island_page.rs — contract tests for the embedded island page (ui/).
//
// The island page is baked into the binary (include_str!) and driven only via
// ExecuteScript: a JS-level breakage (a duplicated line silently nesting the
// IIFE body into a never-called function, a missing element id, an external
// link navigating the WebView away, an intent without a Rust handler) shows up
// on-device as a dead island with no Rust-side error. Pin the contracts here
// in plain source terms so edits fail here instead of on a user's screen.

use std::path::PathBuf;

fn ui_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("ui");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing ui/{name}"))
}

fn src_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing src/{name}"))
}

fn js() -> String {
    ui_file("app.js")
}

fn html() -> String {
    ui_file("index.html")
}

/// A duplicated content line silently nests the rest of the IIFE body into
/// a never-called function: still valid syntax (node --check passes), but
/// `window.notchbud` is never assigned and no listener ever attaches — a dead
/// island with no error anywhere. That exact incident is what this pins.
/// (Bare delimiter lines like `}` legitimately repeat, so only lines with
/// actual content count.)
#[test]
fn no_duplicated_header_lines() {
    let mut prev = String::new();
    for (n, line) in js().lines().enumerate() {
        let t = line.trim();
        if !t.is_empty() && t == prev && t.chars().any(|c| c.is_alphanumeric()) {
            panic!("ui/app.js:{}: duplicated line nests the IIFE body: {t}", n + 1);
        }
        prev = t.to_string();
    }
}

/// Every element id the script looks up must exist in the page, or the lookup
/// is a silent null and that whole flow (key entry, send, settings) is dead.
#[test]
fn js_element_ids_exist_in_html() {
    let js = js();
    let html = html();
    let mut ids: Vec<String> = Vec::new();
    let mut rest = js.as_str();
    while let Some(i) = rest.find("el(\"") {
        let after = &rest[i + 4..];
        if let Some(end) = after.find('"') {
            ids.push(after[..end].to_owned());
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    assert!(!ids.is_empty(), "no el() lookups found — scanner broke");
    for id in &ids {
        assert!(
            html.contains(&format!("id=\"{id}\"")),
            "ui/app.js looks up #{id}, but ui/index.html has no such id"
        );
    }
}

/// No external navigation inside the embedded page: one click on a raw
/// `http` link navigates the WebView away from the island page and every
/// Rust push/intent silently no-ops until restart. External URLs must go
/// through Rust (system browser), never an anchor.
#[test]
fn island_html_has_no_external_links() {
    assert!(
        !html().contains("href=\"http"),
        "embedded page must not link out: open external URLs via Rust"
    );
}

/// `type: "xxx"` values the page can post.
fn posted_intents(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = source;
    while let Some(i) = rest.find("type: \"") {
        let after = &rest[i + 7..];
        if let Some(end) = after.find('"') {
            out.push(after[..end].to_owned());
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Every intent the page posts must have a Rust handler arm, or the click
/// silently does nothing.
#[test]
fn posted_intents_have_rust_handlers() {
    let posted = posted_intents(&js());
    assert!(!posted.is_empty(), "no intents found — scanner broke");
    let main = src_file("main.rs");
    let span = main
        .find("fn handle_webmsg")
        .and_then(|s| {
            main[s..]
                .find("\nfn ")
                .map(|e| main[s..s + e].to_string())
        })
        .expect("handle_webmsg not found");
    for intent in &posted {
        assert!(
            span.contains(&format!("Some(\"{intent}\")")),
            "page posts {intent:?}, but handle_webmsg has no arm for it"
        );
    }
}
