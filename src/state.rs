// state.rs — minimal notch state: idle sliver -> hover-expanding island.
//
// Shell motion (springs, scales) is unchanged. Content now lives in
// HTML/CSS inside WebView2: this file keeps only the backend state the
// page renders (greeting inputs come from the page, not from Rust edits).

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

// Close motion: Nusa's RETRACT, sampled from reference screen captures —
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

fn close_segment(t: f32) -> Option<(f32, f32, f32, f32)> {
    for w in CLOSE_KNOTS.windows(2) {
        if t < w[1].0 {
            return Some((w[0].0, w[0].1, w[1].0, w[1].1));
        }
    }
    None
}

pub fn close_progress(t: f32) -> f32 {
    let t = t.max(0.0);
    close_segment(t).map_or(0.0, |(t0, v0, t1, v1)| v0 + (v1 - v0) * (t - t0) / (t1 - t0))
}

pub fn close_velocity(t: f32) -> f32 {
    let t = t.max(0.0);
    close_segment(t).map_or(0.0, |(t0, v0, t1, v1)| (v1 - v0) / (t1 - t0))
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
// Chat state — backend only. Editing, selection, scrolling and all text
// measurement now live in the WebView2 page; Rust keeps tokens, history,
// the live stream and status lines, and pushes them as JSON.
// ---------------------------------------------------------------------------

pub use crate::connector::chat::{Msg, Role, StreamEvent};

/// History cap: 4 turns back is all that shows and all the slot needs.
const HISTORY_MAX: usize = 8;

/// Which island section owns the frame. Every visual state maps to exactly
/// one of these — no overlaps, no bare fallbacks:
/// * SignIn — not connected: status + the sign-in pill.
/// * Home — signed in, no chat yet: time-based greeting + chat field.
/// * Chat — signed in and chatting: history/stream + chat field pinned down.
///
/// Auth is not a separate screen: sign-in IS the auth page (nav target
/// "auth" resolves to it when signed out, to home/chat when signed in).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    SignIn,
    Home,
    Chat,
}

impl Screen {
    /// Wire string for `page()` / `page_state()["screen"]`. Single source so
    /// the two can't drift apart when a screen is added.
    pub fn as_str(self) -> &'static str {
        match self {
            Screen::SignIn => "signin",
            Screen::Home => "home",
            Screen::Chat => "chat",
        }
    }
}

/// Topbar overlay pages. These sit ON TOP of the base [`Screen`] without
/// destroying it — opening Settings from Home then going Back returns to
/// Home, not to a reset island. The user-facing pages are therefore:
/// home, chat, auth (signin), menu, chats, notifications, settings,
/// plus the delete confirmation that pops over the chats list.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Overlay {
    #[default]
    None,
    Menu,
    Chats,
    Notifications,
    Settings,
    ConfirmDelete,
}

impl Overlay {
    pub fn as_str(self) -> &'static str {
        match self {
            Overlay::None => "none",
            Overlay::Menu => "menu",
            Overlay::Chats => "chats",
            Overlay::Notifications => "notifications",
            Overlay::Settings => "settings",
            Overlay::ConfirmDelete => "confirm",
        }
    }

    /// Non-none overlays in wire order. `from_target` looks these up instead
    /// of re-listing the strings, so the two can't drift apart.
    pub const ALL: [Overlay; 5] = [
        Overlay::Menu,
        Overlay::Chats,
        Overlay::Notifications,
        Overlay::Settings,
        Overlay::ConfirmDelete,
    ];

    pub fn from_target(target: &str) -> Option<Overlay> {
        Self::ALL.into_iter().find(|o| o.as_str() == target)
    }
}

pub struct Chat {
    /// The saved Groq API key; None = not connected.
    pub tokens: Option<crate::connector::auth::ApiKey>,
    /// Resolved Groq model from the key's catalog, cached per key.
    pub model: Option<String>,
    /// Assistant text currently arriving.
    pub stream: String,
    /// Finished turns, oldest first.
    pub history: Vec<(Role, String, Option<String>)>,
    pub streaming: bool,
    /// Browser sign-in in progress (tray Connect clicked).
    pub signing_in: bool,
    /// Transient notice for the slot (e.g. "paste the approval link").
    /// Never a credential. Cleared on connect/fail/disconnect.
    pub notice: Option<String>,
    /// One-line error/status for the slot.
    pub error: Option<String>,
}

