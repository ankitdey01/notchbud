// island_layout.rs — contract tests for the island page layout.
//
// Menu + Notifications pin their heads to one shared top offset, so both
// titles render at exactly the same (x, y) regardless of body height. The
// headless settings page still centers as a group via auto margins. That
// centering is fragile in one specific way: the Back link is
// `display: none`, and auto margins on a display:none box absorb nothing —
// so if the bottom margin lands on the hidden Back link instead of the last
// VISIBLE block, short pages silently un-center (content strands at one edge
// with a void below). These tests pin the contract in plain source terms so
// any edit that breaks it fails here instead of on a user's screen.

use std::path::PathBuf;

fn ui_file(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("ui");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("missing ui/{name}"))
}

/// Strip ALL whitespace so selector formatting (`.a> :b` vs `.a > :b`,
/// newlines, indentation) can never fake a pass or a fail.
fn flat(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn css() -> String {
    flat(&ui_file("styles.css"))
}

/// True when any rule containing `selector` (standalone or grouped, e.g.
/// `a, b { ... }`) also contains `decl`. A plain substring check fails on
/// grouped selectors, where `{` follows the last selector only.
fn rule_has(css: &str, selector: &str, decl: &str) -> bool {
    let mut from = 0;
    while let Some(i) = css[from..].find(selector) {
        let start = from + i;
        match css[start..].find('}') {
            Some(end) if css[start..start + end].contains(decl) => return true,
            Some(_) => from = start + selector.len(),
            None => return false,
        }
    }
    false
}

/// The base centering mechanism must exist: first child pushes from the
/// top, last child pushes from the bottom.
#[test]
fn base_screens_center_via_auto_margins() {
    let css = css();
    assert!(
        css.contains(".screen>:first-child{margin-top:auto;}"),
        "base top-centering rule gone — short pages strand at the top"
    );
    assert!(
        css.contains(".screen>:last-child{margin-bottom:auto;}"),
        "base bottom-centering rule gone — short pages strand at the top"
    );
}

/// Menu and Notifications heads share one pinned top offset, so the two
/// titles render at exactly the same (x, y): same DOM order (head first),
/// same .page-head/.page-title rules, same top offset. Settings is headless
/// and keeps group centering — it must never gain a head pin.
#[test]
fn overlay_heads_share_one_top_offset() {
    let css = css();
    for sel in [
        "#screen-menu.page-head",
        "#screen-notifications.page-head",
    ] {
        assert!(
            rule_has(&css, sel, "margin-top:0;"),
            "{sel}: head pin missing — titles drift apart when body heights differ"
        );
    }
    assert!(
        !rule_has(&css, "#screen-settings.page-head", "margin-top:0"),
        "settings head pinned — the headless page loses group centering"
    );
}

/// The bottom auto margin must sit on the last VISIBLE block of each
/// overlay page — never (only) on the hidden Back link. display:none boxes
/// cannot absorb free space.
#[test]
fn overlay_pages_center_on_visible_content() {
    let css = css();
    for (page, last_visible) in [
        ("menu", "#screen-menu.menugrid"),
        ("notifications-empty", "#notif-empty"),
        ("notifications-list", "#screen-notifications#notif-list"),
        ("settings", "#screen-settings.profile-card"),
    ] {
        assert!(
            rule_has(&css, last_visible, "margin-bottom:auto;"),
            "{page}: bottom centering margin missing on the visible block — \
             the page will strand at one edge"
        );
    }
}
