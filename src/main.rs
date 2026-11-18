// notchbud — minimal notch: idle sliver at the top of the screen; hovering
// expands the island (sign-in prompt when signed out, chat when signed in);
// leaving collapses it.
//
// Shell: Win32 window + Direct2D island silhouette + DComp spring motion.
// Content: WebView2 child inside the island rendering HTML/CSS (ui/).
// The window never activates — a low-level keyboard hook forwards keys
// into the page via island_web while the island is expanded.

// No console window in release builds: this is a pure GUI (Win32) app.
// Debug builds keep the console so the [notchbud] log lines stay visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use notchbud::{connector, island_web, render, sessions, shot, state};

mod prefs;
mod tray;

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{
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
    GetAsyncKeyState, GetKeyboardState, ToUnicode, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_BACK, VK_CONTROL, VK_ESCAPE, VK_MENU, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, GetWindowRect, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SetTimer, SetWindowsHookExW, SetWindowLongPtrW, SetWindowPos,
    GetWindowLongPtrW, ShowWindow, TranslateMessage, UnhookWindowsHookEx, GWLP_USERDATA, HHOOK,
    HICON, HTCLIENT, HTTRANSPARENT, IDC_ARROW, IMAGE_ICON, KBDLLHOOKSTRUCT, LoadImageW, LR_DEFAULTCOLOR,
    MSG, SM_CXSCREEN, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_SHOWNOACTIVATE, WNDCLASSEXW, WH_KEYBOARD_LL, WM_DESTROY, WM_DPICHANGED,
    WM_KEYDOWN, WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

// "TaskbarCreated" message id; re-adds the tray icon when explorer restarts
static TASKBAR_MSG: AtomicU32 = AtomicU32::new(0);

// one-shot: region swap after the close settles
const TIMER_IDLE_RGN: usize = 3;
// one-shot: reveal the web page once the open spring has settled, so DWM
// composites the live Chromium surface only after the shell stops moving
const TIMER_WEB_SHOW: usize = 4;

// Chat pane backend: Groq API key (https://console.groq.com/keys), sealed
// with DPAPI. External URLs always open in the system browser, never in the
// island WebView (navigating it away kills the island until restart).
pub const GROQ_KEYS_URL: &str = "https://console.groq.com/keys";

// chat wiring: connector thread(s) post into this pump via PostMessage.
const WM_CHAT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 10; // connector events
const WM_KEY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 11; // keyboard hook chars
// WM_KEY codes for island shortcuts (past the Unicode range).
const KEY_COPY: usize = 0x110000;
const KEY_SELECT_ALL: usize = 0x110001;
const KEY_CUT: usize = 0x110002;
const KEY_PASTE: usize = 0x110003;
// Arrow keys ride distinct non-Unicode codes so they never collide with
// printable scalars: Left Arrow is U+0025 '%' (37) in Unicode, so sharing
// 37-40 would mistype arrows as %&'( and eat real '%' input as caret motion.
const KEY_LEFT: usize = 0x110010;
const KEY_UP: usize = 0x110011;
const KEY_RIGHT: usize = 0x110012;
const KEY_DOWN: usize = 0x110013;
// WM_CHAT wparam codes (lp carries a Box pointer where noted)
const CHAT_DELTA: usize = 0; // *Box<(u32, String)> (turn gen, delta)
const CHAT_DONE: usize = 1; // turn generation as LPARAM
const CHAT_ERROR: usize = 2; // *Box<(u32, String)> (turn gen, error)
const CHAT_CONNECTED: usize = 3; // *ApiKey
const CHAT_MODEL: usize = 5; // *String (resolved model)
const CHAT_SIGNED_OUT: usize = 6; // *String (terminal auth failure: clear creds)
// Startup verification results (carry the verify generation so a stale
// check can't resurrect creds or clobber a disconnect).
const CHAT_VERIFY_OK: usize = 7; // *(u32, ApiKey)
const CHAT_VERIFY_OUT: usize = 8; // *(u32, String) (terminal: key revoked)
const CHAT_VERIFY_ERR: usize = 9; // *(u32, String) (transient: keep any live stream)
// Groq-titled label for a session (session_id, title); stale ids are ignored.
const CHAT_TITLE: usize = 10; // *Box<(String, String)>
// Screenshot captured for the prompt (data URL); attaches as pendingImage.
const CHAT_SHOT: usize = 11; // *Box<String>

static ISLAND_ACTIVE: AtomicBool = AtomicBool::new(false); // hook gate: island expanded
static CHAT_CANCEL: AtomicBool = AtomicBool::new(false); // island left mid-stream
static VERIFY_GEN: AtomicU32 = AtomicU32::new(0); // startup key-check generation
static TURN_GEN: AtomicU32 = AtomicU32::new(0); // chat turn generation
static HOOK_HWND: AtomicIsize = AtomicIsize::new(0); // where the hook posts
static KEYHOOK: AtomicIsize = AtomicIsize::new(0); // hook handle (unhook on quit)

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
        let _ = SetWindowRgn(app.hwnd, Some(rgn), false);
    }
}

// region follows every expanded/sliver transition. Web visibility is
// choreographed separately: hidden during the spring, revealed after it
// settles (TIMER_WEB_SHOW), so the compositor animates the shell alone.
fn after_state_change(app: &state::App) {
    unsafe { apply_window_region(app) };
}

/// Reveal the page now that the shell sits at scale 1.0.
fn show_web_now(app: &mut state::App) {
    island_web::set_visible(true);
    island_web::layout(app.scale);
    push_web(app);
    island_web::fade_in();
}

/// Push Rust state into the HTML island.
fn push_web(app: &mut state::App) {
    island_web::push_state(&app.page_state());
    app.last_web_push_ms = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
}

/// Streaming coalescing: at most one page render per ~48 ms while tokens
/// arrive; the final DONE always pushes.
fn push_web_throttled(app: &mut state::App) {
    let now = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
    if now.wrapping_sub(app.last_web_push_ms) >= 48 {
        push_web(app);
    }
}

// keep the island window region until the shrink finishes, so the collapse
// isn't clipped by the idle strip mid-flight (Nusa does the same); a one-shot
// timer fires after the settle and swaps regions to the idle hit strip.
unsafe fn schedule_idle_region(hwnd: HWND, settle: f32) {
    KillTimer(Some(hwnd), TIMER_IDLE_RGN).ok();
    SetTimer(Some(hwnd), TIMER_IDLE_RGN, (settle * 1000.0).ceil() as u32, None);
}

// Reveal the page after the open spring settles (matched to the original
// content choreography: greeting fades in only once the pop is done).
unsafe fn schedule_web_show(hwnd: HWND) {
    KillTimer(Some(hwnd), TIMER_WEB_SHOW).ok();
    let delay = web_show_delay_secs();
    SetTimer(Some(hwnd), TIMER_WEB_SHOW, (delay * 1000.0).ceil() as u32, None);
}

// Reveal the page the moment the open spring settles, so the compositor
// animates the shell alone. Kept small on purpose: every millisecond here is
// black island the user stares at. (The fade itself plays after reveal via
// fadeIn — waiting for it here too would double-count it.)
fn web_show_delay_secs() -> f32 {
    state::open_settle()
}

// One-shot hover-leave tracking for the parent window (consumed on fire).
unsafe fn arm_leave_tracking(hwnd: HWND) {
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    let _ = TrackMouseEvent(&mut tme);
}

/// Cursor position in window-client px (cached origin, no GetWindowRect).
/// None when the position cannot be read — callers treat that as outside.
fn cursor_client_px(app: &state::App) -> Option<(f32, f32)> {
    unsafe {
        let mut cpos = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cpos).ok()?;
        Some((
            (cpos.x - app.win_x) as f32,
            (cpos.y - app.win_y) as f32,
        ))
    }
}
// ---------------------------------------------------------------------------
// keyboard hook — the window never activates, so hover alone never grants
// keystrokes: a low-level hook forwards typed keys into the WebView2 page
// (via island_web) while the island is expanded, and eats them so they
// don't type into the app behind it too.
// ---------------------------------------------------------------------------