impl Chat {
    pub fn new() -> Self {
        Chat {
            tokens: None,
            model: None,
            stream: String::new(),
            history: Vec::new(),
            streaming: false,
            signing_in: false,
            notice: None,
            error: None,
        }
    }

    pub fn connected(&self) -> bool {
        self.tokens.is_some()
    }

    pub fn push_history(&mut self, role: Role, text: String) {
        self.push_history_image(role, text, String::new())
    }

    /// Push a user turn with its screenshot thumbnail: renders above the
    /// bubble (see renderChat). The full-size image is never stored —
    /// history keeps the cheap thumb only.
    pub fn push_history_image(&mut self, role: Role, text: String, thumb: String) {
        if self.history.len() >= HISTORY_MAX {
            self.history.remove(0);
        }
        let thumb = (!thumb.trim().is_empty()).then(|| thumb);
        self.history.push((role, text, thumb));
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

    /// Status lines shared by every screen: transient notice and error.
    /// Empty when there is nothing to say.
    pub fn status_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if let Some(n) = &self.notice {
            if !n.trim().is_empty() {
                lines.push(n.clone());
            }
        }
        if let Some(e) = &self.error {
            if !e.trim().is_empty() {
                lines.push(e.clone());
            }
        }
        lines.join("\n")
    }

    /// Full page state for `window.notchbud.render(state)`.
    pub fn ui_state(&self, greeting: &str) -> serde_json::Value {
        let history: Vec<serde_json::Value> = self
            .history
            .iter()
            .map(|(role, text, image)| {
                serde_json::json!({
                    "role": role.as_str(),
                    "text": text,
                    "image": image,
                })
            })
            .collect();
        serde_json::json!({
            "greeting": greeting,
            "status": self.status_text(),
            "history": history,
            "stream": self.stream,
            "streaming": self.streaming,
            "signingIn": self.signing_in,
            "connected": self.connected(),
        })
    }
}

pub struct App {
    pub hwnd: windows::Win32::Foundation::HWND,
    pub scale: f32,
    pub width: i32,
    pub height: i32,

    pub expanded: bool, // false = idle sliver, true = island shown
    pub mouse_inside: bool,

    // island chat backend (history + tokens); content renders in WebView2
    pub chat: Chat,

    /// Local display name ("What should we call you?"). Plain text on disk,
    /// empty until the user answers. Survives disconnects (it is device
    /// profile, not key-bound).
    pub display_name: String,

    /// Settings pencil reopen of the shared name step. Forces `needsName`
    /// even with a saved name; cleared on save or on leaving the island.
    pub rename: bool,

    /// Topbar overlay page (menu / chats / notifications / settings).
    /// `None` means the base [`Screen`] owns the frame. Set via the "nav"
    /// web intent, cleared on Back, on collapse, and on auth transitions.
    pub overlay: Overlay,

    /// Local chat sessions (newest first, see sessions.rs). The live
    /// `chat.history` is the working copy of the session `session_id`.
    pub sessions: Vec<crate::sessions::Session>,
    /// Active session id (empty until the first submit creates one).
    pub session_id: String,
    /// Groq-titled label for the active session (empty until titled).
    pub session_title: String,
    /// Session id awaiting delete confirmation (set by ask_delete, read by
    /// the confirm page, cleared on Yes/No). Only valid while the overlay
    /// is ConfirmDelete — the confirm page is unreachable otherwise.
    pub pending_delete: String,
    /// Camera screenshot awaiting send (full PNG data URL). Set by the
    /// take_shot intent, attached to the next submit, then cleared. Never
    /// archived (see the shortcut note in shot.rs).
    pub pending_image: String,
    /// Display thumb for the pending shot: previews in the pill, then
    /// rides the history turn (history keeps the cheap thumb only).
    pub pending_thumb: String,
    pub update_inflight: bool,
    /// Owner generation of the active manual check (0 = none). Worker
    /// results carry their generation; stale ones are dropped so a late
    /// reply can never clobber a newer check, and background discovery
    /// (gen 0) never disturbs an active check or download.
    pub update_gen: u32,
    pub update_version: Option<String>,
    pub update_url: Option<String>,
    pub update_status: Option<String>,

    // geometry snapshot refreshed each frame (NCHITTEST hit surface)
    pub bar_area: [f32; 4],
    pub rgn_area: [f32; 4],

