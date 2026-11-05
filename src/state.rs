// state.rs — minimal notch state: idle sliver -> hover-expanding island.

use windows::Win32::Foundation::POINT;

pub const WINDOW_UNITS_W: f32 = 640.0;
pub const WINDOW_UNITS_H: f32 = 190.0;

// --- geometry constants, mirrored from Nusa at 1.25 DPI (1300px measured) ---
// Island core: 420x140.4 layout units (525x176 phys px), horizontally
// centered, flush with the screen top. The TOP corners are not square: each
// is a convex quarter-circle "swoop" whose center sits on the flat top edge,
// r = 16.8 units (21 phys px), flaring the silhouette 2x16.8 units WIDER at
// y=0 than the body. Bottom corners: quarter circles, r = 35.2 units (44 px).
pub const ISLAND_W: f32 = 420.0;
pub const ISLAND_H: f32 = 140.4;
pub const ISLAND_X: f32 = (WINDOW_UNITS_W - ISLAND_W) / 2.0;
pub const ISLAND_FLARE_R: f32 = 16.8; // top swoop radius (unit-scaled, not px-fixed)
pub const ISLAND_BOT_R: f32 = 35.2; // bottom corner radius

pub const SLIVER_W: f32 = 45.6; // idle tab 57x5 physical px
pub const SLIVER_H: f32 = 14.0; // hover strip height — matches Nusa's idle hit area
pub const SLIVER_X: f32 = (WINDOW_UNITS_W - SLIVER_W) / 2.0;

// hover swell targets: before the main spring fires, the sliver swells to
// 61.6x7.2 layout units over ~0.19 s (Nusa's two-stage wake).

// The whole motion is a SCALE of the fully-drawn island about the point
// (center-x, y=0) — matching Nusa, whose open overshoot was in the shell
// size itself (the silhouette visibly swelled to 605px before settling at
// 573px) — not a clip reveal. Scale factors are unit-relative (DPI-free):
//   idle:  the shell shrinks to the 45.6x4u tab
//   swell: the 61.6x7.2u wake state
//   open:  1.0 (full island)
pub const ISLAND_TOTAL_W: f32 = ISLAND_W + 2.0 * ISLAND_FLARE_R; // 453.6
pub const SCALE_IDLE_X: f32 = SLIVER_W / ISLAND_TOTAL_W;
pub const SCALE_IDLE_Y: f32 = 4.0 / ISLAND_H;
pub const SCALE_SWELL_X: f32 = 61.6 / ISLAND_TOTAL_W;
pub const SCALE_SWELL_Y: f32 = 7.2 / ISLAND_H;

pub const IDLE_RGN_HALF_W: f32 = 56.0; // 112u wide centered hover strip
pub const IDLE_RGN_H: f32 = 14.0; // region bottom edge (idle)

// ---------------------------------------------------------------------------
// Motion — springs as measured on Nusa's window (screen-capture profiling):
//
//  * idle -> open is two-stage:
//      stage 1  swell: the sliver 45.6x4 u -> 61.6x7.2 u over ~0.19 s,
//               underdamped, gentle (response 0.18, zeta 0.60)
//      stage 2  pop:   swell clip -> full island, underdamped spring with
//               zeta 0.65 (measured overshoot ~6%) and damped period
//               2*pi/13.5 rad/s; first peak 233 ms after stage-2 start,
//               settled ~0.62 s later. As (response, zeta): (0.35, 0.65)
//               — the SwiftUI parametric form, which reproduces it exactly.
//  * open -> idle: fast collapse (~240 ms), then a small bounce that mirrors
//               the open overshoot: the shell dips to CLOSE_DIP_FACTOR of the
//               idle tab and springs back, settling ~0.37 s end-to-end.
//               The dip is a scale factor on the idle tab (not travel %), so
//               both axes stay positive — a travel-% mirror of the open 6%
//               would drive the thin Y scale negative. Total close settle is
//               CLOSE_DELAY + CLOSE_SETTLE.
// ---------------------------------------------------------------------------

