// island_layout.rs — contract tests for the island page layout.
//
// All four overlay pages (menu / chats / notifications / settings) are
// headless:
// the highlighted topbar icon already says where you are, so no title ever
// paints. Each page shows one dotted panel that FILLS everything between
// the navbar and the island bottom (flex:1 + min-height:0) instead of
// centering as a compact block. That fill is fragile in one specific way:
// the Back link is `display: none`, and a display:none box neither fills
// nor scrolls — so if the fill lands on the hidden Back link instead of
// the VISIBLE panel, short pages silently collapse (empty dotted box, or
// no box at all). These tests pin the contract in plain source terms so
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

/// All four overlay pages are headless: the Menu / Chats / Notifications /
 // Settings heads stay in the DOM but must never paint — the active topbar
// icon already says where you are.
#[test]
fn overlay_pages_are_headless() {
    let css = css();
    for sel in [
        "#screen-menu.page-head",
        "#screen-chats.page-head",
        "#screen-notifications.page-head",
        "#screen-settings.page-head",
    ] {
        assert!(
            rule_has(&css, sel, "display:none;"),
            "{sel}: head must stay hidden — the overlay icon already says where you are"
        );
    }
}

/// Each overlay page's VISIBLE panel must fill the navbar-to-shell space
/// (flex:1, never collapsing) and wear the shared dotted border — never
/// (only) the hidden Back link. display:none boxes neither fill nor show
/// a border.
#[test]
fn overlay_pages_fill_with_dotted_panels() {
    let css = css();
    for (page, panel) in [
        ("menu", "#screen-menu.menugrid"),
        ("chats-empty", "#chats-empty"),
        ("chats-list", "#screen-chats#chats-list"),
        ("notifications-empty", "#notif-empty"),
        ("notifications-list", "#screen-notifications#notif-list"),
        ("settings", ".profile-card"),
    ] {
        assert!(
            rule_has(&css, panel, "flex:1;"),
            "{page}: fill missing on the visible panel — \
             the dotted box collapses instead of reaching the edges"
        );
        assert!(
            rule_has(&css, panel, "border:1pxdashedrgba(255,255,255,0.14);"),
            "{page}: dotted border missing on the visible panel"
        );
        assert!(
            rule_has(&css, panel, "border-radius:24px;"),
            "{page}: panel corners must be rounded 24px all around"
        );
    }
}

/// Menu tiles ride a 2x2 grid inside the dotted panel, and the panel
/// scrolls instead of clipping so wrapped rows stay reachable.
#[test]
fn menu_tiles_form_two_by_two_grid() {
    let css = css();
    assert!(
        rule_has(&css, ".menugrid", "grid-template-columns:1fr1fr;"),
        "menugrid is not 2x2 — tiles must sit two per row inside the panel"
    );
    assert!(
        rule_has(&css, ".menugrid", "overflow-y:auto;"),
        "menugrid clips instead of scrolling — wrapped rows go unreachable"
    );
}

/// Button bounce must never rise above full size: the release leg (base
/// .btn transition) is a plain ease-out, so letting go glides 0.93 -> 1
/// with no overshoot kick. The springy dip lives only on :active.
#[test]
fn button_release_has_no_overshoot() {
    let css = css();
    assert!(
        rule_has(&css, ".btn", "transform180msease-out;"),
        "button release must ease out exactly to full size"
    );
    assert!(
        !rule_has(&css, ".btn{", "cubic-bezier"),
        "base button transition must not overshoot above full size"
    );
}

/// Chat history must bottom-anchor with margin-top:auto on the first
/// turn — never justify-content:flex-end, which lays overflowing turns
/// out above the box where no scrollbar can reach them (early turns go
/// permanently invisible in long chats while their boxes still overlap
/// the navbar region).
#[test]
fn history_top_stays_reachable() {
    let css = css();
    assert!(
        !rule_has(&css, ".history", "justify-content:flex-end"),
        "history pinned with flex-end — overflowing turns escape above the box, unreachable"
    );
    assert!(
        rule_has(&css, ".history>:first-child", "margin-top:auto;"),
        "history bottom anchor missing — short chats strand at the top"
    );
}

/// The greeting clips with overflow:hidden (single-line ellipsis), so its
/// line-height must clear descenders ("Good Evening" has two g's) on every
/// font — a tight factor cuts glyph tails in WebView2's Segoe UI.
#[test]
fn greeting_line_height_clears_descenders() {
    let css = css();
    assert!(
        rule_has(&css, ".greet-title", "line-height:normal;"),
        "greeting line-height too tight — descenders clip under overflow:hidden"
    );
}

/// The confirm-page chat name is single-line ellipsis gray text, so its
/// line-height needs the same descender headroom as the greeting — a
/// WebView2/Segoe `normal` box clips tails like the g in
/// "Brief Greeting Interaction" under overflow:hidden.
#[test]
fn confirm_name_line_height_clears_descenders() {
    let css = css();
    assert!(
        rule_has(&css, ".confirm-name", "line-height:1.4;"),
        "confirm name line-height too tight — title tails clip under overflow:hidden"
    );
}

/// First-run name prompt: the question "What should we call you?" is a
/// big white line ABOVE the input — the input itself shows an example
/// name, never the question.
#[test]
fn home_name_prompt_has_question_above_input() {
    let html = ui_file("index.html");
    let entry = html
        .find("id=\"name-entry\"")
        .and_then(|s| {
            html[s..]
                .find("id=\"home-ask\"")
                .map(|e| html[s..s + e].to_string())
        })
        .expect("name-entry must sit before the home chat box");
    let q = entry
        .find("What should we call you?")
        .expect("name question line missing from name-entry");
    let input = entry
        .find("id=\"home-name\"")
        .expect("name input missing from name-entry");
    assert!(
        q < input,
        "question must sit above the input, not inside it"
    );
    assert!(
        entry.contains("placeholder=\"Galaxy Destroyer\""),
        "name input must show an example name, not the question"
    );
    let css = css();
    assert!(
        rule_has(&css, ".name-q", "font-size:17px;"),
        "name question must be big (17px like greeting/page titles)"
    );
    assert!(
        rule_has(&css, ".name-q", "color:#fff;"),
        "name question must be white"
    );
    assert!(
        rule_has(&css, "#name-entry", "max-width:340px;"),
        "name box must not span the island — capped like the signin key entry"
    );
}

/// Chat owns every pixel for history: the back row floats over the feed
/// instead of consuming a top strip — in a 140-unit island a 28px in-flow
/// row is a whole lost line of history.
#[test]
fn chat_back_row_floats_off_the_layout() {
    let css = css();
    assert!(
        rule_has(&css, "#screen-chat", "position:relative;"),
        "chat section must anchor the floating back row"
    );
    assert!(
        rule_has(&css, ".chatnav", "position:absolute;"),
        "chat back row must float — an in-flow row is a black strip off the history"
    );
    assert!(
        rule_has(&css, "#screen-chat.history", "padding-top:30px;"),
        "history must start clear of the floating back button"
    );
}
