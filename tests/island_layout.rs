// island_layout.rs — contract tests for the island page layout.
//
// The overlay pages (menu / notifications / settings) must sit vertically
// centered as a group. That centering is done with auto margins, and it is
// fragile in one specific way: the Back link is `display: none`, and auto
// margins on a display:none box absorb nothing — so if the bottom margin
// lands on the hidden Back link instead of the last VISIBLE block, short
// pages silently un-center (content strands at one edge with a void below).
// These tests pin the contract in plain source terms so any edit that
// breaks it fails here instead of on a user's screen.

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

/// No rule may zero the top margin of an overlay page's first block: that
/// was the compact-phase top-anchor override, and it un-centers menu /
/// notifications / settings outright.
#[test]
fn no_overlay_page_is_top_anchored() {
    let css = css();
    for sel in [
        ".page>:first-child{margin-top:0",
        "#screen-menu>:first-child{margin-top:0",
        "#screen-notifications>:first-child{margin-top:0",
        "#screen-settings>:first-child{margin-top:0",
    ] {
        assert!(
            !css.contains(sel),
            "top-anchor override present ({sel}) — overlay pages will hug the topbar"
        );
    }
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