unsafe extern "system" fn key_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let is_down = wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize;
    if code >= 0 && is_down && ISLAND_ACTIVE.load(Ordering::Acquire) {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let post = |c: usize| {
            let _ = PostMessageW(
                Some(HWND(HOOK_HWND.load(Ordering::Relaxed) as *mut _)),
                WM_KEY,
                WPARAM(c),
                LPARAM(0),
            );
        };
        let ctrl_down = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
        let alt_down = GetAsyncKeyState(VK_MENU.0 as i32) < 0;
        // Island shortcuts work inside the page and never reach the app
        // behind it. Every other Ctrl-only chord is swallowed too: the
        // window never activates, so it would otherwise type into the app
        // behind the island. (Alt chords still pass so Alt+Tab etc. work;
        // Ctrl+Shift+Esc needs a mouse-leave first.)
        if ctrl_down && !alt_down {
            match kb.vkCode {
                0x43 => {
                    post(KEY_COPY);
                    return LRESULT(1);
                }
                0x41 => {
                    post(KEY_SELECT_ALL);
                    return LRESULT(1);
                }
                0x58 => {
                    post(KEY_CUT);
                    return LRESULT(1);
                }
                0x56 => {
                    post(KEY_PASTE);
                    return LRESULT(1);
                }
                _ => {
                    return LRESULT(1);
                }
            }
        }
        // only plain typing: no Ctrl/Alt chords (shortcuts pass through)
        if !ctrl_down && !alt_down {
            let vk = kb.vkCode as u32;
            // Arrows yield no text (ToUnicode finds nothing), so without an
            // explicit code they fall through to the app behind the island
            // and the caret never moves. They ride distinct non-Unicode
            // codes (see KEY_LEFT): the page moves the caret for those,
            // while '%' (37) stays a printable scalar.
            match vk {
                37 => {
                    post(KEY_LEFT);
                    return LRESULT(1);
                }
                38 => {
                    post(KEY_UP);
                    return LRESULT(1);
                }
                39 => {
                    post(KEY_RIGHT);
                    return LRESULT(1);
                }
                40 => {
                    post(KEY_DOWN);
                    return LRESULT(1);
                }
                8 | 13 | 27 => {
                    // VK_BACK VK_RETURN VK_ESCAPE.
                    post(vk as usize);
                    return LRESULT(1);
                }
                _ => {
                    // VK -> character via the active keyboard layout
                    let mut st = [0u8; 256];
                    let _ = GetKeyboardState(&mut st);
                    let mut buf = [0u16; 4];
                    let n = ToUnicode(vk, kb.scanCode, Some(&st), &mut buf, 0);
                    // n > 0: produced text; n < 0: dead key — never eaten
                    if n > 0 {
                        // Astral chars arrive as a surrogate pair: combine
                        // to one scalar. A lone surrogate has no scalar
                        // (JS fromCodePoint throws) — leave it to the app.
                        let scalar = if n >= 2
                            && (0xD800..=0xDBFF).contains(&buf[0])
                            && (0xDC00..=0xDFFF).contains(&buf[1])
                        {
                            0x10000
                                + ((buf[0] as u32 - 0xD800) << 10)
                                + (buf[1] as u32 - 0xDC00)
                        } else {
                            buf[0] as u32
                        };
                        if !(0xD800..=0xDFFF).contains(&scalar) {
                            post(scalar as usize);
                            return LRESULT(1);
                        }
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe fn install_key_hook(hwnd: HWND, hinstance: HINSTANCE) {
    let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), Some(hinstance), 0);
    if let Ok(h) = hook {
        KEYHOOK.store(h.0 as isize, Ordering::Release);
        HOOK_HWND.store(hwnd.0 as isize, Ordering::Release);
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
            let pt = state::screen_pt(app, lparam);
            let x = pt.x as f32;
            let y = pt.y as f32;
            let a = app.bar_area;
            if x >= a[0] && x < a[2] && y >= a[1] && y < a[3] {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }

        WM_MOUSEMOVE => {
            if !app.mouse_inside {
                app.mouse_inside = true;
                unsafe { arm_leave_tracking(hwnd); }
                // Choreography runs on closed->open transitions only (see
                // state::expand_on_mousemove). Moves while already expanded
                // (e.g. the parent strips around the WebView child) must not
                // restart the spring.
                if state::expand_on_mousemove(app.expanded) {
                    app.expanded = true;
                    ISLAND_ACTIVE.store(true, Ordering::Release);
                    // Kill a pending deferred-hide so it can't fire into the
                    // fresh open, then run the spring alone. The page stays
                    // however it is (hidden, or faded mid-close) and is
                    // reshown + faded in after the settle.
                    unsafe {
                        KillTimer(Some(hwnd), TIMER_IDLE_RGN).ok();
                    }
                    render::animate(true);
                    render::draw_now(app);
                    after_state_change(app); // island region now, stays clickable
                    island_web::layout(app.scale);
                    push_web(app); // paint while hidden so reveal shows content, not a flash
                    schedule_web_show(hwnd);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        WM_MOUSELEAVE => {
            // The WebView2 child owns the mouse while it is over the island:
            // the parent sees a leave when the cursor moves into the child.
            // Same predicate as the page's mouseleave (see
            // state::page_leave_collapses) so the two tracking domains can
            // never disagree and thrash close->open->close.
            if !state::page_leave_collapses(&app.bar_area, cursor_client_px(app)) {
                app.mouse_inside = false;
                return LRESULT(0);
            }
            collapse(app);
            LRESULT(0)
        }

        WM_TIMER => {
            match wparam.0 {
                TIMER_IDLE_RGN => {
                    KillTimer(Some(hwnd), TIMER_IDLE_RGN).ok();
                    // Post-settle teardown: the spring is done, so destroying
                    // the Chromium surface (and swapping the region) is
                    // invisible. Never hide mid-spring.
                    island_web::set_visible(false);
                    after_state_change(app);
                }
                TIMER_WEB_SHOW => {
                    KillTimer(Some(hwnd), TIMER_WEB_SHOW).ok();
                    // Late leave wins: only reveal if still expanded.
                    if app.expanded {
                        show_web_now(app);
                    }
                }
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
            LRESULT(0)
        }

        WM_PAINT => {
            render::draw_now(app);
            // validate: without this the update region never clears and the
            // resulting WM_PAINT storm starves everything in the queue
            let _ = ValidateRect(Some(hwnd), None);
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
            tray::remove_tray(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => {
            if msg == WM_CHAT {
                handle_chat_msg(app, wparam, lparam);
                LRESULT(0)
            } else if msg == WM_KEY {
                handle_chat_key(wparam.0);
                LRESULT(0)
            } else if msg == island_web::WM_WEBMSG {
                handle_webmsg(app, lparam);
                LRESULT(0)
            } else if msg == tray::TRAY_CALLBACK {
                // left click toggles the island, right click opens the menu
                let lowword = (lparam.0 & 0xFFFF) as u16 as i32;
                if lowword == WM_RBUTTONUP as i32 {
                    handle_tray_menu(app);
                } else if lowword == windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP as i32
                {
                    app.expanded = !app.expanded;
                    ISLAND_ACTIVE.store(app.expanded, Ordering::Release);
                    render::animate(app.expanded);
                    render::draw_now(app);
                    if app.expanded {
                        unsafe {
                            KillTimer(Some(hwnd), TIMER_IDLE_RGN).ok();
                        }
                        after_state_change(app);
                        island_web::layout(app.scale);
                        push_web(app); // paint while hidden so reveal shows content, not a flash
                        schedule_web_show(hwnd);
                    } else {
                        collapse(app);
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

/// New chat turn (or stream cancel): invalidate every in-flight chat worker.
/// Workers capture the generation and tag DELTA/DONE/ERROR with it; the
/// handler drops anything no longer current so a stale worker can neither
/// modify nor end a newer turn.
fn bump_turn() -> u32 {
    TURN_GEN.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

/// Shared collapse path: hover-out, tray toggle, and the page's own
/// mouseleave all land here.
fn collapse(app: &mut state::App) {
    app.mouse_inside = false;
    app.rename = false; // a rename step never survives leaving the island
    ISLAND_ACTIVE.store(false, Ordering::Release);
    app.expanded = false;
    // Next open starts on the base screen, never a stale overlay page.
    app.close_overlay();
    // the island left mid-stream: cancel the worker, but keep the partial
    // reply as an assistant turn (a mouse slip must not eat the answer).
    // The bump retires the worker: its late DELTA/DONE/ERROR is dropped by
    // generation, never applied to the next turn.
    if app.chat.streaming {
        CHAT_CANCEL.store(true, Ordering::Release);
        bump_turn();
        app.chat.streaming = false;
        if !app.chat.stream.is_empty() {
            let partial = std::mem::take(&mut app.chat.stream);
            app.chat.push_history(state::Role::Assistant, partial);
            archive_current(app);
        }
    }
    // Fade the page instead of hiding it: destroying the live Chromium
    // surface mid-spring hitches DWM in the first frames of the shrink
    // (the close lag). Opacity is composited inside Chromium — no DWM
    // topology change. The real hide is deferred to TIMER_IDLE_RGN,
    // post-settle, next to the region swap. This mirrors the frozen
    // choreography: content leaves during the CLOSE_DELAY hold, shell
    // shrinks alone after.
    island_web::fade_out();
    unsafe {
        KillTimer(Some(app.hwnd), TIMER_WEB_SHOW).ok();
    }
    render::animate(false);
    render::draw_now(app);
    // the region swaps only after the shrink settles, so the collapse
    // isn't clipped by the idle strip
    unsafe {
        schedule_idle_region(
            app.hwnd,
            state::CLOSE_SETTLE + state::CLOSE_DELAY + 0.12,
        )
    };
}

// ---------------------------------------------------------------------------
// behind the handlers
// ---------------------------------------------------------------------------

fn handle_tray_menu(app: &mut state::App) {
    unsafe {
        let chosen = tray::popup_menu(app.hwnd, app.chat.connected());
        match chosen as u32 {
            tray::ID_START => {
                let on = !prefs::is_start_with_windows();
                let _ = prefs::set_start_with_windows(on);
            }
            tray::ID_CONNECT => {
                // The key is entered in the island, not the tray: expand the
                // island so the sign-in page shows.
                app.expanded = true;
                ISLAND_ACTIVE.store(true, Ordering::Release);
                render::animate(true);
                render::draw_now(app);
                after_state_change(app);
                island_web::layout(app.scale);
                push_web(app); // paint while hidden so reveal shows content, not a flash
                schedule_web_show(app.hwnd);
            }
            tray::ID_DISCONNECT => disconnect(app),
            tray::ID_EXIT => PostQuitMessage(0),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// chat handling
// ---------------------------------------------------------------------------

/// Archive the live turns into the local sessions list (newest first,
/// capped, saved to sessions.json). No-op when there is nothing to keep.
fn archive_current(app: &mut state::App) {
    if app.chat.history.is_empty() {
        return;
    }
    if app.session_id.trim().is_empty() {
        app.session_id = sessions::new_id();
    }
    let snap = sessions::snapshot(&app.session_id, &app.session_title, &app.chat.history);
    sessions::upsert(&mut app.sessions, snap);
    sessions::save_sessions(&app.sessions);
}

/// Drop the camera attachment (full image + thumb) after send, remove,
/// or session switch. One place so the pair can't half-clear.
fn clear_shot(app: &mut state::App) {
    app.pending_image.clear();
    app.pending_thumb.clear();
}

/// Start a blank session (history cleared, fresh id). The previous turns
/// must already be archived — this only resets the working copy.
fn begin_session(app: &mut state::App) {
    app.chat.history.clear();
    app.chat.error = None;
    app.chat.notice = None;
    app.chat.streaming = false;
    app.chat.stream.clear();
    app.session_id = sessions::new_id();
    app.session_title.clear();
    clear_shot(app);
}

/// Quietly retire a live stream (Back / open_session / collapse-out).
/// A visible partial reply survives as an assistant turn (same as leaving
/// the island mid-stream); the caller archives it. Clearing it here would
/// eat the answer on Back / chat-switch.
fn cancel_live_stream(app: &mut state::App) {
    if app.chat.streaming {
        CHAT_CANCEL.store(true, Ordering::Release);
        bump_turn();
        app.chat.streaming = false;
        if !app.chat.stream.is_empty() {
            let partial = std::mem::take(&mut app.chat.stream);
            app.chat.push_history(state::Role::Assistant, partial);
        }
    }
}

/// One web intent string field. Missing/non-string reads as "".
fn web_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string()
}

fn disconnect(app: &mut state::App) {
    archive_current(app);
    connector::auth::delete_api_key();
    // A pending startup check must not resurrect creds or clobber this
    // state when it lands — invalidate its generation and drop the
    // in-progress flag so the island falls back to the key prompt.
    VERIFY_GEN.fetch_add(1, Ordering::Relaxed);
    app.chat.signing_in = false;
    app.chat.notice = None;
    app.chat.tokens = None;
    app.chat.model = None;
    app.chat.streaming = false;
    app.chat.stream.clear();
    app.chat.history.clear();
    app.session_id.clear();
    app.session_title.clear();
    clear_shot(app);
    CHAT_CANCEL.store(true, Ordering::Release);
    bump_turn(); // retire any live worker with the session
    app.chat.error = Some("Disconnected.".into());
    // Back to the base auth step (signin), never a stale settings page.
    app.close_overlay();
    push_web(app);
}

/// Island "Enter Groq API Key": validate the key against Groq's model
/// catalog on a worker thread, then persist it sealed with DPAPI.
fn save_api_key(app: &mut state::App, raw_key: &str) {
    if app.chat.signing_in || app.chat.streaming {
        return;
    }
    let key = raw_key.trim().to_owned();
    let Some(api_key) = connector::auth::ApiKey::new(&key) else {
        app.chat.error = Some("That doesn't look like a Groq API key (it starts with gsk_).".into());
        app.chat.notice = None;
        push_web(app);
        return;
    };
    app.chat.signing_in = true;
    app.chat.error = None;
    app.chat.notice = Some("Checking key…".into());
    // A stale cancel from a collapsed stream would swallow this validation's
    // result (CHAT_ERROR treats a set flag as "leave, ignore me"). No chat
    // stream can be live here (early return above), so clearing is safe.
    CHAT_CANCEL.store(false, Ordering::Release);
    push_web(app);
    let hwnd = app.hwnd.0 as isize;
    // Tag the validation result with the current turn so a chat started
    // meanwhile (which bumps the generation) isn't killed by its ERROR.
    let vturn = TURN_GEN.load(Ordering::Relaxed);
    std::thread::spawn(move || unsafe {
        let hwnd_raw = HWND(hwnd as *mut _);
        let fail = |m: String| {
            let _ = PostMessageW(
                Some(hwnd_raw),
                WM_CHAT,
                WPARAM(CHAT_ERROR),
                LPARAM(Box::into_raw(Box::new((vturn, m))) as isize),
            );
        };
        if let Err(e) = connector::auth::validate_api_key(&api_key.key) {
            fail(e.msg);
            return;
        }
        if let Err(e) = connector::auth::save_api_key(&api_key) {
            eprintln!("[notchbud] API key could not be saved ({e}); session continues without persistence");
            let _ = PostMessageW(
                Some(hwnd_raw),
                WM_CHAT,
                WPARAM(CHAT_CONNECTED),
                LPARAM(Box::into_raw(Box::new(api_key)) as isize),
            );
            fail("Signed in for now, but the key could not be saved — it won't survive a restart.".into());
            return;
        }
        let _ = PostMessageW(
            Some(hwnd_raw),
            WM_CHAT,
            WPARAM(CHAT_CONNECTED),
            LPARAM(Box::into_raw(Box::new(api_key)) as isize),
        );
    });
}

/// Boot check: a saved key may have been revoked since last run.
/// Re-validate with the existing catalog check on a worker thread. A dead
/// key drops to the key entry (SIGNED_OUT); a live one just clears the
/// "checking" flag (CONNECTED); anything else stays silent (ERROR clears
/// the flag with no user-visible status — Home never shows boot noise, and
/// a transient network blip must not claim anything until a real attempt).
fn verify_saved_key(app: &mut state::App) {
    let Some(api_key) = app.chat.tokens.clone() else {
        return; // nothing saved — already on the key entry.
    };
    if app.chat.signing_in || app.chat.streaming {
        return;
    }
    app.chat.signing_in = true;
    app.chat.error = None;
    app.chat.notice = None;
    push_web(app);
    let gen = VERIFY_GEN.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let hwnd = app.hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        let hwnd_raw = HWND(hwnd as *mut _);
        match connector::auth::validate_api_key(&api_key.key) {
            Ok(()) => {
                let _ = PostMessageW(
                    Some(hwnd_raw),
                    WM_CHAT,
                    WPARAM(CHAT_VERIFY_OK),
                    LPARAM(Box::into_raw(Box::new((gen, api_key))) as isize),
                );
            }
            Err(e) if e.expired => {
                // No delete here: the file goes only in CHAT_VERIFY_OUT,
                // after its generation check — a stale result must not
                // delete a key saved after this check ran.
                let _ = PostMessageW(
                    Some(hwnd_raw),
                    WM_CHAT,
                    WPARAM(CHAT_VERIFY_OUT),
                    LPARAM(Box::into_raw(Box::new((
                        gen,
                        connector::chat::AUTH_REJECTED_MSG.to_owned(),
                    ))) as isize),
                );
            }
            Err(e) => {
                let _ = PostMessageW(
                    Some(hwnd_raw),
                    WM_CHAT,
                    WPARAM(CHAT_VERIFY_ERR),
                    LPARAM(Box::into_raw(Box::new((gen, e.msg))) as isize),
                );
            }
        }
    });
}

/// Open an external URL in the system browser. The island WebView must never
/// navigate itself (one external click would replace the island page and
/// deaden every intent until restart), so links go out through here.
fn open_url_in_browser(url: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    unsafe {
        let op: Vec<u16> = "open\0".encode_utf16().collect();
        let mut file: Vec<u16> = url.encode_utf16().collect();
        file.push(0);
        let _ = ShellExecuteW(
            None,
            windows::core::PCWSTR(op.as_ptr()),
            windows::core::PCWSTR(file.as_ptr()),
            None,
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
}

/// Read Unicode text off the clipboard (for hook paste injection).
fn get_clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
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
            // Bound the NUL scan by the allocation size so a malformed
            // block without a terminator can't read out of bounds.
            let max = (GlobalSize(HGLOBAL(h.0)) / 2).min(1 << 20);
            let mut len = 0usize;
            while len < max && *p.add(len) != 0 {
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

/// WM_KEY from the hook: the page owns editing, so forward printable keys
/// and shortcuts straight into it. Typing stays live while a stream runs
/// (type-ahead: send still waits for DONE), so nothing typed is lost.
fn handle_chat_key(code: usize) {
    // Hook code -> page shortcut. Lookup, not a branch chain.
    const KEY_SHORTCUTS: [(usize, &str); 3] = [
        (KEY_COPY, "copy"),
        (KEY_SELECT_ALL, "selectAll"),
        (KEY_CUT, "cut"),
    ];
    if let Some((_, name)) = KEY_SHORTCUTS.iter().find(|(c, _)| *c == code) {
        island_web::forward_shortcut(name);
        return;
    }
    if code == KEY_PASTE {
        paste_into_page();
        return;
    }
    if code == VK_BACK.0 as usize
        || code == VK_RETURN.0 as usize
        || code == VK_ESCAPE.0 as usize
        || code == KEY_LEFT
        || code == KEY_UP
        || code == KEY_RIGHT
        || code == KEY_DOWN
        // Printable scalars only: lone surrogates have no scalar value
        // (JS String.fromCodePoint throws on them).
        || ((32..=0x10FFFF).contains(&code) && !(0xD800..=0xDFFF).contains(&code))
    {
        island_web::forward_key(code)
    }
}

/// Paste without focus: read the clipboard in Rust and inject it through
/// the same key path as physical typing (respects the page's 400-char cap).
fn paste_into_page() {
    if let Some(clip) = get_clipboard_text() {
        // Filtered chars are always printable Unicode scalars, so every
        // value fits the page's key range — forward directly.
        for ch in clip.chars().filter(|c| !c.is_control()).take(400) {
            island_web::forward_key(ch as u32 as usize);
        }
    } else {
        island_web::forward_shortcut("paste");
    }
}

/// WM_WEBMSG from the page: nav / save_key / open_keys / disconnect /
/// submit / mouseleave intents.
fn handle_webmsg(app: &mut state::App, lparam: LPARAM) {
    let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
    let v: serde_json::Value = match serde_json::from_str(&s) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[notchbud] webmsg parse: {e:#}");
            return;
        }
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("open_keys") => open_url_in_browser(GROQ_KEYS_URL),
        Some("save_key") => {
            let key = web_str(&v, "key");
            save_api_key(app, &key);
        }
        Some("signin") => {
            // New page posts save_key with the key; an old cached page may
            // post a bare signin — accept an optional key either way.
            let key = web_str(&v, "key");
            if !key.trim().is_empty() {
                save_api_key(app, &key);
            }
        }
        Some("cancel_signin") => {
            // Cancel must also swallow the worker's late result: the
            // CONNECTED/ERROR it already posted is ignored (signing_in
            // gate / CHAT_CANCEL) instead of signing in anyway.
            CHAT_CANCEL.store(true, Ordering::Release);
            app.chat.signing_in = false;
            app.chat.notice = None;
            push_web(app);
        }
        Some("save_name") => {
            // "What should we call you?" (Home prompt + Settings edit).
            // Plain-text local profile, not key-bound: disconnects keep it.
            let raw = web_str(&v, "name");
            let clean = prefs::save_display_name(&raw);
            if !clean.is_empty() {
                app.display_name = clean;
                app.rename = false;
            }
            push_web(app);
        }
        Some("edit_name") => {
            // Settings pencil: reopen the shared name step on Home, even
            // with a saved name. Signed-out or chatting intents are no-ops
            // (the pencil hides there); leaving the island cancels it.
            if app.chat.connected() && !app.chat.has_chatted() {
                app.rename = true;
                app.close_overlay();
            }
            push_web(app);
        }
        Some("disconnect") => disconnect(app),
        Some("new_chat") => {
            // Back from /chat: archive the turns so the session survives,
            // then reset the working copy to home. A live stream is
            // cancelled quietly (same as leaving the island mid-stream).
            cancel_live_stream(app);
            archive_current(app);
            begin_session(app);
            app.close_overlay();
            push_web(app);
        },
        Some("open_session") => {
            // Chats page: jump back into a saved session. The live turns
            // are archived first so nothing is lost. An unknown id is a
            // no-op: it must not cancel the live stream or archive.
            let id = web_str(&v, "id");
            if id.trim().is_empty() {
                return;
            }
            let Some(saved) = app.sessions.iter().find(|s| s.id == id).cloned() else {
                return;
            };
            cancel_live_stream(app);
            archive_current(app);
            app.chat.history.clear();
            for m in &saved.messages {
                let role = if m.role == "assistant" {
                    state::Role::Assistant
                } else {
                    state::Role::User
                };
                app.chat.push_history(role, m.text.clone());
            }
            app.session_id = saved.id;
            app.session_title = saved.title;
            app.chat.error = None;
            app.chat.notice = None;
            clear_shot(app);
            app.close_overlay();
            push_web(app);
        },
        Some("take_shot") => {
            // Camera button: capture the screen on a worker thread (GDI +
            // WIC take ~100ms — never stall the window pump). The result
            // lands via CHAT_SHOT and previews in the prompt.
            let hwnd = app.hwnd.0 as isize;
            std::thread::spawn(move || unsafe {
                let hwnd_raw = HWND(hwnd as *mut _);
                // Empty pair = failure (the CHAT_SHOT arm reports it
                // without touching any live stream).
                let urls = shot::capture_shot().unwrap_or_default();
                let _ = PostMessageW(
                    Some(hwnd_raw),
                    WM_CHAT,
                    WPARAM(CHAT_SHOT),
                    LPARAM(Box::into_raw(Box::new(urls)) as isize),
                );
            });
        },
        Some("clear_shot") => {
            // Preview remove: drop the attachment, hide the preview.
            clear_shot(app);
            push_web(app);
        },
        Some("ask_delete") => {
            // Chats row trash: pop the confirm page for a stored session.
            // Unknown ids are ignored — the confirm page is unreachable then.
            let id = web_str(&v, "id");
            if app.sessions.iter().any(|s| s.id == id) {
                app.pending_delete = id;
                app.overlay = state::Overlay::ConfirmDelete;
                push_web(app);
            }
        },
        Some("confirm_delete") => {
            // Confirm Yes: drop the stored session for good, back to the
            // list. When the deleted row is the live session, its working
            // copy dies with it (no re-archive — that would resurrect it).
            if !app.pending_delete.trim().is_empty() {
                let doomed = app.pending_delete.clone();
                sessions::remove(&mut app.sessions, &doomed);
                sessions::save_sessions(&app.sessions);
                if app.session_id == doomed {
                    begin_session(app);
                }
                app.pending_delete.clear();
            }
            app.overlay = state::Overlay::Chats;
            push_web(app);
        },
        Some("cancel_delete") => {
            // Confirm No (or Esc): keep the session, back to the list.
            app.pending_delete.clear();
            app.overlay = state::Overlay::Chats;
            push_web(app);
        },
        Some("nav") => {
            let target = v.get("target").and_then(|t| t.as_str()).unwrap_or("back");
            app.navigate(target);
            push_web(app);
        }
        Some("back") => {
            app.close_overlay();
            push_web(app);
        }
        Some("submit") => {
            let text = web_str(&v, "text");
            submit_chat(app, &text);
        }
        Some("mouseleave") => {
            if !app.expanded {
                return;
            }
            // The page can't tell "left to desktop" from "left to the parent
            // surface" — the child is inset in the island, so most exits
            // cross parent strips first. Collapsing here thrashed
            // close->open->close on every downward exit (page leave, then a
            // parent MOUSEMOVE re-expand, then the true leave). Only a cursor
            // outside the island bar is a true exit (see
            // state::page_leave_collapses); otherwise just make sure the
            // parent catches it.
            if state::page_leave_collapses(&app.bar_area, cursor_client_px(app)) {
                collapse(app);
            } else {
                app.mouse_inside = false;
                unsafe { arm_leave_tracking(app.hwnd) };
            }
        }
        _ => {}
    }
}

/// Enter: push the user turn, spawn the connector thread for one streaming
/// response.
fn submit_chat(app: &mut state::App, raw: &str) {
    let text = raw.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some(api_key) = app.chat.tokens.clone() else { return };
    if app.chat.streaming {
        return;
    }
    // First turn of a blank island starts a session id up front so the
    // title worker can attach to it.
    if app.session_id.trim().is_empty() {
        app.session_id = sessions::new_id();
    }
    let session_id = app.session_id.clone();
    // The camera shot (if any) rides this turn only, then clears: the full
    // image goes to the model, the thumb stays on the history turn above
    // the bubble. Archive still stores text only.
    let shot = if app.pending_image.trim().is_empty() {
        None
    } else {
        Some((
            std::mem::take(&mut app.pending_image),
            std::mem::take(&mut app.pending_thumb),
        ))
    };
    if let Some((_, thumb)) = &shot {
        app.chat
            .push_history_image(state::Role::User, text, thumb.clone());
    } else {
        app.chat.push_history(state::Role::User, text);
    }
    // Archive the user turn immediately: Back mid-stream keeps the prompt.
    archive_current(app);
    let needs_title = app.session_title.trim().is_empty();
    let first_user_text = app
        .chat
        .history
        .iter()
        .find(|(r, _, _)| *r == state::Role::User)
        .map(|(_, t, _)| t.clone())
        .unwrap_or_default();
    app.chat.streaming = true;
    app.chat.stream.clear();
    app.chat.error = None;
    app.chat.notice = None;
    CHAT_CANCEL.store(false, Ordering::Release);
    let turn = bump_turn(); // this worker owns the island until the next turn
    // A sent prompt owns the island — drop any overlay (menu/notifications/
    // settings) so the live stream shows on the chat page.
    app.close_overlay();
    push_web(app);

    let hwnd = app.hwnd.0 as isize;
    let history: Vec<state::Msg> = app
        .chat
        .history
        .iter()
        .map(|(role, text, _)| state::Msg { role: *role, text: text.clone() })
        .collect();
    let model = app.chat.model.clone();
    let user_name = app.display_name.clone(); // frozen per turn, like history
    std::thread::spawn(move || unsafe {
        let hwnd = HWND(hwnd as *mut _);
        // Screenshot turns always run on the Qwen vision model — the text
        // models can't see image_url parts. Used for this turn only, never
        // persisted (no CHAT_MODEL post): the next text turn resolves its
        // model as usual. Resolved before the catalog so a catalog error
        // can't block a screenshot turn that needs no catalog pick.
        let mut current = if shot.is_some() {
            connector::auth::SCREENSHOT_MODEL.to_owned()
        } else {
            match model {
                Some(m) => m,
                None => {
                    // The key's catalog decides; no hardcoded fallback.
                    match connector::auth::pick_model(&api_key.key) {
                        Ok(m) => {
                            let _ = PostMessageW(
                                Some(hwnd),
                                WM_CHAT,
                                WPARAM(CHAT_MODEL),
                                LPARAM(Box::into_raw(Box::new(m.clone())) as isize),
                            );
                            m
                        }
                        Err(msg) => {
                            if msg == "expired" {
                                connector::auth::delete_api_key();
                                let _ = PostMessageW(
                                    Some(hwnd),
                                    WM_CHAT,
                                    WPARAM(CHAT_SIGNED_OUT),
                                    LPARAM(Box::into_raw(Box::new(
                                        connector::chat::AUTH_REJECTED_MSG.to_owned(),
                                    )) as isize),
                                );
                            } else {
                                let _ = PostMessageW(
                                    Some(hwnd),
                                    WM_CHAT,
                                    WPARAM(CHAT_ERROR),
                                    LPARAM(Box::into_raw(Box::new((turn, msg))) as isize),
                                );
                            }
                            return;
                        }
                    }
                }
            }
        };
        let shot_ref = shot.as_ref().map(|(full, _)| full.as_str());
        let post_delta = |d: String| {
            let _ = PostMessageW(
                Some(hwnd),
                WM_CHAT,
                WPARAM(CHAT_DELTA),
                LPARAM(Box::into_raw(Box::new((turn, d))) as isize),
            );
        };
        let cancelled = || CHAT_CANCEL.load(Ordering::Relaxed);
        // The reply as it arrives: after Done this titles the session
        // alongside the user's message (see the title call below).
        let mut reply = String::new();
        // One retry: a project-blocked model (403) falls through to the
        // next catalog model instead of failing the turn.
        let mut retried = false;
        loop {
            let result = connector::chat::stream_chat(&api_key.key, &current, &user_name, &history, shot_ref, |ev| match ev {
                connector::chat::StreamEvent::Delta(d) => {
                    if cancelled() {
                        false
                    } else {
                        let d = connector::chat::strip_citation_markers(&d);
                        reply.push_str(&d);
                        post_delta(d);
                        true
                    }
                }
                connector::chat::StreamEvent::Done => false,
                connector::chat::StreamEvent::Error(m) => {
                    if m == connector::chat::AUTH_REJECTED_MSG {
                        connector::auth::delete_api_key();
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_CHAT,
                            WPARAM(CHAT_SIGNED_OUT),
                            LPARAM(Box::into_raw(Box::new(m)) as isize),
                        );
                    } else {
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_CHAT,
                            WPARAM(CHAT_ERROR),
                            LPARAM(Box::into_raw(Box::new((turn, m))) as isize),
                        );
                    }
                    false
                }
            });
            match result {
                Ok(()) => {
                    let _ = PostMessageW(Some(hwnd), WM_CHAT, WPARAM(CHAT_DONE), LPARAM(turn as isize));
                    // Separate title prompt, once per session, after the
                    // reply lands: a short Groq label from BOTH sides of
                    // the first exchange for the Chats list. Best-effort —
                    // a failure just leaves the snippet fallback. A
                    // cancelled turn never titles: its partial reply would
                    // label the session from a cut-off answer.
                    if needs_title && !cancelled() {
                        // Re-strip the assembled reply: a citation split
                        // across two deltas only completes here.
                        let clean = connector::chat::strip_citation_markers(&reply);
                        if let Some(title) = connector::chat::fetch_title(
                            &api_key.key,
                            &current,
                            &first_user_text,
                            &clean,
                        )
                        {
                            let _ = PostMessageW(
                                Some(hwnd),
                                WM_CHAT,
                                WPARAM(CHAT_TITLE),
                                LPARAM(
                                    Box::into_raw(Box::new((session_id.clone(), title)))
                                        as isize,
                                ),
                            );
                        }
                    }
                    break;
                }
                Err(m) if m == connector::chat::AUTH_REJECTED_MSG => {
                    // Terminal auth rejection: drop the credential and the
                    // cached model, return to key entry via SIGNED_OUT.
                    connector::auth::delete_api_key();
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_CHAT,
                        WPARAM(CHAT_SIGNED_OUT),
                        LPARAM(Box::into_raw(Box::new(m)) as isize),
                    );
                    break;
                }
                Err(m) if m == connector::chat::MODEL_UNAVAILABLE_MSG && !retried => {
                    // Image turns stay on the vision model: retrying with a
                    // catalog text model would resend image_url to a model
                    // that rejects it. Fail the turn instead.
                    if shot.is_some() {
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_CHAT,
                            WPARAM(CHAT_ERROR),
                            LPARAM(Box::into_raw(Box::new((turn, m))) as isize),
                        );
                        break;
                    }
                    retried = true;
                    match connector::auth::pick_model_excluding(&api_key.key, &[current.as_str()]) {
                        Ok(next) if next != current => {
                            let _ = PostMessageW(
                                Some(hwnd),
                                WM_CHAT,
                                WPARAM(CHAT_MODEL),
                                LPARAM(Box::into_raw(Box::new(next.clone())) as isize),
                            );
                            current = next;
                            continue;
                        }
                        _ => {
                            let _ = PostMessageW(
                                Some(hwnd),
                                WM_CHAT,
                                WPARAM(CHAT_ERROR),
                                LPARAM(Box::into_raw(Box::new((turn, m))) as isize),
                            );
                            break;
                        }
                    }
                }
                Err(m) => {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_CHAT,
                        WPARAM(CHAT_ERROR),
                        LPARAM(Box::into_raw(Box::new((turn, m))) as isize),
                    );
                    break;
                }
            }
        }
    });
}

/// WM_CHAT from a connector thread.
fn handle_chat_msg(app: &mut state::App, wparam: WPARAM, lparam: LPARAM) {
    match wparam.0 {
        CHAT_DELTA => {
            let (gen, s) = unsafe { *Box::from_raw(lparam.0 as *mut (u32, String)) };
            if gen != TURN_GEN.load(Ordering::Relaxed) {
                return; // stale worker — a newer turn owns the island
            }
            if app.chat.streaming {
                app.chat.stream.push_str(&s);
                // Re-strip the whole buffer: a marker split across two deltas
                // only completes here, and the slot shows this text live.
                app.chat.stream = connector::chat::strip_citation_markers(&app.chat.stream);
                push_web_throttled(app);
            }
        }
        CHAT_DONE => {
            if lparam.0 as u32 != TURN_GEN.load(Ordering::Relaxed) {
                return; // stale worker — must not end a newer turn
            }
            if app.chat.streaming {
                if !app.chat.stream.is_empty() {
                    let done = std::mem::take(&mut app.chat.stream);
                    app.chat.push_history(state::Role::Assistant, done);
                }
                app.chat.streaming = false;
                archive_current(app);
                push_web(app);
            }
        }
        CHAT_SHOT => {
            // Camera capture landed: preview it in the prompt. A capture
            // that arrives mid-stream still previews — it attaches to the
            // NEXT submit, never the live turn. Empty pair = capture failed.
            let (url, thumb) = unsafe { *Box::from_raw(lparam.0 as *mut (String, String)) };
            if url.trim().is_empty() {
                app.chat.error = Some("Couldn't capture the screen.".into());
            } else {
                app.pending_image = url;
                app.pending_thumb = thumb;
            }
            push_web(app);
        }
        CHAT_TITLE => {
            let b = unsafe { Box::from_raw(lparam.0 as *mut (String, String)) };
            let (sid, title) = (&b.0, b.1.trim().to_owned());
            if title.is_empty() {
                return;
            }
            // Stale title (user started a new session meanwhile): attach it
            // to the matching archived session instead of the live one.
            // First title wins: a second request for the same session
            // (fast follow-up turn before the first lands) must not
            // overwrite it.
            if app.session_id == *sid && app.session_title.trim().is_empty() {
                app.session_title = title.clone();
            }
            if let Some(s) = app.sessions.iter_mut().find(|s| &s.id == sid) {
                if s.title.trim().is_empty() {
                    s.title = title.clone();
                }
            } else if app.session_id == *sid {
                let snap =
                    sessions::snapshot(&app.session_id, &app.session_title, &app.chat.history);
                sessions::upsert(&mut app.sessions, snap);
            } else {
                return;
            }
            sessions::save_sessions(&app.sessions);
            push_web(app);
        }
        CHAT_ERROR => {
            let (gen, s) = unsafe { *Box::from_raw(lparam.0 as *mut (u32, String)) };
            if gen != TURN_GEN.load(Ordering::Relaxed) {
                return; // stale worker — must not fail a newer turn
            }
            if !CHAT_CANCEL.load(Ordering::Relaxed) {
                app.chat.streaming = false;
                app.chat.stream.clear();
                // A blocked model pick must not stick: drop it so the next
                // turn re-resolves from the catalog instead of retrying it.
                if s.as_str() == connector::chat::MODEL_UNAVAILABLE_MSG {
                    app.chat.model = None;
                }
                app.chat.error = Some(s);
                // A failed key check must be retryable: drop the in-progress
                // flag so the island falls back to the key prompt.
                app.chat.signing_in = false;
                app.chat.notice = None;
            } else {
                // a cancelled leave: swallow the failure quietly
                CHAT_CANCEL.store(false, Ordering::Release);
            }
            push_web(app);
        }
        CHAT_CONNECTED => {
            let t = unsafe { Box::from_raw(lparam.0 as *mut connector::auth::ApiKey) };
            if !app.chat.signing_in {
                return; // stale (cancelled sign-in, later disconnect) — never resurrect
            }
            app.chat.tokens = Some(*t);
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = None;
            push_web(app);
        }
        CHAT_SIGNED_OUT => {
            // Terminal auth failure (rejected key): the saved key is already
            // deleted. Clear the UI back to the key prompt, and invalidate
            // any pending startup verification so its delayed VERIFY_OK
            // can't resurrect these creds via the stale-result check.
            VERIFY_GEN.fetch_add(1, Ordering::Relaxed);
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            archive_current(app);
            app.chat.tokens = None;
            app.chat.model = None;
            app.chat.streaming = false;
            app.chat.stream.clear();
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = Some(*s);
            push_web(app);
        }
        CHAT_MODEL => {
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            app.chat.model = Some(*s);
        }
        CHAT_VERIFY_OK => {
            let b = unsafe { Box::from_raw(lparam.0 as *mut (u32, connector::auth::ApiKey)) };
            if b.0 != VERIFY_GEN.load(Ordering::Relaxed) {
                return; // stale check (e.g. after disconnect) — never resurrect creds
            }
            app.chat.tokens = Some(b.1);
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = None;
            push_web(app);
        }
        CHAT_VERIFY_OUT => {
            let b = unsafe { Box::from_raw(lparam.0 as *mut (u32, String)) };
            if b.0 != VERIFY_GEN.load(Ordering::Relaxed) {
                return; // stale — disconnect already owns the state
            }
            // Current: this check ran on the saved key, so the rejection
            // deletes it here, not in the worker.
            connector::auth::delete_api_key();
            archive_current(app);
            app.chat.tokens = None;
            app.chat.model = None;
            app.chat.streaming = false;
            app.chat.stream.clear();
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = Some(b.1);
            push_web(app);
        }
        CHAT_VERIFY_ERR => {
            let b = unsafe { Box::from_raw(lparam.0 as *mut (u32, String)) };
            if b.0 != VERIFY_GEN.load(Ordering::Relaxed) {
                return; // stale — leave the newer state alone
            }
            // Transient check failure at boot (e.g. network blip): stay
            // silent so Home never shows boot noise, and never tears down
            // a chat submitted while the check was pending. The next real
            // attempt (chat/save_key) surfaces its own error if still failing.
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = None;
            push_web(app);
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
            app.hwnd,
            None,
            suggested.left,
            suggested.top,
            app.width,
            app.height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // refresh the cached origin the hit-test reads (avoids GetWindowRect
        // on every mouse move over the strip)
        let mut wr = RECT::default();
        let _ = GetWindowRect(app.hwnd, &mut wr);
        app.win_x = wr.left;
        app.win_y = wr.top;
        render::draw_now(app); // resizes its renderer on dim/scale change
        island_web::layout(app.scale);
        after_state_change(app);
    }
}

// ---------------------------------------------------------------------------
// startup
// ---------------------------------------------------------------------------

fn dpi_scale() -> f32 {
    unsafe {
        let monitor = MonitorFromPoint(
            windows::Win32::Foundation::POINT { x: 0, y: 0 },
            MONITOR_DEFAULTTOPRIMARY,
        );
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
        // Cat-logo icon from the exe resources (assets/app.rc, group id 1).
        // Missing resources fall back to null so startup never aborts; the
        // window-creation flow accepts null icon handles.
        let hicon_big: HICON = LoadImageW(
            Some(hinstance),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            32,
            32,
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();
        let hicon_sm: HICON = LoadImageW(
            Some(hinstance),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            16,
            16,
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            hIcon: hicon_big,
            hIconSm: hicon_sm,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
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
            Some(hinstance),
            None,
        )?;

        // --- App state ---
        let mut app = Box::new(state::App::new(hwnd, scale, width, height));
        app.chat.tokens = connector::auth::load_api_key(); // saved key, if any
        app.display_name = prefs::load_display_name(); // saved name, if any
        app.sessions = sessions::load_sessions(); // past chats, if any
        app.win_x = x;
        app.win_y = 0;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

        // --- one-time Run-key self-heal (exe moved -> rewrite the path) ---
        prefs::heal_start_with_windows();

        // --- tray icon + explorer-restart hook (non-fatal) ---
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if let Err(e) = tray::add_tray(hwnd) {
            eprintln!("[notchbud] tray unavailable, continuing without it: {e:#}");
        }
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_MSG.store(taskbar_created, Ordering::Relaxed);

        // --- keyboard hook for the island input (never activates the window) ---
        install_key_hook(hwnd, hinstance);

        // --- WebView2 content host (transparent child over the island) ---
        island_web::init(hwnd, scale);
        island_web::layout(scale);

        // first frame
        if let Some(app) = app_from(hwnd) {
            render::draw_now(app);
            after_state_change(app);
            verify_saved_key(app);
        }

        // --- message pump ---
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 != 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                break; // WM_QUIT
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn webmsg(json: &str) -> LPARAM {
        LPARAM(Box::into_raw(Box::new(json.to_owned())) as isize)
    }

    fn blank_app() -> state::App {
        state::App::new(HWND(std::ptr::null_mut()), 1.0, 640, 190)
    }

    /// One test at a time may point NOTCHBUD_DATA_DIR elsewhere: env vars
    /// are process-global and tests run on parallel threads.
    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points NOTCHBUD_DATA_DIR at a fresh temp dir for the test's lifetime,
    /// holding the env lock so parallel tests can't interfere, and removes
    /// the dir on drop. The real %APPDATA% profile is never touched.
    struct TempDataDir {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: std::path::PathBuf,
        prev: Option<std::ffi::OsString>,
    }

    impl TempDataDir {
        fn set() -> Self {
            // Tolerate a poisoned lock (a panicked test): dirs are unique
            // per test, and set() always re-points the override.
            let lock = TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = format!("notchbud-test-{}-{n}", std::process::id());
            // Prefer the OS temp dir; fall back to the repo target dir when
            // writes outside the workspace are denied (sandboxed runs).
            let ws_fallback =
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
            let dir = [std::env::temp_dir(), ws_fallback]
                .into_iter()
                .map(|b| b.join(&name))
                .find_map(|d| std::fs::create_dir_all(&d).ok().map(|()| d))
                .expect("no writable temp dir for tests");
            let prev = std::env::var_os("NOTCHBUD_DATA_DIR");
            std::env::set_var("NOTCHBUD_DATA_DIR", &dir);
            TempDataDir { _lock: lock, dir, prev }
        }
    }

    impl Drop for TempDataDir {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("NOTCHBUD_DATA_DIR", v),
                None => std::env::remove_var("NOTCHBUD_DATA_DIR"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn nav_intent_opens_settings() {
        let mut app = blank_app();
        handle_webmsg(&mut app, webmsg(r#"{"type":"nav","target":"settings"}"#));
        assert_eq!(app.overlay, state::Overlay::Settings);
        // Toggling the active icon closes it back.
        handle_webmsg(&mut app, webmsg(r#"{"type":"nav","target":"settings"}"#));
        assert_eq!(app.overlay, state::Overlay::None);
    }

    #[test]
    fn new_chat_archives_history_back_to_home() {
        // Back from /chat: the turns archive into the local sessions list
        // (Back no longer loses the session), then the working copy resets
        // to home. A live stream is cancelled quietly.
        let _data = TempDataDir::set();
        let mut app = blank_app();
        app.chat.tokens =
            Some(connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap());
        app.chat.push_history(state::Role::User, "hi".into());
        app.chat.streaming = true;
        app.chat.stream = "half…".into();
        assert_eq!(app.screen(), state::Screen::Chat);
        handle_webmsg(&mut app, webmsg(r#"{"type":"new_chat"}"#));
        assert!(app.chat.history.is_empty());
        assert!(!app.chat.streaming);
        assert!(app.chat.stream.is_empty());
        assert_eq!(app.screen(), state::Screen::Home);
        assert_eq!(app.sessions.len(), 1, "the turn must survive Back");
    }

    #[test]
    fn open_session_restores_archived_turns() {
        // Chats page: opening a session restores its turns into the working
        // copy; the live turns archive first so nothing is lost.
        let _data = TempDataDir::set();
        let mut app = blank_app();
        app.chat.tokens =
            Some(connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap());
        app.session_id = "live".into();
        app.chat.push_history(state::Role::User, "live q".into());
        app.sessions.push(sessions::Session {
            id: "saved".into(),
            title: "Saved Title".into(),
            updated_ms: 1,
            messages: vec![sessions::StoredTurn {
                role: "user".into(),
                text: "saved q".into(),
            }],
        });
        handle_webmsg(&mut app, webmsg(r#"{"type":"open_session","id":"saved"}"#));
        assert_eq!(app.chat.history.len(), 1);
        assert_eq!(app.chat.history[0].1, "saved q");
        assert_eq!(app.session_id, "saved");
        assert_eq!(app.session_title, "Saved Title");
        assert_eq!(app.sessions.len(), 2, "live turns archive alongside");
        assert_eq!(app.screen(), state::Screen::Chat);
    }

    #[test]
    fn delete_flow_asks_confirms_and_cancels() {
        // Trash -> confirm page; Yes drops the session and returns to the
        // list; No (or Esc) keeps it. Unknown ids never reach confirm.
        let _data = TempDataDir::set();
        let mut app = blank_app();
        for id in ["s1", "s2"] {
            app.sessions.push(sessions::Session {
                id: id.into(),
                title: format!("T{id}"),
                updated_ms: 0,
                messages: vec![],
            });
        }
        handle_webmsg(&mut app, webmsg(r#"{"type":"ask_delete","id":"nope"}"#));
        assert_eq!(app.overlay, state::Overlay::None);
        assert!(app.pending_delete.is_empty());
        handle_webmsg(&mut app, webmsg(r#"{"type":"ask_delete","id":"s1"}"#));
        assert_eq!(app.overlay, state::Overlay::ConfirmDelete);
        assert_eq!(app.pending_delete, "s1");
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).expect("page_state must stay valid JSON");
        assert_eq!(v["page"], "confirm");
        assert_eq!(
            v["deleteTitle"], "Ts1",
            "confirm page must name the chat Yes would drop, got {v:?}"
        );
        handle_webmsg(&mut app, webmsg(r#"{"type":"cancel_delete"}"#));
        assert_eq!(app.overlay, state::Overlay::Chats);
        assert!(app.pending_delete.is_empty());
        assert_eq!(app.sessions.len(), 2, "No must keep the session");
        handle_webmsg(&mut app, webmsg(r#"{"type":"ask_delete","id":"s1"}"#));
        handle_webmsg(&mut app, webmsg(r#"{"type":"confirm_delete"}"#));
        assert_eq!(app.overlay, state::Overlay::Chats);
        assert!(app.pending_delete.is_empty());
        assert_eq!(
            app.sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s2"],
            "Yes must drop exactly the confirmed session"
        );
    }

    #[test]
    fn shot_preview_shows_and_clears() {
        // CHAT_SHOT (worker result) previews the data URL; clear_shot
        // drops it. Both push the page state the preview reads.
        let mut app = blank_app();
        let msg = Box::into_raw(Box::new((
            "data:image/png;base64,AAA".to_owned(),
            "data:image/png;base64,BBB".to_owned(),
        )));
        handle_chat_msg(&mut app, WPARAM(CHAT_SHOT), LPARAM(msg as isize));
        assert_eq!(app.pending_image, "data:image/png;base64,AAA");
        assert_eq!(app.pending_thumb, "data:image/png;base64,BBB");
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).expect("page_state must stay valid JSON");
        assert_eq!(v["pendingImage"], "data:image/png;base64,BBB");
        handle_webmsg(&mut app, webmsg(r#"{"type":"clear_shot"}"#));
        assert!(app.pending_image.is_empty());
        assert!(app.pending_thumb.is_empty());
        let v: serde_json::Value =
            serde_json::from_str(&app.page_state()).expect("page_state must stay valid JSON");
        assert!(v["pendingImage"].is_null());
        // A fresh session never carries a stale attachment.
        app.pending_image = "data:image/png;base64,AAA".into();
        app.pending_thumb = "data:image/png;base64,BBB".into();
        begin_session(&mut app);
        assert!(app.pending_image.is_empty());
        assert!(app.pending_thumb.is_empty());
    }

    #[test]
    fn chat_title_attaches_to_live_and_archived_session() {
        // The title arm saves the sessions file: isolate it in a temp dir.
        let _data = TempDataDir::set();
        let mut app = blank_app();
        app.session_id = "s1".into();
        app.chat
            .push_history(state::Role::User, "plan a trip".into());
        let msg = Box::into_raw(Box::new(("s1".to_owned(), "Trip Ideas".to_owned())));
        // push_web is best-effort (no WebView on the test host: island_web
        // guards on READY so it no-ops).
        handle_chat_msg(&mut app, WPARAM(CHAT_TITLE), LPARAM(msg as isize));
        assert_eq!(app.session_title, "Trip Ideas");
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].title, "Trip Ideas");
    }

    #[test]
    fn save_key_with_junk_sets_error_without_validating() {
        // A format-invalid key is rejected synchronously: no key check, no
        // network thread, the island stays on the key prompt with a hint.
        let mut app = blank_app();
        handle_webmsg(&mut app, webmsg(r#"{"type":"save_key","key":"junk"}"#));
        assert!(!app.chat.signing_in);
        assert!(
            app.chat
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("gsk_"),
            "error was {:?}",
            app.chat.error
        );
    }

    #[test]
    fn open_keys_points_at_groq_console() {
        assert_eq!(GROQ_KEYS_URL, "https://console.groq.com/keys");
    }

    #[test]
    fn save_name_stores_trimmed_name_and_ignores_blank() {
        // save_name persists the display-name file: isolate it in a temp dir.
        let _data = TempDataDir::set();
        let mut app = blank_app();
        handle_webmsg(&mut app, webmsg(r#"{"type":"save_name","name":"  Alex  "}"#));
        assert_eq!(app.display_name, "Alex");
        assert!(app.greeting().contains("Alex"));
        // Blank never clears an existing name.
        handle_webmsg(&mut app, webmsg(r#"{"type":"save_name","name":"   "}"#));
        assert_eq!(app.display_name, "Alex");
        // Overlong input caps at the stored max.
        let long = "x".repeat(40);
        handle_webmsg(
            &mut app,
            webmsg(&format!(r#"{{"type":"save_name","name":"{long}"}}"#)),
        );
        assert_eq!(app.display_name.len(), crate::prefs::DISPLAY_NAME_MAX);
        // edit_name (settings pencil) reopens the shared step: overlay
        // drops, the prompt owns Home until save (or leave).
        app.chat.tokens =
            Some(connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap());
        app.display_name = "Alex".into();
        app.overlay = state::Overlay::Settings;
        handle_webmsg(&mut app, webmsg(r#"{"type":"edit_name"}"#));
        assert!(app.rename);
        assert_eq!(app.overlay, state::Overlay::None);
        let v: serde_json::Value = serde_json::from_str(&app.page_state()).unwrap();
        assert_eq!(v["needsName"], true);
        handle_webmsg(&mut app, webmsg(r#"{"type":"save_name","name":"Bob"}"#));
        assert!(!app.rename);
        assert_eq!(app.display_name, "Bob");
        // Signed out: a stale edit intent is a no-op.
        let mut signed_out = blank_app();
        handle_webmsg(&mut signed_out, webmsg(r#"{"type":"edit_name"}"#));
        assert!(!signed_out.rename);
    }

    #[test]
    fn boot_without_saved_key_skips_verification() {
        // No key on disk: nothing to check, no thread, still signed out.
        let mut app = blank_app();
        verify_saved_key(&mut app);
        assert!(!app.chat.signing_in);
        assert!(app.chat.notice.is_none());
        assert_eq!(app.screen(), state::Screen::SignIn);
    }

    #[test]
    fn boot_transient_verify_error_stays_silent_on_home() {
        // A boot-check network blip must not paint Home: no notice, no
        // error, creds kept, still on Home. The next real attempt reports
        // its own failure if still failing.
        let mut app = blank_app();
        app.chat.tokens =
            Some(connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap());
        app.chat.signing_in = true;
        let gen = VERIFY_GEN.load(Ordering::Relaxed);
        let msg = Box::into_raw(Box::new((gen, "Couldn't reach Groq. Try again.".to_owned())));
        handle_chat_msg(&mut app, WPARAM(CHAT_VERIFY_ERR), LPARAM(msg as isize));
        assert!(!app.chat.signing_in);
        assert!(app.chat.notice.is_none());
        assert!(app.chat.error.is_none());
        assert!(app.chat.tokens.is_some());
        assert_eq!(app.screen(), state::Screen::Home);
    }

    #[test]
    fn boot_dead_key_returns_to_key_entry() {
        // The SIGNED_OUT the boot check posts for a rejected key: the
        // credential and cached model go, the island lands on key entry.
        let mut app = blank_app();
        app.chat.tokens =
            Some(connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap());
        app.chat.model = Some("openai/gpt-oss-20b".into());
        let msg = Box::into_raw(Box::new(
            connector::chat::AUTH_REJECTED_MSG.to_owned(),
        ));
        handle_chat_msg(&mut app, WPARAM(CHAT_SIGNED_OUT), LPARAM(msg as isize));
        assert!(app.chat.tokens.is_none());
        assert!(app.chat.model.is_none());
        assert_eq!(app.screen(), state::Screen::SignIn);
    }

    #[test]
    fn signed_out_invalidates_pending_verification() {
        // Terminal sign-out bumps the verify generation: a delayed
        // VERIFY_OK from the boot check must fail the stale-result check
        // and leave credentials cleared, never resurrect them.
        let mut app = blank_app();
        app.chat.tokens =
            Some(connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap());
        let gen = VERIFY_GEN.load(Ordering::Relaxed);
        let msg = Box::into_raw(Box::new(
            connector::chat::AUTH_REJECTED_MSG.to_owned(),
        ));
        handle_chat_msg(&mut app, WPARAM(CHAT_SIGNED_OUT), LPARAM(msg as isize));
        assert!(app.chat.tokens.is_none());
        let stale = Box::into_raw(Box::new((
            gen,
            connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap(),
        )));
        handle_chat_msg(&mut app, WPARAM(CHAT_VERIFY_OK), LPARAM(stale as isize));
        assert!(app.chat.tokens.is_none());
        assert_eq!(app.screen(), state::Screen::SignIn);
    }

    #[test]
    fn stale_turn_messages_are_dropped() {
        // A cancelled worker's late DELTA/DONE/ERROR must not touch the
        // newer turn. The temp-dir guard also holds the env lock, so no
        // parallel test can bump TURN_GEN mid-assert.
        let _data = TempDataDir::set();
        let mut app = blank_app();
        app.chat.tokens =
            Some(connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap());
        app.chat.streaming = true;
        app.chat.stream = "new".into();
        let cur = bump_turn();
        let stale = cur.wrapping_sub(1);
        handle_chat_msg(&mut app, WPARAM(CHAT_DONE), LPARAM(stale as isize));
        assert!(app.chat.streaming, "stale DONE must not end the turn");
        assert_eq!(app.chat.stream, "new");
        let msg = Box::into_raw(Box::new((stale, "boom".to_owned())));
        handle_chat_msg(&mut app, WPARAM(CHAT_ERROR), LPARAM(msg as isize));
        assert!(app.chat.streaming, "stale ERROR must not fail the turn");
        assert!(app.chat.error.is_none());
        let msg = Box::into_raw(Box::new((stale, "old".to_owned())));
        handle_chat_msg(&mut app, WPARAM(CHAT_DELTA), LPARAM(msg as isize));
        assert_eq!(app.chat.stream, "new", "stale DELTA must not append");
        handle_chat_msg(&mut app, WPARAM(CHAT_DONE), LPARAM(cur as isize));
        assert!(!app.chat.streaming, "current DONE completes the turn");
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn cancelled_signin_ignores_late_connected() {
        // Cancel is real: a worker CONNECTED posted before the cancel lands
        // while signing_in=false is dropped, never resurrecting creds.
        let mut app = blank_app();
        let msg = Box::into_raw(Box::new(
            connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap(),
        ));
        handle_chat_msg(&mut app, WPARAM(CHAT_CONNECTED), LPARAM(msg as isize));
        assert!(app.chat.tokens.is_none());
        // A live sign-in still connects.
        app.chat.signing_in = true;
        let msg = Box::into_raw(Box::new(
            connector::auth::ApiKey::new("gsk_test1234567890abcdef").unwrap(),
        ));
        handle_chat_msg(&mut app, WPARAM(CHAT_CONNECTED), LPARAM(msg as isize));
        assert!(app.chat.tokens.is_some());
        assert!(!app.chat.signing_in);
    }

    #[test]
    fn web_reveal_delay_within_budget() {
        // Hover-to-content budget: the shell spring is measured physics, the
        // reveal must not pad it — every millisecond here is black island.
        assert!(
            web_show_delay_secs() <= 0.85,
            "reveal takes {}s",
            web_show_delay_secs()
        );
    }
}