pub const OPEN_RESPONSE: f32 = 0.35;
pub const OPEN_ZETA: f32 = 0.65;
pub const SWELL_RESPONSE: f32 = 0.18;
pub const SWELL_ZETA: f32 = 0.60;
// Content choreography: on retract the greeting fades out over CLOSE_DELAY
// BEFORE the visibly shrinking curve begins (CLOSE_KNOTS t is shifted by
// this much); the reply on expand: the greeting fades in over CONTENT_FADE,
// only after the pop spring has fully settled at scale 1.0.
pub const CLOSE_DELAY: f32 = 0.10;
pub const CONTENT_FADE: f32 = 0.15;
pub const STAGE1_DUR: f32 = 0.19; // swell duration before the pop spring starts

// Close motion: Nusa's RETRACT, sampled from screen captures on this PC —
// a fast, near-symmetric collapse (~240 ms end-to-end, visible shrink begins
// ~35 ms after the leave), then a bounce dip that mirrors the open overshoot
// (the shell closes "too much" to CLOSE_DIP_FACTOR of the idle tab before
// springing back). Recognized curve: piecewise-linear through the measured
// frames for the collapse; velocity is the knot-slope, feeding Hermite.
// The bounce tail (collapse-end -> dip -> recover) is sampled as smooth
// Hermite ease segments in render.rs, so both axes stay positive.
pub const CLOSE_KNOTS: [(f32, f32); 10] = [
    (0.000, 1.0),
    (0.035, 0.979),
    (0.066, 0.808),
    (0.100, 0.594),
    (0.129, 0.420),
    (0.154, 0.283),
    (0.181, 0.168),
    (0.210, 0.087),
    (0.238, 0.0),
    (0.600, 0.0), // holds closed afterwards
];

pub fn close_progress(t: f32) -> f32 {
    let t = t.max(0.0);
    for w in CLOSE_KNOTS.windows(2) {
        if t < w[1].0 {
            let (t0, v0) = w[0];
            let (t1, v1) = w[1];
            return v0 + (v1 - v0) * (t - t0) / (t1 - t0);
        }
    }
    0.0
}

pub fn close_velocity(t: f32) -> f32 {
    let t = t.max(0.0);
    for w in CLOSE_KNOTS.windows(2) {
        if t < w[1].0 {
            let (t0, v0) = w[0];
            let (t1, v1) = w[1];
            return (v1 - v0) / (t1 - t0);
        }
    }
    0.0
}

pub const CLOSE_SETTLE: f32 = 0.368; // collapse + bounce dip + recover
pub const CLOSE_COLLAPSE_END: f32 = 0.238; // the measured collapse duration
pub const CLOSE_DIP_FACTOR: f32 = 0.65; // shell shrinks to 65% of the idle tab at the dip
pub const CLOSE_DIP_T: f32 = 0.283; // time of deepest dip (close clock, excl. CLOSE_DELAY)
pub const CLOSE_RECOVER_T: f32 = 0.368; // back at idle (== CLOSE_SETTLE)

/// Bounce multiplier on the idle tab for the close tail (close clock `t`
/// excludes CLOSE_DELAY): 1.0 through the collapse, dips linearly to
/// CLOSE_DIP_FACTOR at CLOSE_DIP_T, recovers linearly to 1.0 at
/// CLOSE_RECOVER_T, holds 1.0 after. Linear like close_progress — the
/// compositor tail eases it with Hermite; this is the reversal estimate.
pub fn close_bounce_factor(t: f32) -> f32 {
    if t <= CLOSE_COLLAPSE_END || t >= CLOSE_RECOVER_T {
        1.0
    } else if t < CLOSE_DIP_T {
        1.0 + (CLOSE_DIP_FACTOR - 1.0) * (t - CLOSE_COLLAPSE_END)
            / (CLOSE_DIP_T - CLOSE_COLLAPSE_END)
    } else {
        CLOSE_DIP_FACTOR
            + (1.0 - CLOSE_DIP_FACTOR) * (t - CLOSE_DIP_T)
                / (CLOSE_RECOVER_T - CLOSE_DIP_T)
    }
}

