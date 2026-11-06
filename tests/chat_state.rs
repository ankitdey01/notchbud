// chat_state.rs — pure Chat state-machine + Roles + LPARAM math.
// No window, no network, no registry: safe on any dev machine.

use std::time::{SystemTime, UNIX_EPOCH};

use notchbud::connector::auth::{Tokens, PLAN_SCOPE};
use notchbud::connector::chat::Role;
use notchbud::state::{
    client_pt, field_shown, screen_pt, utf16_byte_range, utf16_len, utf16_slice, App, Chat,
    FieldSel,
};
use windows::Win32::Foundation::{HWND, LPARAM};

fn toks(expires_in_secs: i64, plan_allowed: bool) -> Tokens {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Tokens {
        access: "acc".into(),
        refresh: "ref".into(),
        id_token: "id".into(),
        expires_at: (now as i64 + expires_in_secs) as u64,
        client_id: "oaiapp_test".into(),
        subject: "sub".into(),
        email: None,
        scopes: vec![PLAN_SCOPE.into()],
        plan_allowed,
        host_id: "host".into(),
    }
}

// --- Role codes (persisted in history; must stay stable) ---

#[test]
fn role_codes_round_trip() {
    assert_eq!(Role::User.code(), 0);
    assert_eq!(Role::Assistant.code(), 1);
    assert!(Role::from_code(0) == Some(Role::User));
    assert!(Role::from_code(1) == Some(Role::Assistant));
    assert!(Role::from_code(2).is_none());
    assert!(Role::from_code(255).is_none());
}

// --- history cap: newest 8 turns survive, view snaps to bottom ---

#[test]
fn push_history_caps_at_eight_and_resets_scroll() {
    let mut c = Chat::new();
    c.scroll = 120.0;
    for i in 0..10 {
        c.push_history(Role::User, format!("m{i}"));
    }
    assert_eq!(c.history.len(), 8, "HISTORY_MAX must hold");
    assert_eq!(c.history[0].1, "m2", "oldest two turns evicted");
    assert_eq!(c.history[7].1, "m9");
    assert_eq!(c.scroll, 0.0, "new turn snaps back to newest line");
}

// --- sign-in pill matrix ---

#[test]
fn signin_pill_states() {
    let mut c = Chat::new();
    assert!(c.shows_signin_button());
    assert_eq!(c.slot_text(), "Sign in with ChatGPT");

    c.signing_in = true;
    assert!(!c.shows_signin_button());

    c.signing_in = false;
    c.error = Some("boom".into());
    assert!(c.shows_signin_button());
    let t = c.slot_text();
    assert!(t.contains("boom"), "error stays visible");
    assert!(t.contains("Sign in with ChatGPT"), "CTA under status");

    // connected: no pill, input line shown
    c.error = None;
    c.tokens = Some(toks(3600, true));
    assert!(!c.shows_signin_button());
    assert!(c.slot_text().contains("›"), "input caret line shown");
}

// --- slot text: plan gate, history, streaming ---

#[test]
fn slot_text_branches() {
    // identity-only sign-in (no plan scope) nags exactly once
    let mut c = Chat::new();
    c.tokens = Some(toks(3600, false));
    assert!(c.slot_text().contains("isn't enabled"));

    // history renders user turns with › and assistant plain
    c.tokens = Some(toks(3600, true));
    c.push_history(Role::User, "hi".into());
    c.push_history(Role::Assistant, "hey".into());
    let t = c.slot_text();
    assert!(t.contains("› hi"));
    assert!(t.contains("hey"));

    // streaming text shows, input line hides while streaming
    c.streaming = true;
    c.stream = "half…".into();
    let t = c.slot_text();
    assert!(t.contains("half…"));
    assert!(!t.contains("▏"), "no caret while streaming");

    // notice + error both surface, notice first
    c.streaming = false;
    c.stream.clear();
    c.notice = Some("n1".into());
    c.error = Some("e1".into());
    let t = c.slot_text();
    assert!(t.find("n1") < t.find("e1"));
}

// --- LPARAM packing: low word x, high word y, both signed 16-bit ---

fn pack(x: i32, y: i32) -> LPARAM {
    LPARAM((((y as u32) << 16) | (x as u32 & 0xFFFF)) as isize)
}

#[test]
fn client_pt_sign_extends() {
    let p = client_pt(pack(1, 2));
    assert_eq!((p.x, p.y), (1, 2));
    let p = client_pt(pack(-1, -2));
    assert_eq!((p.x, p.y), (-1, -2));
    let p = client_pt(pack(32767, -32768));
    assert_eq!((p.x, p.y), (32767, -32768));
}

#[test]
fn screen_pt_subtracts_window_origin() {
    let mut app = App::new(HWND(std::ptr::null_mut()), 1.0, 640, 190);
    app.win_x = 10;
    app.win_y = 20;
    let p = screen_pt(&app, pack(15, 25));
    assert_eq!((p.x, p.y), (5, 5));
}

// --- selectable transcript: UTF-16 helpers (DirectWrite units) ---

