// notchbud — minimal notch: idle sliver at the top of the screen; hovering
// expands the island (sign-in prompt when signed out, chat when signed in);
// leaving collapses it.

// No console window in release builds: this is a pure GUI (Win32) app.
// Debug builds keep the console so the [notchbud] log lines stay visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use notchbud::{connector, render, state, user};

mod prefs;
mod tray;

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::core::{w, Result};use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, MonitorFromPoint, SetWindowRgn, ValidateRect, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForSystem, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardState, ReleaseCapture, SetCapture, ToUnicode, TME_LEAVE,
    TRACKMOUSEEVENT, TrackMouseEvent, VK_BACK, VK_CONTROL, VK_ESCAPE, VK_MENU, VK_RETURN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, GetWindowRect, KillTimer, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetCursor, SetWindowsHookExW,
    SetWindowLongPtrW, SetTimer, SetWindowPos, GetWindowLongPtrW, ShowWindow, TranslateMessage, UnhookWindowsHookEx,
    GWLP_USERDATA, HHOOK, HTCLIENT, HTTRANSPARENT, IDC_ARROW, IDC_IBEAM, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
    SM_CXSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNOACTIVATE, WNDCLASSEXW, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCHITTEST, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN, WM_TIMER,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

// "TaskbarCreated" message id; re-adds the tray icon when explorer restarts
static TASKBAR_MSG: AtomicU32 = AtomicU32::new(0);

// timers on the message pump, replacing spawned sleep threads — KillTimer on
// any reschedule makes the choreography cancellable
const TIMER_FADE_START: usize = 1; // one-shot: fade becomes visible after the delay
const TIMER_FADE: usize = 2;      // 16 ms repaint pump while the text alpha animates
const TIMER_IDLE_RGN: usize = 3;  // one-shot: region swap after the close settles
const FADE_STEP_MS: u32 = 16;

// chat wiring: connector thread(s) post into this pump via PostMessage — the
// WM_TIMER message-pump pattern extended with two custom app messages.
const WM_CHAT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 10; // connector events
const WM_KEY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 11; // keyboard hook chars
const WM_WHEEL: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 12; // mouse hook wheel
/// Chat scroll per wheel notch, in layout units (draw multiplies by scale).
const WHEEL_STEP: f32 = 48.0;
// WM_KEY codes for the island clipboard shortcuts (past the Unicode range
// so they never collide with typed characters).
const KEY_COPY: usize = 0x110000; // Ctrl+C / Ctrl+Insert: copy highlight, else transcript
const KEY_SELECT_ALL: usize = 0x110001; // Ctrl+A: highlight the whole viewport
const KEY_CUT: usize = 0x110002; // Ctrl+X / Shift+Delete: cut field highlight to clipboard
const KEY_PASTE: usize = 0x110003; // Ctrl+V / Shift+Insert: paste clipboard into the field
// WM_CHAT wparam codes (lp carries a Box pointer where noted)
const CHAT_DELTA: usize = 0; // *String
const CHAT_DONE: usize = 1;
const CHAT_ERROR: usize = 2; // *String
const CHAT_CONNECTED: usize = 3; // *Tokens
const CHAT_MODEL: usize = 5; // *String (resolved model)
const CHAT_SIGNED_OUT: usize = 6; // *String (terminal auth failure: clear creds)
const CHAT_STATUS: usize = 7; // *String (transient notice, keeps signing_in)

static ISLAND_ACTIVE: AtomicBool = AtomicBool::new(false); // hook gate: island expanded
static CHAT_CANCEL: AtomicBool = AtomicBool::new(false);   // island left mid-stream
static HOOK_HWND: AtomicIsize = AtomicIsize::new(0);       // where the hook posts
static KEYHOOK: AtomicIsize = AtomicIsize::new(0);         // hook handle (unhook on quit)
static MOUSEHOOK: AtomicIsize = AtomicIsize::new(0);       // wheel hook handle (unhook on quit)

// ---------------------------------------------------------------------------
// wndproc
// ---------------------------------------------------------------------------

unsafe fn app_from(hwnd: HWND) -> Option<&'static mut state::App> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut state::App;
    if p.is_null() { None } else { Some(&mut *p) }
}

unsafe fn apply_window_region(app: &state::App) {
    let a = app.rgn_area;
    let l = a[0].floor() as i32;
    let r = a[2].ceil() as i32;
    let b = a[3].ceil() as i32;
    if r <= l || b <= 0 {
        return; // nothing visible — keep the previous region rather than an empty one
    }
    let rgn = CreateRectRgn(l, 0, r, b);
    if !rgn.is_invalid() {
        let _ = SetWindowRgn(app.hwnd, rgn, false);
    }
}

// region + visuals must follow every expanded/sliver transition
fn after_state_change(app: &state::App) {
    unsafe { apply_window_region(app) };
}

// content-choreography pump: the silhouette scale animates on the DComp
// compositor thread and WM_PAINT never fires on its own during a run, so
// repaint every 16 ms (DWM's cadence) while the drawn text alpha has a
// scheduled fade — otherwise the fade would never advance. delay = when the
// fade starts (0 for the retract fade-out), dur = how long it runs.
unsafe fn start_fade_pump(hwnd: HWND, app: &mut state::App, delay: f32, dur: f32) {
    KillTimer(hwnd, TIMER_FADE_START).ok();
    KillTimer(hwnd, TIMER_FADE).ok();
    app.pump_steps = ((dur.max(0.0) / 0.016) as usize).max(1);
    if delay > 0.001 {
        SetTimer(hwnd, TIMER_FADE_START, (delay * 1000.0).ceil() as u32, None);
    } else {
        SetTimer(hwnd, TIMER_FADE, FADE_STEP_MS, None);
    }
}

// keep the island window region until the shrink finishes, so the collapse
// isn't clipped by the idle strip mid-flight (Nusa does the same); a one-shot
// timer fires after the settle and swaps regions to the idle hit strip.
unsafe fn schedule_idle_region(hwnd: HWND, settle: f32) {
    KillTimer(hwnd, TIMER_IDLE_RGN).ok();
    SetTimer(hwnd, TIMER_IDLE_RGN, (settle * 1000.0).ceil() as u32, None);
}