/// Time from the pop-spring start (after stage 1) until the island sits at
/// scale 1.0 — the moment the greeting content is allowed to reveal.
pub fn open_settle() -> f32 {
    STAGE1_DUR + spring_settle(OPEN_RESPONSE, OPEN_ZETA)
}

/// Normalized progress toward the target at time `t` for a parametric spring
/// with the given response and damping fraction (critically damped at 1.0).
pub fn spring_progress(response: f32, zeta: f32, t: f32) -> f32 {
    let t = t.max(0.0);
    let wn = std::f32::consts::TAU / response;
    if zeta >= 0.999 {
        let x = wn * t;
        return 1.0 - (1.0 + x) * (-x).exp();
    }
    let sigma = zeta * wn;
    let wd = wn * (1.0 - zeta * zeta).sqrt();
    1.0 - (-sigma * t).exp() * ((wd * t).cos() + (sigma / wd) * (wd * t).sin())
}

/// Velocity (derivative of `spring_progress`) at time `t`.
pub fn spring_velocity(response: f32, zeta: f32, t: f32) -> f32 {
    let t = t.max(0.0);
    let wn = std::f32::consts::TAU / response;
    if zeta >= 0.999 {
        let x = wn * t;
        let tail = (-x).exp();
        return wn * wn * t * tail;
    }
    let sigma = zeta * wn;
    let wd = wn * (1.0 - zeta * zeta).sqrt();
    (-sigma * t).exp() * (wd * t).sin() * (wd + sigma * sigma / wd)
}

/// Time until the spring's remaining trajectory stays under 0.1%.
pub fn spring_settle(response: f32, zeta: f32) -> f32 {
    let wn = std::f32::consts::TAU / response;
    let sigma = zeta * wn;
    if zeta >= 0.999 {
        // (1+x)e^{-x} = 1e-3 at x ≈ 9.2337
        9.2337 / sigma
    } else {
        // envelope c·e^{-σt} = 1e-3
        let wd = wn * (1.0 - zeta * zeta).sqrt();
        let c = (1.0 + (sigma / wd) * (sigma / wd)).sqrt();
        sigma.recip() * (c * 1000.0f32).ln()
    }
}

// Visual scale targets for the three states.
pub fn scale_idle() -> [f32; 2] {
    [SCALE_IDLE_X, SCALE_IDLE_Y]
}
pub fn scale_swell() -> [f32; 2] {
    [SCALE_SWELL_X, SCALE_SWELL_Y]
}

// ---------------------------------------------------------------------------
// Chat state — everything the streaming panel inside the frozen island needs.
// Geometry, springs, scale choreography above are untouched.
// ---------------------------------------------------------------------------

pub use crate::connector::chat::{Msg, Role, StreamEvent};

/// History cap: 4 turns back is all that shows and all the slot needs.
const HISTORY_MAX: usize = 8;

/// Which island section owns the frame. Every visual state maps to exactly
/// one of these — no overlaps, no bare fallbacks:
/// * Onboarding — no saved name yet: "What should we call you?" + name field.
///   Enter submits; there is no button.
/// * SignIn — name known but not connected: status + the "Sign in with
///   ChatGPT" pill, exactly like before. No greeting, no chat field.
/// * Home — signed in, no chat yet: time-based greeting + chat field.
/// * Chat — signed in and chatting: history/stream + chat field pinned down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Onboarding,
    SignIn,
    Home,
    Chat,
}

