// chat_state.rs — pure Chat backend + Roles + LPARAM math.
// No window, no network, no registry: safe on any dev machine.
// Content editing/selection now lives in the WebView2 page; Rust keeps
// the API key, history, the live stream and status lines.

use notchbud::connector::auth::ApiKey;
use notchbud::connector::chat::Role;
use notchbud::state::{client_pt, screen_pt, App, Chat};
use windows::Win32::Foundation::{HWND, LPARAM};

fn key() -> ApiKey {
    ApiKey::new("gsk_test1234567890abcdef").unwrap()
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

// --- history cap: newest 8 turns survive ---

#[test]
fn push_history_caps_at_eight() {
    let mut c = Chat::new();
    for i in 0..10 {
        c.push_history(Role::User, format!("m{i}"));
    }
    assert_eq!(c.history.len(), 8, "HISTORY_MAX must hold");
    assert_eq!(c.history[0].1, "m2", "oldest two turns evicted");
    assert_eq!(c.history[7].1, "m9");
}

// --- sign-in pill matrix ---

#[test]
fn signin_pill_states() {
    let mut c = Chat::new();
    assert!(c.shows_signin_button());

    c.signing_in = true;
    assert!(!c.shows_signin_button());

    c.signing_in = false;
    c.error = Some("boom".into());
    assert!(c.shows_signin_button());
    assert!(c.status_text().contains("boom"), "error stays visible");
}

// --- status text: notice + error ---

#[test]
fn status_text_branches() {
    // notice + error both surface, notice first
    let mut c = Chat::new();
    c.tokens = Some(key());
    c.notice = Some("n1".into());
    c.error = Some("e1".into());
    let t = c.status_text();
    assert!(t.find("n1") < t.find("e1"));
}

// --- ui_state: the JSON the page renders ---

#[test]
fn ui_state_carries_history_and_stream() {
    let mut c = Chat::new();
    c.tokens = Some(key());
    c.push_history(Role::User, "hi".into());
    c.push_history(Role::Assistant, "hey".into());
    c.streaming = true;
    c.stream = "half…".into();
    let v = c.ui_state("Good evening");
    assert_eq!(v["greeting"], "Good evening");
    assert_eq!(v["streaming"], true);
    assert_eq!(v["history"][0]["role"], "user");
    assert_eq!(v["history"][0]["text"], "hi");
    assert_eq!(v["history"][1]["text"], "hey");
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