    // window origin in screen px, cached so the hit-test never calls
    // GetWindowRect — seeded at startup, refreshed on WM_DPICHANGED reflow
    pub win_x: i32,
    pub win_y: i32,
    /// Last UI-thread push into the WebView2 page (GetTickCount64 ms).
    /// Used to coalesce streaming deltas so Chromium isn't re-rendered
    /// per token.
    pub last_web_push_ms: u64,
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
            display_name: String::new(),
            rename: false,
            overlay: Overlay::None,
            sessions: Vec::new(),
            session_id: String::new(),
            session_title: String::new(),
            pending_delete: String::new(),
            pending_image: String::new(),
            pending_thumb: String::new(),
            update_inflight: false,
            update_gen: 0,
            update_version: None,
            update_url: None,
            update_status: None,
            bar_area: [0.0; 4],
            rgn_area: [0.0; 4],
            win_x: 0,
            win_y: 0,
            last_web_push_ms: 0,
        }
    }

    /// First name shown in the island UI. Empty until the user answers
    /// "What should we call you?" — the greeting then shows the day part
    /// with it ("Good Evening, Alex").
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Greeting owns the top of the island only once signed in and before
    /// any chat does — the first submitted prompt hides it for the session.
    pub fn shows_greeting(&self) -> bool {
        self.chat.connected() && !self.chat.has_chatted()
    }

    /// Which section owns this frame (see Screen). The greeting and chat
    /// field wait behind the sign-in: a signed-out island shows only the
    /// sign-in prompt.
    pub fn screen(&self) -> Screen {
        if !self.chat.connected() {
            Screen::SignIn
        } else if self.chat.has_chatted() {
            Screen::Chat
        } else {
            Screen::Home
        }
    }

    /// Effective UI page the WebView2 router shows. The overlay wins when
    /// open; otherwise the base screen owns the frame. Auth is an alias:
    /// signin is the auth page when signed out.
    pub fn page(&self) -> &'static str {
        match self.overlay {
            Overlay::None => self.screen().as_str(),
            overlay => overlay.as_str(),
        }
    }

    /// Close the overlay and return to the base screen.
    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Topbar / menu navigation. Targets:
    /// * "menu" | "notifications" | "settings" — open (toggle: tapping the
    ///   active icon closes it back to the base screen).
    /// * "auth" — close the overlay so the base auth step shows (signin
    ///   when signed out, home/chat when signed in).
    /// * "home" | "chat" | "back" | "" — close the overlay, stay on base.
    /// Returns true when the overlay changed.
    pub fn navigate(&mut self, target: &str) -> bool {
        let prev = self.overlay;
        match Overlay::from_target(target) {
            Some(next) => {
                self.overlay = if prev == next {
                    Overlay::None
                } else {
                    next
                };
            }
            None => {
                // "auth" / "home" / "chat" / "back" all resolve to the base
                // screen — the base Screen enum already picks the right one.
                self.overlay = Overlay::None;
            }
        }
        self.overlay != prev
    }

    /// "Good Evening, Alex" for right now. Empty name greets with the day
    /// part alone ("Good Evening").
    pub fn greeting(&self) -> String {
        crate::user::greeting_now(self.display_name())
    }

    /// Page state for the WebView2 island (screen + overlay page + chat).
    /// `screen` stays for backwards compat; `page` is what the router
    /// shows. `overlay` is "none" on the base flow. Settings reads
    /// `connected` / `keyReady` (keyReady mirrors connected: a saved key
    /// means ready); `userName` is the display name (null until set);
    /// `needsName` owns Home until answered — the page shows the name box
    /// INSTEAD of the chat box, never both (mirrors SignIn owning the
    /// frame when signed out); `email` stays null (an API key carries
    /// none); notifications reads `notifications` (empty until dynamic
    /// — the page shows its hardcoded empty state meanwhile); chats reads
    /// `sessions` (linear, newest first: id/title/preview) plus
    /// `currentId` for the live session.
    pub fn page_state(&self) -> String {
        let mut v = self.chat.ui_state(&self.greeting());
        v["screen"] = serde_json::json!(self.screen().as_str());
        v["overlay"] = serde_json::json!(self.overlay.as_str());
        v["page"] = serde_json::json!(self.page());
        v["userName"] = if self.display_name.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(self.display_name)
        };
        v["needsName"] = serde_json::json!(
            self.chat.connected()
                && !self.chat.has_chatted()
                && (self.rename || self.display_name.trim().is_empty())
        );
        v["email"] = serde_json::Value::Null;
        v["keyReady"] = serde_json::json!(self.chat.connected());
        v["notifications"] = serde_json::json!([] as [serde_json::Value; 0]);
        let sessions: Vec<serde_json::Value> = self
            .sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "title": s.display_title(),
                    "preview": s.preview(),
                })
            })
            .collect();
        v["sessions"] = serde_json::json!(sessions);
        v["currentId"] = serde_json::json!(self.session_id);
        // Camera preview: the thumb data URL when a shot is attached, else null.
        v["pendingImage"] = if self.pending_thumb.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(self.pending_thumb)
        };
        // Confirm page: which stored chat the Yes button would delete.
        v["deleteTitle"] = serde_json::json!(
            self.sessions
                .iter()
                .find(|s| s.id == self.pending_delete)
                .map(|s| s.display_title())
                .unwrap_or_default()
        );
        v["update"] = serde_json::json!({"available": self.update_version.is_some(),"version": self.update_version,"status": self.update_status});
        v.to_string()
    }

    /// Point-in-rect for click routing (window px).
    pub fn hit(rect: &[f32; 4], x: f32, y: f32) -> bool {
        x >= rect[0] && x < rect[2] && y >= rect[1] && y < rect[3]
    }

    pub fn layout_size(&self) -> (f32, f32) {
        (self.scale * WINDOW_UNITS_W, self.scale * WINDOW_UNITS_H)
    }
}