pub struct Chat {
    /// The saved ChatGPT sign-in; None = not connected.
    pub tokens: Option<crate::connector::auth::Tokens>,
    /// Resolved plan model from the account catalog, cached per sign-in.
    pub model: Option<String>,
    pub input: String,
    /// Assistant text currently arriving.
    pub stream: String,
    /// Finished turns, oldest first.
    pub history: Vec<(Role, String)>,
    pub streaming: bool,
    /// Browser sign-in in progress (tray Connect clicked).
    pub signing_in: bool,
    /// Transient notice for the slot (e.g. "paste the approval link").
    /// Never a credential. Cleared on connect/fail/disconnect.
    pub notice: Option<String>,
    /// One-line error/status for the slot.
    pub error: Option<String>,
    /// Scroll offset in px; 0 = pinned to the newest line (bottom-anchored),
    /// larger values reveal older lines. Wheel hook adjusts, draw clamps.
    pub scroll: f32,
    /// Content height beyond the island viewport; refreshed every frame.
    pub max_scroll: f32,
}

impl Chat {
    pub fn new() -> Self {
        Chat {
            tokens: None,
            model: None,
            input: String::new(),
            stream: String::new(),
            history: Vec::new(),
            streaming: false,
            signing_in: false,
            notice: None,
            error: None,
            scroll: 0.0,
            max_scroll: 0.0,
        }
    }

    pub fn connected(&self) -> bool {
        self.tokens.is_some()
    }

    /// Signed in AND authorized for ChatGPT-plan inference.
    pub fn plan_ready(&self) -> bool {
        self.tokens.as_ref().is_some_and(|t| t.plan_ready())
    }

    pub fn push_history(&mut self, role: Role, text: String) {
        if self.history.len() >= HISTORY_MAX {
            self.history.remove(0);
        }
        self.history.push((role, text));
        self.scroll = 0.0; // a new turn snaps the view back to the newest line
    }

    /// True once a conversation owns the island: finished turns, a live
    /// stream, or buffered stream text. The greeting shows only before this.
    pub fn has_chatted(&self) -> bool {
        !self.history.is_empty() || self.streaming || !self.stream.is_empty()
    }

    /// True when the island should show the sign-in pill button.
    /// That's every signed-out idle state except the transient
    /// "Connecting…" wait — the click handler uses the same gate.
    pub fn shows_signin_button(&self) -> bool {
        !self.connected() && !self.signing_in
    }

    /// The text shown in the slot when the island is open, ready for
    /// render::draw_content.
    pub fn slot_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if let Some(n) = &self.notice {
            lines.push(n.clone());
        }
        if let Some(e) = &self.error {
            lines.push(e.clone());
        }
        // Identity-only sign-in (no plan scope): inference stays off until
        // the user authorizes plan usage via Connect.
        if self.connected() && !self.plan_ready() && self.error.is_none() {
            lines.push("ChatGPT plan use isn't enabled — Connect again to authorize it.".into());
        }
        for (role, text) in &self.history {
            match role {
                Role::User => lines.push(format!("› {text}")),
                Role::Assistant => lines.push(text.clone()),
            }
        }
        if self.streaming && !self.stream.is_empty() {
            lines.push(self.stream.clone());
        }
        if self.connected() && !self.streaming {
            lines.push(format!("› {}▏", self.input));
        }
        if lines.is_empty() {
            if self.signing_in {
                "Connecting to ChatGPT…".into()
            } else if !self.connected() {
                "Sign in with ChatGPT".into()
            } else {
                // Connected with nothing to show yet — normally unreachable
                // (the input line above covers it); kept as a safe fallback.
                format!("› {}▏", self.input)
            }
        } else {
            // Signed-out error states (e.g. "Disconnected…", expired session)
            // still offer the next action underneath the status line.
            if !self.connected() && !self.signing_in {
                lines.push("Sign in with ChatGPT".into());
            }
            lines.join("\n")
        }
    }
}

pub struct App {
    pub hwnd: windows::Win32::Foundation::HWND,
    pub scale: f32,
    pub width: i32,
    pub height: i32,

