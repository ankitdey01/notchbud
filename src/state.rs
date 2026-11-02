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
pub const ISLAND_BOT_R: f32 = 35.2;   // bottom corner radius

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
pub const IDLE_RGN_H: f32 = 14.0;      // region bottom edge (idle)

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
//  * open -> idle: fast collapse, ~240 ms end-to-end, no visible overshoot:
//               critically damped response 0.21, with the ~60 ms the shell
//               needs to notice the leave baked in as a delay.
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
// a fast, near-symmetric collapse (~240 ms end-to-end, no overshoot, visible
// shrink begins ~35 ms after the leave). Recognized curve: piecewise-linear
// through the measured frames; velocity is the knot-slope, feeding Hermite.
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

pub const CLOSE_SETTLE: f32 = 0.238; // the measured collapse duration

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

pub struct App {
    pub hwnd: windows::Win32::Foundation::HWND,
    pub scale: f32,
    pub width: i32,
    pub height: i32,

    pub expanded: bool,   // false = idle sliver, true = island shown
    pub mouse_inside: bool,

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
    pub fn new(hwnd: windows::Win32::Foundation::HWND, scale: f32, width: i32, height: i32) -> Self {
        App {
            hwnd,
            scale,
            width,
            height,
            expanded: false,
            mouse_inside: false,
            bar_area: [0.0; 4],
            rgn_area: [0.0; 4],
            win_x: 0,
            win_y: 0,
            pump_steps: 0,
        }
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
