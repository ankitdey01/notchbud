// auth_math.rs — pure auth math: refresh timing, plan gate, constants.
// No network, no DPAPI, no %APPDATA% writes: safe on any dev machine.
// (Deliberately NOT tested here: save/load/delete_tokens touch the real
// login at %APPDATA%\notchbud\auth.bin, and refresh/list/revoke hit the
// network. Those stay manual.)

use std::time::{SystemTime, UNIX_EPOCH};

use notchbud::connector::auth::{
    Tokens, PLAN_SCOPE, REFRESH_MARGIN_SECS, USER_AGENT,
};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn toks(expires_in_secs: i64, plan_allowed: bool, access: &str, refresh: &str) -> Tokens {
    Tokens {
        access: access.into(),
        refresh: refresh.into(),
        id_token: "id".into(),
        expires_at: (now_secs() as i64 + expires_in_secs) as u64,
        client_id: "oaiapp_test".into(),
        subject: "sub".into(),
        email: None,
        scopes: vec![PLAN_SCOPE.into()],
        plan_allowed,
        host_id: "host".into(),
    }
}

#[test]
fn refresh_margin_is_five_minutes() {
    assert_eq!(REFRESH_MARGIN_SECS, 5 * 60, "contract with the comment in auth.rs");
}

#[test]
fn needs_refresh_boundaries() {
    use notchbud::connector::auth::needs_refresh;
    // expired / nearly expired -> refresh (tick-safe: far from the edge)
    assert!(needs_refresh(&toks(-5, true, "a", "r")));
    assert!(needs_refresh(&toks(0, true, "a", "r")));
    assert!(needs_refresh(&toks(299, true, "a", "r")));
    // comfortably fresh -> no refresh (310s survives any second-tick)
    assert!(!needs_refresh(&toks(310, true, "a", "r")));
    assert!(!needs_refresh(&toks(3600, true, "a", "r")));
}

#[test]
fn plan_ready_gate() {
    let good = toks(3600, true, "a", "r");
    assert!(good.plan_ready());
    assert!(!toks(3600, false, "a", "r").plan_ready(), "no plan scope");
    assert!(!toks(3600, true, "", "r").plan_ready(), "empty access");
    assert!(!toks(3600, true, "a", "").plan_ready(), "empty refresh");
}

#[test]
fn identity_consts() {
    assert_eq!(PLAN_SCOPE, "chatgpt.tokens.use.direct");
    assert!(
        USER_AGENT.starts_with("notchbud/"),
        "server-side originator tag rides on this"
    );
}