    pub expanded: bool, // false = idle sliver, true = island shown
    pub mouse_inside: bool,

    // island chat (frozen slot content + history); tokens live here too
    pub chat: Chat,

    // first-run identity: None until onboarding saves it (persists at
    // %APPDATA%\notchbud\user.json). While None the island shows the name
    // prompt instead of chat/sign-in.
    pub user_name: Option<String>,
    /// Raw onboarding draft (what the name field currently holds).
    pub name_draft: String,
    /// Why the last name submit was rejected (shown under the field).
    pub name_error: Option<String>,

    // last-frame pill bounds in window px (for click routing). Refreshed by
    // every draw; None when the pill isn't on screen.
    pub signin_rect: Option<[f32; 4]>,

    // geometry snapshot refreshed each frame (NCHITTEST hit surface)
    pub bar_area: [f32; 4],
    pub rgn_area: [f32; 4],

    // window origin in screen px, cached so the hit-test never calls
    // GetWindowRect — seeded at startup, refreshed on WM_DPICHANGED reflow
    pub win_x: i32,
    pub win_y: i32,

    // remaining repaint steps of the active content-fade pump (WM_TIMER)
    pub pump_steps: usize,
}

impl App {
    pub fn new(
        hwnd: windows::Win32::Foundation::HWND,
        scale: f32,
        width: i32,
        height: i32,
    ) -> Self {
        App {
            hwnd,
            scale,
            width,
            height,
            expanded: false,
            mouse_inside: false,
            chat: Chat::new(),
            user_name: None,
            name_draft: String::new(),
            name_error: None,
            signin_rect: None,
            bar_area: [0.0; 4],
            rgn_area: [0.0; 4],
            win_x: 0,
            win_y: 0,
            pump_steps: 0,
        }
    }

    /// No usable saved name -> the island must ask for it. A missing file,
    /// a corrupt file, and a blank name all land here.
    pub fn needs_onboarding(&self) -> bool {
        self.user_name
            .as_ref()
            .is_none_or(|n| n.trim().is_empty())
    }

    /// Greeting owns the top of the island only once signed in and before
    /// any chat does — the first submitted prompt hides it for the session.
    pub fn shows_greeting(&self) -> bool {
        !self.needs_onboarding() && self.chat.connected() && !self.chat.has_chatted()
    }

    /// Which section owns this frame (see Screen). The greeting and chat
    /// field wait behind the sign-in: after the name, a signed-out island
    /// shows only the sign-in prompt, exactly like before.
    pub fn screen(&self) -> Screen {
        if self.needs_onboarding() {
            Screen::Onboarding
        } else if !self.chat.connected() {
            Screen::SignIn
        } else if self.chat.has_chatted() {
            Screen::Chat
        } else {
            Screen::Home
        }
    }

    /// "Good evening, Ankit" for right now; empty when onboarding.
    pub fn greeting(&self) -> String {
        match self.user_name.as_deref() {
            Some(n) if !n.trim().is_empty() => crate::user::greeting_now(n),
            _ => String::new(),
        }
    }

    /// Point-in-rect for pill click routing (window px, same space as the
    /// rects draw_content publishes).
    pub fn hit(rect: &[f32; 4], x: f32, y: f32) -> bool {
        x >= rect[0] && x < rect[2] && y >= rect[1] && y < rect[3]
    }

    pub fn layout_size(&self) -> (f32, f32) {
        (self.scale * WINDOW_UNITS_W, self.scale * WINDOW_UNITS_H)
    }
}

// lParam of a CLIENT-area mouse message (WM_MOUSEMOVE, WM_LBUTTONUP, ...):
// already client px. WM_NCHITTEST lParam is SCREEN px — use screen_pt for it.
pub fn client_pt(lparam: windows::Win32::Foundation::LPARAM) -> POINT {
    let sx = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
    let sy = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    POINT { x: sx, y: sy }
}