// ---------------------------------------------------------------------------
// keyboard hook — the only functional addition to the frozen UI.
// The window is WS_EX_NOACTIVATE, so hover alone never grants keystrokes:
// a low-level hook forwards typed keys into WM_KEY while the island is
// expanded (and eats them, so they don't type into the app behind it too).
// ---------------------------------------------------------------------------

unsafe extern "system" fn key_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let is_down = wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize;
    if code >= 0 && is_down && ISLAND_ACTIVE.load(Ordering::Acquire) {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let post = |c: usize| {
            let _ = PostMessageW(
                HWND(HOOK_HWND.load(Ordering::Relaxed) as *mut _),
                WM_KEY,
                WPARAM(c),
                LPARAM(0),
            );
        };
        let ctrl_down = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
        let alt_down = GetAsyncKeyState(VK_MENU.0 as i32) < 0;
        let shift_down = GetAsyncKeyState(VK_SHIFT.0 as i32) < 0;
        // Island clipboard shortcuts work inside the island and never reach
        // the app behind it. Other Ctrl/Alt chords still pass through.
        if ctrl_down && !alt_down {
            match kb.vkCode {
                0x43 | 0x2D => {
                    // C, Insert
                    post(KEY_COPY);
                    return LRESULT(1);
                }
                0x41 => {
                    // A
                    post(KEY_SELECT_ALL);
                    return LRESULT(1);
                }
                0x58 => {
                    // X
                    post(KEY_CUT);
                    return LRESULT(1);
                }
                0x56 => {
                    // V
                    post(KEY_PASTE);
                    return LRESULT(1);
                }
                _ => {}
            }
        } else if shift_down && !ctrl_down && !alt_down {
            match kb.vkCode {
                0x2D => {
                    // Shift+Insert: paste
                    post(KEY_PASTE);
                    return LRESULT(1);
                }
                0x2E => {
                    // Shift+Delete: cut
                    post(KEY_CUT);
                    return LRESULT(1);
                }
                _ => {}
            }
        }
        // only plain typing: no Ctrl/Alt chords (shortcuts pass through)
        if !ctrl_down && !alt_down {
            let vk = kb.vkCode as u32;
            match vk {
                8 | 13 | 27 => { // VK_BACK VK_RETURN VK_ESCAPE
                    post(vk as usize);
                    return LRESULT(1);
                }
                _ => {
                    // VK -> character via the active keyboard layout
                    let mut state = [0u8; 256];
                    let _ = GetKeyboardState(&mut state);
                    let mut buf = [0u16; 4];
                    let n = ToUnicode(vk, kb.scanCode, Some(&state), &mut buf, 0);
                    // n > 0: produced text; n < 0: dead key — never eaten
                    if n > 0 {
                        post(buf[0] as usize);
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe fn install_key_hook(hwnd: HWND, hinstance: HINSTANCE) {
    let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), hinstance, 0);
    if let Ok(h) = hook {
        KEYHOOK.store(h.0 as isize, Ordering::Release);
        HOOK_HWND.store(hwnd.0 as isize, Ordering::Release);
    }
    // wheel scrolling over the expanded island — NOACTIVATE windows never
    // receive WM_MOUSEWHEEL, so mirror the keyboard hook pattern
    let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hinstance, 0);
    if let Ok(h) = hook {
        MOUSEHOOK.store(h.0 as isize, Ordering::Release);
    }
}

// wheel-notches posted as WM_WHEEL while the island is expanded; everything
// else passes through untouched
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0
        && wparam.0 == WM_MOUSEWHEEL as usize
        && ISLAND_ACTIVE.load(Ordering::Acquire)
    {
        let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let delta = ((ms.mouseData >> 16) as u16 as i16) as i32; // signed notch delta
        let _ = PostMessageW(
            HWND(HOOK_HWND.load(Ordering::Relaxed) as *mut _),
            WM_WHEEL,
            WPARAM(delta as usize), // i32 preserved through usize round-trip
            LPARAM(0),
        );
        return LRESULT(1); // eaten: don't scroll the window behind too
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Visible input field selection for the current screen (name prompt or
/// chat line); None on SignIn, which draws no field.
fn field_sel_mut(app: &mut state::App) -> Option<&mut state::FieldSel> {
    match app.screen() {
        state::Screen::Onboarding => Some(&mut app.name_sel),
        state::Screen::Home | state::Screen::Chat => Some(&mut app.chat.input_sel),
        state::Screen::SignIn => None,
    }
}

/// End a field select-drag: a zero-length tap clears the highlight.
fn end_field_drag(sel: &mut state::FieldSel) {
    sel.dragging = false;
    if let Some((a, b)) = sel.range {
        if a == b {
            sel.clear();
        }
    } else {
        sel.anchor = None;
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = app_from(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    match msg {
        WM_NCHITTEST => {
            // inside the active (sliver or expanded island) region -> HTCLIENT,
            // else HTTRANSPARENT (click-through for windows in our thread).
            // Cross-process click-through comes from apply_window_region —
            // the OS never routes hits that land outside the window region.
            // draw() publishes the rect in px; apply_window_region consumes it.
            let pt = state::screen_pt(app, lparam);
            let x = pt.x as f32;
            let y = pt.y as f32;
            let a = app.bar_area;
            let in_bar = x >= a[0] && x < a[2] && y >= a[1] && y < a[3];
            if in_bar {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }

        WM_SETCURSOR => {
            // I-beam over selectable text (chat history or the input line),
            // arrow everywhere else. Handled here — not on MOUSEMOVE — so
            // DefWindowProc can't flicker it back to the class arrow.
            if (lparam.0 & 0xFFFF) == HTCLIENT as isize {
                let mut cpos = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut cpos).is_ok() {
                    // Cached window origin, no GetWindowRect per mouse move.
                    let (px, py) =
                        ((cpos.x - app.win_x) as f32, (cpos.y - app.win_y) as f32);
                    let over_viewport = app.chat_viewport.is_some_and(|r| {
                        px >= r[0] && px < r[2] && py >= r[1] && py < r[3]
                    });
                    let over_field = app.input_field.as_ref().is_some_and(|g| {
                        px >= g.rect[0] && px < g.rect[2] && py >= g.rect[1] && py < g.rect[3]
                    });
                    let id = if over_viewport || over_field {
                        IDC_IBEAM
                    } else {
                        IDC_ARROW
                    };
                    if let Ok(cur) = LoadCursorW(None, id) {
                        let _ = SetCursor(cur);
                        return LRESULT(1);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        WM_MOUSEMOVE => {
            let pt = state::client_pt(lparam);
            let (px, py) = (pt.x as f32, pt.y as f32);
            // Input-field drag first (name prompt or chat line): clamped
            // into the field rect so dragging out selects to the end.
            if app.chat.input_sel.dragging || app.name_sel.dragging {
                if (wparam.0 & 1) != 0 {
                    if let Some(g) = app.input_field.as_ref() {
                        let cx = px.clamp(g.rect[0], (g.rect[2] - 1.0).max(g.rect[0]));
                        let cy = py.clamp(g.rect[1], (g.rect[3] - 1.0).max(g.rect[1]));
                        if let Some(pos) = render::field_caret_from_point(app, cx, cy) {
                            if let Some(sel) = field_sel_mut(app) {
                                let anchor = sel.anchor.unwrap_or(pos);
                                sel.range = Some((anchor.min(pos), anchor.max(pos)));
                                render::draw_now(app);
                            }
                        }
                    }
                } else {
                    // Button released without a LBUTTONUP: end the drags
                    // rather than sticking.
                    end_field_drag(&mut app.chat.input_sel);
                    end_field_drag(&mut app.name_sel);
                    let _ = ReleaseCapture();
                    render::draw_now(app);
                }
            // Select-drag extension while the button is held (capture keeps
            // moves coming even past the island edge; clamp to the viewport
            // so dragging out selects to the nearest end).
            } else if app.chat.selecting {
                if (wparam.0 & 1) != 0 {
                    if app.screen() == state::Screen::Chat {
                        if let Some(vp) = app.chat_viewport {
                            let cx = px.clamp(vp[0], (vp[2] - 1.0).max(vp[0]));
                            let cy = py.clamp(vp[1], (vp[3] - 1.0).max(vp[1]));
                            if let Some(pos) = render::caret_from_point(app, cx, cy) {
                                let anchor = app.chat.selection_anchor.unwrap_or(pos);
                                app.chat.selection =
                                    Some((anchor.min(pos), anchor.max(pos)));
                                render::draw_now(app);
                            }
                        }
                    }
                } else {
                    // Button released without a LBUTTONUP (hook ate it, or
                    // capture broke): end the drag rather than sticking.
                    app.chat.selecting = false;
                    if let Some((a, b)) = app.chat.selection {
                        if a == b {
                            app.chat.clear_selection();
                        }
                    }
                    let _ = ReleaseCapture();
                    render::draw_now(app);
                }
            }
            if !app.mouse_inside {
                app.mouse_inside = true;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                app.expanded = true;
                ISLAND_ACTIVE.store(true, Ordering::Release);
                // animation clock first, so the painted launch frame starts
                // from the fresh content timeline (greeting hidden on open)
                render::animate(true);
                render::draw_now(app);
                after_state_change(app); // island region now, so it stays clickable while the clip animates open
                // the greeting reveals only after the pop settles — pump the
                // repaints that fade it in
                start_fade_pump(app.hwnd, app, state::open_settle(), state::CONTENT_FADE + 0.05);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        WM_MOUSELEAVE => {
            // A select-drag owns the island: leaving mid-drag must not
            // collapse it (or cancel the stream being copied).
            if app.chat.selecting || app.chat.input_sel.dragging || app.name_sel.dragging {
                app.mouse_inside = false;
                return LRESULT(0);
            }
            app.mouse_inside = false;
            ISLAND_ACTIVE.store(false, Ordering::Release);
            app.expanded = false;
            // the island left mid-stream: default cancel — the partial
            // stream is discarded silently
            if app.chat.streaming {
                CHAT_CANCEL.store(true, Ordering::Release);
                app.chat.streaming = false;
                app.chat.stream.clear();
            }
            // animation clock first so the painted frame fades the text out
            // starting at alpha 1; it is fully gone before the shrink starts
            render::animate(false);
            render::draw_now(app);
            start_fade_pump(app.hwnd, app, 0.0, state::CLOSE_DELAY + 0.05);
            // keep the island window region until the shrink finishes, so the
            // collapse isn't clipped by the idle strip mid-flight (Nusa does
            // the same); a one-shot timer swaps regions after the settle.
            schedule_idle_region(app.hwnd, state::CLOSE_SETTLE + state::CLOSE_DELAY + 0.12);
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            let pt = state::client_pt(lparam);
            let (px, py) = (pt.x as f32, pt.y as f32);
            // Input field first (name prompt or chat line): begin a
            // select-drag. The field sits below the viewport; rects are
            // disjoint.
            if let Some(pos) = render::field_caret_from_point(app, px, py) {
                if let Some(sel) = field_sel_mut(app) {
                    sel.dragging = true;
                    sel.anchor = Some(pos);
                    sel.range = Some((pos, pos));
                    let _ = SetCapture(hwnd);
                    render::draw_now(app);
                }
                LRESULT(0)
            // Chat viewport: begin a select-drag. Anywhere else on the Chat
            // screen (padding): drop the highlight.
            } else if app.screen() == state::Screen::Chat {
                if let Some(vp) = app.chat_viewport {
                    if px >= vp[0] && px < vp[2] && py >= vp[1] && py < vp[3] {
                        if let Some(pos) = render::caret_from_point(app, px, py) {
                            app.chat.selecting = true;
                            app.chat.selection_anchor = Some(pos);
                            app.chat.selection = Some((pos, pos));
                            let _ = SetCapture(hwnd);
                            render::draw_now(app);
                        }
                        LRESULT(0)
                    } else {
                        if app.chat.selection.is_some()
                            || app.chat.selection_anchor.is_some()
                        {
                            app.chat.clear_selection();
                            render::draw_now(app);
                        }
                        LRESULT(0)
                    }
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            } else {
                // Clicking elsewhere on a field screen drops the field
                // highlight; SignIn keeps default handling (pill uses UP).
                let mut changed = false;
                if let Some(sel) = field_sel_mut(app) {
                    if sel.range.is_some() || sel.anchor.is_some() {
                        sel.clear();
                        changed = true;
                    }
                }
                if changed {
                    render::draw_now(app);
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
        }

        WM_LBUTTONUP => {
            // End of a field select-drag: a zero-length tap clears it.
            if app.chat.input_sel.dragging || app.name_sel.dragging {
                end_field_drag(&mut app.chat.input_sel);
                end_field_drag(&mut app.name_sel);
                let _ = ReleaseCapture();
                render::draw_now(app);
                return LRESULT(0);
            }
            // End of a viewport select-drag: a zero-length tap clears it.
            if app.chat.selecting {
                app.chat.selecting = false;
                let _ = ReleaseCapture();
                if let Some((a, b)) = app.chat.selection {
                    if a == b {
                        app.chat.clear_selection();
                    }
                } else {
                    app.chat.selection_anchor = None;
                }
                render::draw_now(app);
                return LRESULT(0);
            } else if app.screen() == state::Screen::SignIn
                && app.chat.shows_signin_button()
                && !app.chat.signing_in
            {
                // Only the sign-in pill is clickable: it starts the
                // browser-based ChatGPT sign-in (same as tray Connect).
                let pt = state::client_pt(lparam);
                let (px, py) = (pt.x as f32, pt.y as f32);
                let on_pill = app.signin_rect.is_some_and(|r| state::App::hit(&r, px, py));
                // Lenient: the pill is the visual, but the whole island stays
                // actionable so a first-timer cannot miss.
                let a = app.bar_area;
                let on_island = px >= a[0] && px < a[2] && py >= a[1] && py < a[3];
                if on_pill || on_island {
                    start_signin(app);
                    render::draw_now(app);
                }
            }
            LRESULT(0)
        }

        WM_TIMER => {
            match wparam.0 {
                TIMER_FADE_START => {
                    KillTimer(hwnd, TIMER_FADE_START).ok();
                    SetTimer(hwnd, TIMER_FADE, FADE_STEP_MS, None);
                }
                TIMER_FADE => {
                    if app.pump_steps == 0 {
                        KillTimer(hwnd, TIMER_FADE).ok();
                    } else {
                        app.pump_steps -= 1;
                        render::draw_now(app);
                        if app.pump_steps == 0 {
                            KillTimer(hwnd, TIMER_FADE).ok();
                        }
                    }
                }
                TIMER_IDLE_RGN => {
                    KillTimer(hwnd, TIMER_IDLE_RGN).ok();
                    after_state_change(app);
                }
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
            LRESULT(0)
        }

        WM_PAINT => {
            render::draw_now(app);
            // validate: without this the update region never clears and the
            // resulting WM_PAINT storm starves everything in the queue
            let _ = ValidateRect(hwnd, None);
            LRESULT(0)
        }

        WM_DPICHANGED => {
            reflow_on_dpi_change(app, lparam);
            LRESULT(0)
        }

        WM_DESTROY => {
            let hook_handle = KEYHOOK.load(Ordering::Relaxed);
            if hook_handle != 0 {
                let _ = UnhookWindowsHookEx(HHOOK(hook_handle as *mut _));
            }
            let hook_handle = MOUSEHOOK.load(Ordering::Relaxed);
            if hook_handle != 0 {
                let _ = UnhookWindowsHookEx(HHOOK(hook_handle as *mut _));
            }
            tray::remove_tray(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => {
            if msg == WM_CHAT {
                handle_chat_msg(app, wparam, lparam);
                LRESULT(0)
            } else if msg == WM_KEY {
                handle_chat_key(app, wparam.0);
                LRESULT(0)
            } else if msg == WM_WHEEL {
                // wheel up (positive) reveals older lines; draw clamps to
                // [0, max_scroll] and skips work when nothing overflows
                let delta = wparam.0 as i32;
                if delta != 0 && app.chat.max_scroll > 0.0 {
                    app.chat.scroll =
                        (app.chat.scroll + delta.signum() as f32 * WHEEL_STEP * app.scale)
                            .clamp(0.0, app.chat.max_scroll);
                    render::draw_now(app);
                }
                LRESULT(0)
            } else if msg == tray::TRAY_CALLBACK {
                // left click toggles the island, right click opens the menu
                let lowword = (lparam.0 & 0xFFFF) as u16 as i32;
                if lowword == WM_RBUTTONUP as i32 {
                    handle_tray_menu(app);
                } else if lowword == WM_LBUTTONUP as i32 {
                    app.expanded = !app.expanded;
                    ISLAND_ACTIVE.store(app.expanded, Ordering::Release);
                    // animation clock first, then the launch frame (same
                    // choreography as the hover paths)
                    render::animate(app.expanded);
                    render::draw_now(app);
                    if app.expanded {
                        after_state_change(app); // island region now, so it stays clickable while the clip animates open
                        start_fade_pump(app.hwnd, app, state::open_settle(), state::CONTENT_FADE + 0.05);
                    } else {
                        // the region swaps only after the shrink settles, so
                        // the collapse isn't clipped by the idle strip
                        schedule_idle_region(app.hwnd, state::CLOSE_SETTLE + state::CLOSE_DELAY + 0.12);
                        start_fade_pump(app.hwnd, app, 0.0, state::CLOSE_DELAY + 0.05);
                    }
                }
                LRESULT(0)
            } else {
                let taskbar_msg = TASKBAR_MSG.load(Ordering::Relaxed);
                if taskbar_msg != 0 && msg == taskbar_msg {
                    let _ = tray::add_tray(hwnd); // explorer restarted — re-register
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// behind the handlers
// ---------------------------------------------------------------------------

fn handle_tray_menu(app: &mut state::App) {
    unsafe {
        let chosen = tray::popup_menu(app.hwnd, app.chat.connected(), app.chat.plan_ready());
        match chosen as u32 {
            tray::ID_START => {
                let on = !prefs::is_start_with_windows();
                let _ = prefs::set_start_with_windows(on);
            }
            tray::ID_CONNECT => start_signin(app),
            tray::ID_DISCONNECT => disconnect(app),
            tray::ID_EXIT => PostQuitMessage(0),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// chat handling (Milestones 1–3)
// ---------------------------------------------------------------------------

fn disconnect(app: &mut state::App) {
    // Best-effort remote revocation off the UI thread; local credentials are
    // cleared immediately either way.
    if let Some(tokens) = app.chat.tokens.clone() {
        std::thread::spawn(move || connector::auth::revoke_session(&tokens));
    }
    connector::auth::delete_tokens();
    app.chat.tokens = None;
    app.chat.model = None;
    app.chat.streaming = false;
    app.chat.stream.clear();
    app.chat.history.clear();
    app.chat.notice = None;
    app.chat.clear_selection();
    app.chat.input_sel.clear();
    CHAT_CANCEL.store(true, Ordering::Release);
    app.chat.error = Some("Disconnected from ChatGPT.".into());
    render::draw_now(app);
}

/// Tray "Connect ChatGPT": browser-based SIWC authorization on a worker
/// thread. Fresh installs register dynamically; saved accounts reauthorize
/// with their issued client id (also the path for authorizing plan usage
/// after an identity-only sign-in).
fn start_signin(app: &mut state::App) {
    if app.chat.signing_in || app.chat.streaming {
        return;
    }
    if app.chat.plan_ready() {
        return;
    }
    app.chat.signing_in = true;
    app.chat.error = None;
    render::draw_now(app);
    let hwnd = app.hwnd.0 as isize;
    let saved = app.chat.tokens.clone();
    std::thread::spawn(move || unsafe {
        let hwnd_raw = HWND(hwnd as *mut _);
        let fail = |m: String| {
            let _ = PostMessageW(hwnd_raw, WM_CHAT, WPARAM(CHAT_ERROR), LPARAM(Box::into_raw(Box::new(m)) as isize));
        };
        let host_id = match connector::auth::load_or_create_host_id() {
            Ok(h) => h,
            Err(e) => {
                fail(e.msg);
                return;
            }
        };
        let (pending, url) = match connector::auth::begin_authorize(saved.as_ref(), &host_id) {
            Ok(p) => p,
            Err(e) => {
                fail(e.msg);
                return;
            }
        };
        // Keep the redirect target and verifier before the blocking wait.
        eprintln!("[notchbud] signin: listener on {}", pending.redirect_uri);
        // Open the approval page in the default browser; the clipboard
        // keeps a silent copy in case the handoff is blocked. The same
        // loopback callback completes either path.
        open_browser(&url);
        let _ = copy_text_to_clipboard(&url);
        let _ = PostMessageW(
            hwnd_raw,
            WM_CHAT,
            WPARAM(CHAT_STATUS),
            LPARAM(Box::into_raw(Box::new(
                "Approve in your browser to finish signing in.".to_string(),
            )) as isize),
        );
        eprintln!("[notchbud] signin: waiting for callback");
        let (cb, meta) = match connector::auth::wait_callback(pending) {
            Ok(c) => c,
            Err(e) => {
                fail(e.msg);
                return;
            }
        };
        let nonce = meta.nonce.clone();
        let data = match connector::auth::exchange_code(&meta, &cb.code, &cb.client_id) {
            Ok(d) => d,
            Err(e) => {
                fail(e.msg);
                return;
            }
        };
        match connector::auth::finish_signin(&data, &cb.client_id, &nonce, &host_id, saved.as_ref()) {
            Ok(t) => {
                // Persistence must not fail silently: without the saved grant
                // the next launch would be signed out again. Still connect
                // this session so a storage hiccup doesn't waste an approval.
                if let Err(e) = connector::auth::save_tokens(&t) {
                    eprintln!("[notchbud] credentials could not be saved ({e}); session continues without persistence");
                    let _ = PostMessageW(hwnd_raw, WM_CHAT, WPARAM(CHAT_CONNECTED), LPARAM(Box::into_raw(Box::new(t)) as isize));
                    fail("Signed in for now, but credentials could not be saved — sign-in won't survive a restart.".into());
                    return;
                }
                let _ = PostMessageW(hwnd_raw, WM_CHAT, WPARAM(CHAT_CONNECTED), LPARAM(Box::into_raw(Box::new(t)) as isize));
            }
            Err(e) => fail(e.msg),
        }
    });
}
fn copy_text_to_clipboard(text: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let mut ok = false;
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(h);
                if SetClipboardData(13 /* CF_UNICODETEXT */, HANDLE(h.0)).is_ok() {
                    ok = true;
                }
            }
        }
        let _ = CloseClipboard();
        ok
    }
}

/// Read Unicode text off the clipboard (for field paste). System-owned
/// handle: lock/unlock only, never freed.
fn get_clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let text = (|| {
            let h = GetClipboardData(13 /* CF_UNICODETEXT */).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p, len);
            let s = String::from_utf16(slice).ok();
            let _ = GlobalUnlock(HGLOBAL(h.0));
            s
        })();
        let _ = CloseClipboard();
        text
    }
}

/// Open the URL the way any app opens a link: hand it to the OS, which
/// routes it to the already-running default browser.
fn open_browser(url: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            HWND(std::ptr::null_mut()),
            windows::core::PCWSTR(op.as_ptr()),
            windows::core::PCWSTR(file.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// WM_KEY from the hook: onboarding edits the name field, otherwise the chat
/// input line. Enter submits. Chat keys only work while signed in — a
/// signed-out island shows just the sign-in pill, with nothing to type in.
/// Field shortcuts: Ctrl+C copies the field highlight (else the transcript
/// on Chat / the whole line elsewhere), Ctrl+X cuts it, Ctrl+V pastes,
/// Ctrl+A selects the line first, then the transcript.
fn handle_chat_key(app: &mut state::App, code: usize) {
    if code == KEY_COPY || code == KEY_SELECT_ALL || code == KEY_CUT || code == KEY_PASTE {
        // CUT/PASTE edit the visible input line: needs a field and no
        // running stream (typing is gated the same way).
        if code == KEY_CUT || code == KEY_PASTE {
            if app.needs_onboarding() || !app.chat.connected() || app.chat.streaming {
                return;
            }
            if code == KEY_CUT {
                if let Some(removed) = app.chat.input_sel.delete_selected(&mut app.chat.input) {
                    copy_text_to_clipboard(&removed);
                    app.chat.error = None;
                    render::draw_now(app);
                }
            } else if let Some(clip) = get_clipboard_text() {
                app.chat
                    .input_sel
                    .replace_selected(&mut app.chat.input, &clip, 400);
                app.chat.error = None;
                render::draw_now(app);
            }
            return;
        }
        // Input highlight wins over the viewport for copy; select-all takes
        // the line first and the transcript on the repeat.
        if code == KEY_COPY {
            if let Some(t) = app.chat.input_sel.selected(&app.chat.input) {
                if !t.is_empty() {
                    copy_text_to_clipboard(&t);
                }
                return;
            }
            if app.screen() == state::Screen::Chat {
                // COPY/SELECT-ALL on the Chat viewport work mid-stream, so
                // a partial reply can be grabbed while it arrives.
                let text = app.chat.copy_text();
                if !text.trim().is_empty() {
                    copy_text_to_clipboard(&text);
                }
            } else if !app.chat.input.trim().is_empty() {
                copy_text_to_clipboard(&app.chat.input.clone());
            }
            return;
        }
        if app.screen() == state::Screen::Chat {
            let total = state::utf16_len(&app.chat.input);
            let line_full =
                total > 0 && app.chat.input_sel.normalized(&app.chat.input) == Some((0, total));
            if total > 0 && !line_full {
                app.chat.input_sel.select_all(&app.chat.input);
            } else {
                app.chat.input_sel.clear();
                app.chat.select_all();
            }
            render::draw_now(app);
        } else if app.chat.connected() {
            app.chat.input_sel.select_all(&app.chat.input);
            render::draw_now(app);
        }
        return;
    }
    if app.needs_onboarding() {
        handle_name_key(app, code);
        return;
    }
    if !app.chat.connected() || app.chat.streaming {
        return; // no field to type in; silent while a stream runs
    }
    // typing targets the input line at the bottom of the slot; snap back
    app.chat.scroll = 0.0;
    match code {
        c if c == VK_BACK.0 as usize => {
            // Backspace clears a highlight, else pops a char.
            if app.chat.input_sel.normalized(&app.chat.input).is_some() {
                let _ = app.chat.input_sel.delete_selected(&mut app.chat.input);
            } else {
                app.chat.input.pop();
            }
            app.chat.error = None;
        }
        c if c == VK_ESCAPE.0 as usize => {
            app.chat.input.clear();
            app.chat.error = None;
            app.chat.clear_selection();
            app.chat.input_sel.clear();
        }
        c if c == VK_RETURN.0 as usize => submit_chat(app),
        c if (32..=0x10FFFF).contains(&c) => {
            if let Some(ch) = char::from_u32(c as u32) {
                if !ch.is_control() {
                    // Typing over a highlight replaces it (standard edit).
                    if app.chat.input_sel.normalized(&app.chat.input).is_some() {
                        let mut s = String::new();
                        s.push(ch);
                        app.chat
                            .input_sel
                            .replace_selected(&mut app.chat.input, &s, 400);
                        app.chat.error = None;
                    } else if app.chat.input.chars().count() < 400 {
                        app.chat.input.push(ch);
                        app.chat.error = None;
                    }
                }
            }
        }
        _ => {}
    }
    render::draw_now(app);
}

/// WM_KEY while the island asks for a name: same editing keys, capped at
/// NAME_MAX_CHARS. Enter persists (greeting and chat wait for sign-in);
/// Escape clears the draft. The draft is selectable/copyable/cuttable like
/// the chat line.
fn handle_name_key(app: &mut state::App, code: usize) {
    if code == KEY_COPY {
        if let Some(t) = app.name_sel.selected(&app.name_draft) {
            if !t.is_empty() {
                copy_text_to_clipboard(&t);
            }
        } else if !app.name_draft.trim().is_empty() {
            copy_text_to_clipboard(&app.name_draft.clone());
        }
        return;
    }
    if code == KEY_CUT {
        if let Some(removed) = app.name_sel.delete_selected(&mut app.name_draft) {
            copy_text_to_clipboard(&removed);
            app.name_error = None;
            render::draw_now(app);
        }
        return;
    }
    if code == KEY_PASTE {
        if let Some(clip) = get_clipboard_text() {
            app.name_sel
                .replace_selected(&mut app.name_draft, &clip, user::NAME_MAX_CHARS);
            app.name_error = None;
            render::draw_now(app);
        }
        return;
    }
    if code == KEY_SELECT_ALL {
        app.name_sel.select_all(&app.name_draft);
        render::draw_now(app);
        return;
    }
    match code {
        c if c == VK_BACK.0 as usize => {
            if app.name_sel.normalized(&app.name_draft).is_some() {
                let _ = app.name_sel.delete_selected(&mut app.name_draft);
            } else {
                app.name_draft.pop();
            }
            app.name_error = None;
        }
        c if c == VK_ESCAPE.0 as usize => {
            app.name_draft.clear();
            app.name_error = None;
            app.name_sel.clear();
        }
        c if c == VK_RETURN.0 as usize => submit_name(app),
        c if (32..=0x10FFFF).contains(&c) => {
            if let Some(ch) = char::from_u32(c as u32) {
                if !ch.is_control() {
                    if app.name_sel.normalized(&app.name_draft).is_some() {
                        let mut s = String::new();
                        s.push(ch);
                        app.name_sel.replace_selected(
                            &mut app.name_draft,
                            &s,
                            user::NAME_MAX_CHARS,
                        );
                        app.name_error = None;
                    } else if app.name_draft.chars().count() < user::NAME_MAX_CHARS
                    {
                        app.name_draft.push(ch);
                        app.name_error = None;
                    }
                }
            }
        }
        _ => {}
    }
    render::draw_now(app);
}

/// Onboarding submit: persist the name so the next launch greets directly.
/// Blank/too-long drafts stay on the prompt with the reason underneath.
fn submit_name(app: &mut state::App) {
    match user::save_name(&app.name_draft) {
        Ok(canonical) => {
            app.user_name = Some(canonical);
            app.name_draft.clear();
            app.name_error = None;
            app.name_sel.clear();
        }
        Err(e) => {
            eprintln!("[notchbud] save name failed: {}", e.detail);
            app.name_error = Some(e.ui.to_owned());
        }
    }
    render::draw_now(app);
}
/// Enter: push the user turn, spawn the connector thread for one streaming
/// response. Refresh happens on the chat thread (5-min margin), never UI.
fn submit_chat(app: &mut state::App) {
    let text = app.chat.input.trim().to_string();
    if text.is_empty() {
        return;
    }
    // Keys only reach here while signed in (handle_chat_key gates), so a
    // missing connection just drops a stale turn instead of stranding it.
    let Some(tokens) = app.chat.tokens.clone() else { return };
    if app.chat.streaming {
        return;
    }
    if !tokens.plan_ready() {
        // Identity-only sign-in: inference stays off until plan usage is
        // authorized (tray Connect re-runs OAuth with the full scope set).
        app.chat.error = Some("ChatGPT plan use isn't enabled — Connect again to authorize it.".into());
        render::draw_now(app);
        return;
    }
    app.chat.input.clear();
    app.chat.input_sel.clear();
    app.chat.push_history(state::Role::User, text);
    app.chat.streaming = true;
    app.chat.stream.clear();
    app.chat.error = None;
    app.chat.notice = None;
    CHAT_CANCEL.store(false, Ordering::Release);
    render::draw_now(app);

    let hwnd = app.hwnd.0 as isize;
    let history: Vec<state::Msg> = app
        .chat
        .history
        .iter()
        .map(|(role, text)| state::Msg { role: *role, text: text.clone() })
        .collect();
    let model = app.chat.model.clone();
    let user_name = app.user_name.clone();
    std::thread::spawn(move || unsafe {
        let hwnd = HWND(hwnd as *mut _);
        // 5-min-margin refresh; OpenAI rotates the refresh token per use, and
        // this thread is the only refresher while a turn runs.
        let tokens = match connector::auth::needs_refresh(&tokens) {
            false => tokens,
            true => match connector::auth::refresh_tokens(&tokens) {
                Ok(t) => {
                    if let Err(e) = connector::auth::save_tokens(&t) {
                        eprintln!("[notchbud] refreshed credentials could not be saved ({e}); this session continues, but the sign-in may not survive a restart");
                    }
                    let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_CONNECTED), LPARAM(Box::into_raw(Box::new(t.clone())) as isize));
                    t
                }
                Err(e) if e.expired => {
                    // Terminal: the saved grant is unusable. Clear it so the
                    // island falls back to the sign-in prompt.
                    connector::auth::delete_tokens();
                    let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_SIGNED_OUT), LPARAM(Box::into_raw(Box::new(e.msg)) as isize));
                    return;
                }
                // transient: the still-valid access token usually works
                Err(_) => tokens,
            },
        };
        let model = match model {
            Some(m) => m,
            None => {
                // The account catalog decides; no hardcoded fallback.
                match connector::auth::pick_model(&tokens) {
                    Ok(m) => {
                        let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_MODEL), LPARAM(Box::into_raw(Box::new(m.clone())) as isize));
                        m
                    }
                    Err(msg) => {
                        if msg == "expired" {
                            connector::auth::delete_tokens();
                            let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_SIGNED_OUT), LPARAM(Box::into_raw(Box::new("Sign-in expired. Connect again from the tray.".to_owned())) as isize));
                        } else {
                            let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_ERROR), LPARAM(Box::into_raw(Box::new(msg)) as isize));
                        }
                        return;
                    }
                }
            }
        };
        let post_delta = |d: String| {
            let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_DELTA), LPARAM(Box::into_raw(Box::new(d)) as isize));
        };
        let cancelled = || CHAT_CANCEL.load(Ordering::Relaxed);
        let result = connector::chat::stream_chat(&tokens, &model, &history, user_name.as_deref(), |ev| match ev {
            connector::chat::StreamEvent::Delta(d) => {
                if cancelled() {
                    false
                } else {
                    post_delta(d);
                    true
                }
            }
            connector::chat::StreamEvent::Done => false,
            connector::chat::StreamEvent::Error(m) => {
                let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_ERROR), LPARAM(Box::into_raw(Box::new(m)) as isize));
                false
            }
        });
        match result {
            Ok(()) => {
                let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_DONE), LPARAM(0));
            }
            Err(m) => {
                let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_ERROR), LPARAM(Box::into_raw(Box::new(m)) as isize));
            }
        }
    });
}

/// WM_CHAT from a connector thread.
fn handle_chat_msg(app: &mut state::App, wparam: WPARAM, lparam: LPARAM) {
    match wparam.0 {
        CHAT_DELTA => {
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            if app.chat.streaming {
                app.chat.stream.push_str(&s);
                render::draw_now(app);
            }
        }
        CHAT_DONE => {
            if app.chat.streaming {
                if !app.chat.stream.is_empty() {
                    let done = std::mem::take(&mut app.chat.stream);
                    app.chat.push_history(state::Role::Assistant, done);
                }
                app.chat.streaming = false;
                render::draw_now(app);
            }
        }
        CHAT_ERROR => {
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            if !CHAT_CANCEL.load(Ordering::Relaxed) {
                app.chat.streaming = false;
                app.chat.stream.clear();
                app.chat.error = Some(*s);
                // A failed sign-in must be retryable: drop the in-progress
                // flag so the island falls back to the
                // "Sign in with ChatGPT" prompt (click to retry).
                app.chat.signing_in = false;
                app.chat.notice = None;
            } else {
                // a cancelled leave: swallow the failure quietly
                CHAT_CANCEL.store(false, Ordering::Release);
            }
            render::draw_now(app);
        }
        CHAT_CONNECTED => {
            let t = unsafe { Box::from_raw(lparam.0 as *mut connector::auth::Tokens) };
            app.chat.tokens = Some(*t);
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = None;
            render::draw_now(app);
        }
        CHAT_STATUS => {
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            app.chat.notice = Some(*s);
            render::draw_now(app);
        }
        CHAT_SIGNED_OUT => {
            // Terminal auth failure (expired/revoked grant): the saved grant
            // is already deleted. Clear the UI back to the sign-in prompt.
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            app.chat.tokens = None;
            app.chat.model = None;
            app.chat.streaming = false;
            app.chat.stream.clear();
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = Some(*s);
            render::draw_now(app);
        }
        CHAT_MODEL => {
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            app.chat.model = Some(*s);
        }
        _ => {}
    }
}

fn reflow_on_dpi_change(app: &mut state::App, lparam: LPARAM) {
    unsafe {
        let suggested: RECT = core::ptr::read_unaligned(lparam.0 as *const RECT);
        app.scale = (suggested.right - suggested.left) as f32 / state::WINDOW_UNITS_W;
        let (w, h) = app.layout_size();
        app.width = w.round() as i32;
        app.height = h.round() as i32;
        let _ = SetWindowPos(
            app.hwnd, None,
            suggested.left, suggested.top,
            app.width, app.height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // refresh the cached origin the hit-test reads (avoids GetWindowRect
        // on every mouse move over the strip)
        let mut wr = RECT::default();
        let _ = GetWindowRect(app.hwnd, &mut wr);
        app.win_x = wr.left;
        app.win_y = wr.top;
        render::draw_now(app); // resizes its renderer on dim/scale change
        after_state_change(app);
    }
}

// ---------------------------------------------------------------------------
// startup
// ---------------------------------------------------------------------------

fn dpi_scale() -> f32 {
    unsafe {
        let monitor = MonitorFromPoint(windows::Win32::Foundation::POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 {
            dx as f32 / 96.0
        } else {
            GetDpiForSystem() as f32 / 96.0
        }
    }
}

fn main() -> Result<()> {
    unsafe {
        // --- DPI awareness (PER_MONITOR_AWARE_V2) ---
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;

        // --- single instance: Local\Notchbud.SingleInstance ---
        let mut name_wide: Vec<u16> = r"Local\Notchbud.SingleInstance".encode_utf16().collect();
        name_wide.push(0); // explicit NUL — r"...\0" is a literal backslash-zero
        let _mutex = CreateMutexW(None, true, windows::core::PCWSTR(name_wide.as_ptr()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Tell instead of vanishing: without a tray icon there is no
            // other way to know an older instance is still holding the app.
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxW, MB_ICONINFORMATION, MB_OK,
            };
            let _ = MessageBoxW(
                None,
                w!("Notchbud is already running (check Task Manager if you cannot see it)."),
                w!("Notchbud"),
                MB_OK | MB_ICONINFORMATION,
            );
            return Ok(()); // already running — exit after telling
        }

        let hmodule = GetModuleHandleW(None)?;
        let hinstance = HINSTANCE(hmodule.0);
        let scale = dpi_scale();

        // --- class "NotchWindow" + our wndproc ---
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_ARROW)?,
            lpszClassName: w!("NotchbudWindow"),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        // --- 640×190 layout-units × DPI scale, centered top of the primary screen ---
        let width = (scale * state::WINDOW_UNITS_W).round() as i32;
        let height = (scale * state::WINDOW_UNITS_H).round() as i32;
        let x = (GetSystemMetrics(SM_CXSCREEN) - width) / 2;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW,
            w!("NotchbudWindow"),
            w!("Notchbud"),
            windows::Win32::UI::WindowsAndMessaging::WS_POPUP,
            x,
            0,
            width,
            height,
            None,
            None,
            hinstance,
            None,
        )?;

        // --- App state ---
        let mut app = Box::new(state::App::new(hwnd, scale, width, height));
        app.chat.tokens = connector::auth::load_tokens(); // saved sign-in, if any
        app.user_name = user::load_name(); // saved name; None = first run onboarding
        app.win_x = x;
        app.win_y = 0;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

        // --- one-time Run-key self-heal (exe moved -> rewrite the path) ---
        prefs::heal_start_with_windows();

        // --- tray icon + explorer-restart hook (non-fatal: the island
        // runs fine without it; Shell_NotifyIcon NIM_ADD can return
        // E_ACCESSDENIED on machines where Explorer blocks the icon) ---
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if let Err(e) = tray::add_tray(hwnd) {
            eprintln!("[notchbud] tray unavailable, continuing without it: {e:#}");
        }
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_MSG.store(taskbar_created, Ordering::Relaxed);

        // --- keyboard hook for the chat input (never activates the window) ---
        install_key_hook(hwnd, hinstance);

        // first frame
        if let Some(app) = app_from(hwnd) {
            render::draw_now(app);
            after_state_change(app);
        }

        // --- message pump ---
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                break; // WM_QUIT
            }
        }
        Ok(())
    }
}
