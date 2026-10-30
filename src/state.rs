// state.rs — minimal notch state: idle sliver -> hover-expanding island.

use windows::Win32::Foundation::POINT;

pub const WINDOW_UNITS_W: f32 = 640.0;
pub const WINDOW_UNITS_H: f32 = 190.0;

// --- geometry constants (measured from the original, physical px / 1.25) ---
pub const ISLAND_W: f32 = 420.0; // island x 109.6..529.6 in the 640-unit window
pub const ISLAND_H: f32 = 139.2; // island y 0..139.2
pub const ISLAND_X: f32 = (WINDOW_UNITS_W - ISLAND_W) / 2.0;
pub const SLIVER_W: f32 = 46.4; // idle tab 58x5 physical px
pub const SLIVER_H: f32 = 6.0;  // hit-test height — a hover margin around the tab
pub const SLIVER_X: f32 = (WINDOW_UNITS_W - SLIVER_W) / 2.0;


pub const IDLE_RGN_HALF_W: f32 = 56.0; // 112u wide centered hover strip
pub const IDLE_RGN_H: f32 = 14.0;      // region bottom edge (idle)

// ---------------------------------------------------------------------------
// Atoll motion — spring translation
//
// Atoll (ContentView.swift, modern close animation) drives the notch
// shell with SwiftUI springs:
//   open:  Animation.spring(response: 0.42, dampingFraction: 1.0)
//   close: Animation.spring(response: 0.45, dampingFraction: 1.0)
//
// those are critically damped mass-spring oscillators (mass 1, ω = 2π/response):
//   x(t) = 1 - (1 + ωt)·e^(-ωt)     — starts at rest, no overshoot
//   x'(t) = ω²·t·e^(-ωt)
// Stated exactly so the sampled curve matches SwiftUI's on-screen feel.
// ---------------------------------------------------------------------------

pub const SPRING_RESPONSE_OPEN: f32 = 0.42; // seconds
pub const SPRING_RESPONSE_CLOSE: f32 = 0.45; // seconds

/// Normalized progress toward the target at time `t` for a critically
/// damped SwiftUI spring with the given response.
pub fn spring_progress(response: f32, t: f32) -> f32 {
    let w = std::f32::consts::TAU / response;
    let x = w * t.max(0.0);
    1.0 - (1.0 + x) * (-x).exp()
}

/// Velocity (derivative of `spring_progress`) at time `t`.
pub fn spring_velocity(response: f32, t: f32) -> f32 {
    let w = std::f32::consts::TAU / response;
    let x = w * t.max(0.0);
    let tail = (-x).exp();
    w * w * t.max(0.0) * tail
}

/// Time until the spring settles to 99.9% (1 + x)e^-x = 1e-3 → x ≈ 9.23.
pub fn spring_settle(response: f32) -> f32 {
    9.23 * response / std::f32::consts::TAU
}

// --- compositor clip rects, in window px (scale-correct at any DPI) ---
// The idle clip: window pokes through only the sliver; the open clip:
// whole window (the island is transparent beyond its shape anyway).
pub fn clip_closed(scale: f32) -> [f32; 3] {
    let cx = WINDOW_UNITS_W / 2.0 * scale;
    [cx - SLIVER_W * scale / 2.0, cx + SLIVER_W * scale / 2.0, 4.0 * scale] // (left, right, bottom)
}
pub fn clip_open(scale: f32) -> [f32; 3] {
    [0.0, WINDOW_UNITS_W * scale, (WINDOW_UNITS_H - 0.4) * scale]
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

/// lParam of WM_NCHITTEST: screen px — subtract the window origin.
pub fn screen_pt(app: &App, lparam: windows::Win32::Foundation::LPARAM) -> POINT {
    unsafe {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut wr = RECT::default();
        let _ = GetWindowRect(app.hwnd, &mut wr);
        let mut p = client_pt(lparam);
        p.x -= wr.left;
        p.y -= wr.top;
        p
    }
}