/// lParam of WM_NCHITTEST: screen px — subtract the cached window origin.
pub fn screen_pt(app: &App, lparam: windows::Win32::Foundation::LPARAM) -> POINT {
    let mut p = client_pt(lparam);
    p.x -= app.win_x;
    p.y -= app.win_y;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_in_hides_the_pill_and_failures_restore_it() {
        let mut c = Chat::new();
        // idle: pill only
        assert!(c.shows_signin_button());
        assert_eq!(c.slot_text(), "Sign in with ChatGPT");
        // in flight: connecting text, no pill
        c.signing_in = true;
        c.notice = Some("Approve in your browser to finish signing in.".into());
        let t = c.slot_text();
        assert!(t.contains("Approve in your browser"));
        assert!(!t.contains("Sign in with ChatGPT"));
        assert!(!c.shows_signin_button());
        // failure clears back to the pill
        c.signing_in = false;
        c.notice = None;
        c.error = Some("That approval already expired. Connect again.".into());
        assert!(c.shows_signin_button());
        assert!(c.slot_text().contains("Sign in with ChatGPT"));
    }

    fn blank_app() -> App {
        App::new(
            windows::Win32::Foundation::HWND(std::ptr::null_mut()),
            1.0,
            640,
            190,
        )
    }

    /// Test-only sign-in: real tokens come from the browser flow, but the
    /// routing below only needs `connected()` to be true.
    fn sign_in(app: &mut App) {
        app.chat.tokens = Some(crate::connector::auth::Tokens {
            access: "a".into(),
            refresh: "r".into(),
            id_token: "i".into(),
            expires_at: u64::MAX,
            client_id: "c".into(),
            subject: "s".into(),
            email: None,
            scopes: vec![crate::connector::auth::PLAN_SCOPE.into()],
            plan_allowed: true,
            host_id: "h".into(),
        });
    }

    #[test]
    fn onboarding_until_a_name_is_saved() {
        let mut app = blank_app();
        assert!(app.needs_onboarding());
        assert_eq!(app.screen(), Screen::Onboarding);
        assert!(!app.shows_greeting());
        app.user_name = Some("   ".into());
        assert!(app.needs_onboarding());
        app.user_name = Some("Ankit".into());
        assert!(!app.needs_onboarding());
        // Named but signed out: only the sign-in prompt, no greeting.
        assert_eq!(app.screen(), Screen::SignIn);
        assert!(!app.shows_greeting());
        // Greeting waits for the sign-in.
        sign_in(&mut app);
        assert_eq!(app.screen(), Screen::Home);
        assert!(app.shows_greeting());
    }

    #[test]
    fn first_chat_hides_the_greeting() {
        let mut app = blank_app();
        app.user_name = Some("Ankit".into());
        sign_in(&mut app);
        assert_eq!(app.screen(), Screen::Home);
        app.chat.push_history(Role::User, "hello who are you".into());
        assert!(app.chat.has_chatted());
        assert!(!app.shows_greeting());
        assert_eq!(app.screen(), Screen::Chat);
    }

    #[test]
    fn streaming_counts_as_chatted() {
        let mut app = blank_app();
        app.user_name = Some("Ankit".into());
        sign_in(&mut app);
        app.chat.streaming = true;
        app.chat.stream = "Hi".into();
        assert!(app.chat.has_chatted());
        assert_eq!(app.screen(), Screen::Chat);
    }

    #[test]
    fn signed_out_with_history_stays_on_signin() {
        // An expired grant mid-chat keeps its history but routes back to
        // the sign-in prompt (which shows the history above its pill).
        let mut app = blank_app();
        app.user_name = Some("Ankit".into());
        sign_in(&mut app);
        app.chat.push_history(Role::User, "hi".into());
        assert_eq!(app.screen(), Screen::Chat);
        app.chat.tokens = None;
        assert_eq!(app.screen(), Screen::SignIn);
    }
}
