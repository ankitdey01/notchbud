// chat_state.rs — pure Chat state-machine + Roles + LPARAM math.
// No window, no network, no registry: safe on any dev machine.

use std::time::{SystemTime, UNIX_EPOCH};

use notchbud::connector::auth::{Tokens, PLAN_SCOPE};
use notchbud::connector::chat::Role;
use notchbud::state::{client_pt, screen_pt, App, Chat};
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