/// Page mouseleave decision: the child is inset in the island, so leaving
/// the document usually means landing on parent surface, not leaving the
/// island. Collapse only on a cursor truly outside the island bar.
/// `None` (position unreadable) collapses — fail closed, never stuck open.
pub fn page_leave_collapses(bar: &[f32; 4], cursor: Option<(f32, f32)>) -> bool {
    cursor.map_or(true, |(x, y)| !App::hit(bar, x, y))
}

/// Mousemove choreography decision: closed->open transitions only. Moves
/// while already expanded (e.g. the parent strips around the WebView
/// child) must not restart the open spring.
pub fn expand_on_mousemove(expanded: bool) -> bool {
    !expanded
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

    /// Both leave paths (parent WM_MOUSELEAVE and the page's mouseleave)
    /// collapse on exactly this predicate. Half-open on right/bottom,
    /// matching NCHITTEST hit-testing — pin it so the two tracking domains
    /// can never disagree again (that disagreement thrashed
    /// close->open->close on every downward exit).
    #[test]
    fn hit_is_half_open_and_shared_by_leave_paths() {
        let bar = [10.0, 0.0, 20.0, 14.0];
        assert!(App::hit(&bar, 10.0, 0.0));
        assert!(App::hit(&bar, 15.0, 13.9));
        assert!(!App::hit(&bar, 20.0, 7.0), "right edge exclusive");
        assert!(!App::hit(&bar, 15.0, 14.0), "bottom edge exclusive");
        assert!(!App::hit(&bar, 9.9, 7.0));
        // parent strips around an inset WebView child count as inside:
        // a child->strip move must not collapse.
        let island = [110.0, 0.0, 530.0, 140.4];
        assert!(App::hit(&island, 115.0, 70.0));
        assert!(App::hit(&island, 525.0, 5.0));
        assert!(!App::hit(&island, 531.0, 70.0));
    }

    #[test]
    fn signing_in_hides_the_pill_and_failures_restore_it() {
        let mut c = Chat::new();
        // idle: pill only
        assert!(c.shows_signin_button());

        // in flight: no pill
        c.signing_in = true;
        assert!(!c.shows_signin_button());

        // failure clears back to the pill
        c.signing_in = false;
        c.error = Some(crate::connector::chat::AUTH_REJECTED_MSG.into());
        assert!(c.shows_signin_button());
        assert!(c.status_text().contains("rejected"));
    }

    fn blank_app() -> App {
        App::new(
            windows::Win32::Foundation::HWND(std::ptr::null_mut()),
            1.0,
            640,
            190,
        )
    }

    /// Test-only sign-in: a saved Groq key is all `connected()` needs.
    fn sign_in(app: &mut App) {
        app.chat.tokens = Some(crate::connector::auth::ApiKey {
            key: "gsk_test1234567890abcdef".into(),
        });
    }

    #[test]
    fn shot_thumb_rides_its_history_turn() {
        // The thumbnail a screenshot turn leaves behind renders above its
        // bubble; plain turns carry null so the page renders text only.
        let mut app = blank_app();
        app.chat.push_history(Role::User, "plain".into());
        app.chat
            .push_history_image(Role::User, "look".into(), "data:image/png;base64,AAA".into());
        let v = app.chat.ui_state("Hi");
        assert!(v["history"][0]["image"].is_null());
        assert_eq!(v["history"][1]["image"], "data:image/png;base64,AAA");
    }

    #[test]
    fn greeting_shows_day_part_without_account_name() {
        // Signed out: the sign-in prompt owns the frame, no greeting name.
        let mut app = blank_app();
        assert_eq!(app.screen(), Screen::SignIn);
        assert!(!app.shows_greeting());
        assert_eq!(app.display_name(), "");
        // Signed in with a key: greeting shows the day part alone.
        sign_in(&mut app);
        assert_eq!(app.screen(), Screen::Home);
        assert!(app.shows_greeting());
        assert_eq!(app.display_name(), "");
        assert!(app.greeting().contains("Good") || app.greeting().contains("Sleep"));
    }

    #[test]
    fn first_chat_hides_the_greeting() {
        let mut app = blank_app();
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
        sign_in(&mut app);
        app.chat.push_history(Role::User, "hi".into());
        assert_eq!(app.screen(), Screen::Chat);
        app.chat.tokens = None;
        assert_eq!(app.screen(), Screen::SignIn);
    }

    #[test]
    fn page_state_matches_screen() {
        let mut app = blank_app();
        sign_in(&mut app);
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["screen"], "home");
        assert_eq!(v["page"], "home");
        assert_eq!(v["overlay"], "none");
        assert!(!v["greeting"].as_str().unwrap().is_empty());
        app.chat.push_history(Role::User, "hi".into());
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["screen"], "chat");
        assert_eq!(v["page"], "chat");
    }

    #[test]
    fn update_state_flows_into_page_state() {
        // Up to date: the island shows no glow, no version, no status.
        let mut app = blank_app();
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["update"]["available"], false);
        assert!(v["update"]["version"].is_null());
        assert!(v["update"]["status"].is_null());
        // New release known: the button glows with the version.
        app.update_version = Some("0.9.9".into());
        app.update_url = Some("https://x/setup.exe".into());
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["update"]["available"], true);
        assert_eq!(v["update"]["version"], "0.9.9");
        // Transient worker line (Checking…/Downloading…/error) rides along.
        app.update_status = Some("Downloading v0.9.9…".into());
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["update"]["status"], "Downloading v0.9.9…");
    }

    #[test]
    fn overlay_wins_over_base_screen_and_toggles() {
        let mut app = blank_app();
        sign_in(&mut app);
        assert_eq!(app.page(), "home");

        // open settings from home
        assert!(app.navigate("settings"));
        assert_eq!(app.overlay, Overlay::Settings);
        assert_eq!(app.page(), "settings");
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["overlay"], "settings");
        assert_eq!(v["page"], "settings");
        assert_eq!(v["screen"], "home", "base screen survives under the overlay");
        assert!(v["userName"].is_null());
        assert_eq!(v["email"], serde_json::Value::Null);

        // tapping the active icon toggles back
        assert!(app.navigate("settings"));
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.page(), "home");

        // notifications + menu
        app.navigate("notifications");
        assert_eq!(app.page(), "notifications");
        app.navigate("menu");
        assert_eq!(app.page(), "menu", "switching overlay replaces it");

        // back / home / chat / auth all close the overlay
        for target in ["back", "home", "chat", "auth", ""] {
            app.close_overlay();
            app.navigate("menu");
            assert_eq!(app.page(), "menu");
            app.navigate(target);
            assert_eq!(app.overlay, Overlay::None);
        }
    }

    #[test]
    fn auth_target_resolves_to_base_auth_step() {
        // signed out -> signin owns the frame even via "auth"
        let mut app = blank_app();
        app.navigate("auth");
        assert_eq!(app.screen(), Screen::SignIn);
        assert_eq!(app.page(), "signin");

        // signed in -> home (auth has nothing to do)
        sign_in(&mut app);
        app.navigate("auth");
        assert_eq!(app.page(), "home");
    }

    #[test]
    fn signed_out_menu_home_returns_to_key_entry() {
        // No Auth tile anymore: a signed-out user on the Menu overlay picks
        // Home and must land on key entry (the base signin screen), never a
        // dead home page and with no restart.
        let mut app = blank_app();
        app.navigate("menu");
        assert_eq!(app.page(), "menu");
        app.navigate("home");
        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.page(), "signin");
    }

    #[test]
    fn page_state_carries_sessions_linearly() {
        // The Chats page reads `sessions` (newest first: title + preview)
        // plus `currentId` for the live session.
        let mut app = blank_app();
        sign_in(&mut app);
        app.session_id = "s1".into();
        app.session_title = "Trip Ideas".into();
        app.chat.push_history(Role::User, "plan a trip".into());
        crate::sessions::upsert(
            &mut app.sessions,
            crate::sessions::snapshot(&app.session_id, &app.session_title, &app.chat.history),
        );
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["sessions"][0]["id"], "s1");
        assert_eq!(v["sessions"][0]["title"], "Trip Ideas");
        assert_eq!(v["sessions"][0]["preview"], "plan a trip");
        assert_eq!(v["currentId"], "s1");
        // Untitled sessions fall back to a snippet, never blank.
        let mut bare = blank_app();
        sign_in(&mut bare);
        bare.session_id = "s2".into();
        bare.chat.push_history(Role::User, "hello there".into());
        crate::sessions::upsert(
            &mut bare.sessions,
            crate::sessions::snapshot(&bare.session_id, &bare.session_title, &bare.chat.history),
        );
        let v: serde_json::Value =
            serde_json::from_str(&bare.page_state()).unwrap();
        assert_eq!(v["sessions"][0]["title"], "hello there");
    }

    #[test]
    fn chats_overlay_toggles_like_other_pages() {
        let mut app = blank_app();
        sign_in(&mut app);
        assert!(app.navigate("chats"));
        assert_eq!(app.overlay, Overlay::Chats);
        assert_eq!(app.page(), "chats");
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["overlay"], "chats");
        assert_eq!(v["page"], "chats");
        assert_eq!(v["screen"], "home", "base screen survives under the overlay");
        // Tapping the active Chats tile closes it back.
        assert!(app.navigate("chats"));
        assert_eq!(app.overlay, Overlay::None);
    }

    #[test]
    fn page_state_carries_settings_fields() {
        let mut app = blank_app();
        sign_in(&mut app);
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert!(v["userName"].is_null());
        assert!(v["email"].is_null());
        assert_eq!(v["keyReady"], true);
        assert!(v["notifications"].as_array().unwrap().is_empty());
    }

    #[test]
    fn display_name_flows_into_greeting_and_page_state() {
        let mut app = blank_app();
        sign_in(&mut app);
        app.display_name = "Alex".into();
        assert_eq!(app.display_name(), "Alex");
        assert!(app.greeting().contains("Alex"));
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["userName"], "Alex");
    }

    #[test]
    fn rename_reopens_name_step_without_trapping_chat() {
        // The settings pencil forces the shared step even with a saved
        // name; a live chat still never loses its box to the prompt.
        let mut app = blank_app();
        sign_in(&mut app);
        app.display_name = "Alex".into();
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], false);
        app.rename = true;
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], true);
        app.rename = false; // answered (the save_name arm clears it)
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], false);
        // A live chat still never loses its box to the prompt.
        app.rename = true;
        app.chat.push_history(Role::User, "hi".into());
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], false);
        assert_eq!(v["screen"], "chat");
        // Signed out: no prompt even when forced (the pencil hides there).
        let mut app = blank_app();
        app.rename = true;
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], false);
    }

    #[test]
    fn name_prompt_owns_home_until_answered() {
        // Connected but nameless, nothing asked yet: needsName owns Home —
        // the page shows the name box INSTEAD of the chat box, never both
        // (mirrors SignIn owning the frame when signed out).
        let mut app = blank_app();
        sign_in(&mut app);
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], true);
        // Answered: the chat box owns Home again.
        app.display_name = "Alex".into();
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], false);
        // A live chat never loses its box to the prompt (no trap).
        let mut app = blank_app();
        sign_in(&mut app);
        app.chat.push_history(Role::User, "hi".into());
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], false);
        // Signed out: the sign-in screen owns the frame, no name prompt.
        let app = blank_app();
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["screen"], "signin");
        assert_eq!(v["needsName"], false);
    }
}