#[test]
fn utf16_helpers_count_and_slice() {
    assert_eq!(utf16_len("hi"), 2);
    assert_eq!(utf16_len("› hi"), 4); // › is a single unit
    assert_eq!(utf16_len("a😀b"), 4); // emoji is a surrogate pair
    assert_eq!(utf16_slice("› hello", 0, 2), "› ");
    assert_eq!(utf16_slice("a😀b", 1, 3), "😀");
    assert_eq!(utf16_slice("abc", 1, 99), "bc"); // end clamps
    assert_eq!(utf16_slice("abc", 2, 2), "");
}

#[test]
fn viewport_text_combines_status_history_stream() {
    let mut c = Chat::new();
    c.tokens = Some(toks(3600, true));
    c.push_history(Role::User, "hi".into());
    c.push_history(Role::Assistant, "hey".into());
    let t = c.viewport_text();
    assert!(t.contains("› hi"), "{t}");
    assert!(t.contains("hey"), "{t}");
    // error status rides above the conversation
    c.error = Some("e1".into());
    let t = c.viewport_text();
    assert!(t.starts_with("e1"), "{t}");
    assert!(t.contains("› hi"), "{t}");
}

#[test]
fn selection_copy_prefers_highlight() {
    let mut c = Chat::new();
    c.tokens = Some(toks(3600, true));
    c.push_history(Role::User, "hello".into());
    c.push_history(Role::Assistant, "world".into());
    // no highlight: Ctrl+C takes the whole transcript
    let full = c.viewport_text();
    assert_eq!(c.copy_text(), full);
    // a highlight wins
    let n = utf16_len(&full);
    assert!(n > 4);
    c.selection = Some((0, 4));
    assert_eq!(c.copy_text(), utf16_slice(&full, 0, 4));
    // reversed + overlong ends clamp instead of panicking
    c.selection = Some((99, 2));
    assert_eq!(c.selected_text().unwrap(), utf16_slice(&full, 2, 99));
    c.clear_selection();
    assert!(c.selected_text().is_none());
    assert_eq!(c.copy_text(), c.viewport_text());
}

#[test]
fn select_all_covers_viewport() {
    let mut c = Chat::new();
    assert!(c.viewport_text().is_empty());
    c.select_all();
    assert!(c.selection.is_none(), "nothing to highlight");
    c.tokens = Some(toks(3600, true));
    c.push_history(Role::User, "hi".into());
    c.select_all();
    let (a, b) = c.selection.unwrap();
    assert_eq!((a, b), (0, utf16_len(&c.viewport_text())));
    assert_eq!(c.selected_text().unwrap(), c.viewport_text());
}

// --- input field selection + edit ops (UTF-16 offsets, DirectWrite units) ---

#[test]
fn utf16_byte_range_maps_emoji() {
    assert_eq!(utf16_byte_range("a😀b", 0, 1), (0, 1));
    assert_eq!(utf16_byte_range("a😀b", 1, 3), (1, 5)); // surrogate pair stays whole
    assert_eq!(utf16_byte_range("a😀b", 0, 99), (0, 6));
    assert_eq!(utf16_byte_range("", 0, 5), (0, 0));
}

#[test]
fn field_shown_decorates() {
    assert_eq!(field_shown("", "› ", "Ask anything…"), "Ask anything…▏");
    assert_eq!(field_shown("hi", "› ", "Ask anything…"), "› hi▏");
    assert_eq!(field_shown("Al", "", "Type your name…"), "Al▏");
}

#[test]
fn field_select_all_and_selected() {
    let mut sel = FieldSel::default();
    assert!(sel.selected("hi").is_none());
    sel.select_all("hi");
    assert_eq!(sel.range, Some((0, 2)));
    assert_eq!(sel.selected("hi").unwrap(), "hi");
    sel.select_all("");
    assert!(sel.range.is_none(), "empty field clears");
}

#[test]
fn field_cut_deletes_highlight() {
    let mut text = "hello world".to_owned();
    let mut sel = FieldSel::default();
    // no highlight: nothing to cut
    assert!(sel.delete_selected(&mut text).is_none());
    assert_eq!(text, "hello world");
    sel.range = Some((6, 11));
    assert_eq!(sel.delete_selected(&mut text).unwrap(), "world");
    assert_eq!(text, "hello ");
    assert!(sel.range.is_none(), "cut clears the highlight");
}

#[test]
fn field_replace_respects_cap_and_filters_controls() {
    // type-over: highlight replaced, then cleared
    let mut text = "ab".to_owned();
    let mut sel = FieldSel::default();
    sel.range = Some((0, 2));
    sel.replace_selected(&mut text, "XYZ", 5);
    assert_eq!(text, "XYZ");
    assert!(sel.range.is_none());
    // cap counts the selection as freed room
    let mut text = "ab".to_owned();
    let mut sel = FieldSel::default();
    sel.range = Some((0, 2));
    sel.replace_selected(&mut text, "wxyz", 2);
    assert_eq!(text, "wx");
    // append path: control chars stripped, capped
    let mut text = "ab".to_owned();
    let mut sel = FieldSel::default();
    sel.replace_selected(&mut text, "c\nd", 3);
    assert_eq!(text, "abc");
    // reversed + overlong ranges clamp instead of panicking
    let mut text = "a😀b".to_owned();
    let mut sel = FieldSel::default();
    sel.range = Some((99, 1));
    assert_eq!(sel.delete_selected(&mut text).unwrap(), "😀b");
    assert_eq!(text, "a");
}
