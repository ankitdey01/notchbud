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

use notchbud::{connector, island_web, render, state};

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
// WM_CHAT wparam codes (lp carries a Box pointer where noted)
const CHAT_DELTA: usize = 0; // *String
const CHAT_DONE: usize = 1;
const CHAT_ERROR: usize = 2; // *String
const CHAT_CONNECTED: usize = 3; // *ApiKey
const CHAT_MODEL: usize = 5; // *String (resolved model)
const CHAT_SIGNED_OUT: usize = 6; // *String (terminal auth failure: clear creds)

static ISLAND_ACTIVE: AtomicBool = AtomicBool::new(false); // hook gate: island expanded
static CHAT_CANCEL: AtomicBool = AtomicBool::new(false); // island left mid-stream
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
        // behind it. Other Ctrl/Alt chords still pass through.
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
                _ => {}
            }
        }
        // only plain typing: no Ctrl/Alt chords (shortcuts pass through)
        if !ctrl_down && !alt_down {
            let vk = kb.vkCode as u32;
            match vk {
                8 | 13 | 27 => {
                    // VK_BACK VK_RETURN VK_ESCAPE
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
                handle_chat_key(app, wparam.0);
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

/// Shared collapse path: hover-out, tray toggle, and the page's own
/// mouseleave all land here.
fn collapse(app: &mut state::App) {
    app.mouse_inside = false;
    ISLAND_ACTIVE.store(false, Ordering::Release);
    app.expanded = false;
    // Next open starts on the base screen, never a stale overlay page.
    app.close_overlay();
    // the island left mid-stream: default cancel — the partial
    // stream is discarded silently
    if app.chat.streaming {
        CHAT_CANCEL.store(true, Ordering::Release);
        app.chat.streaming = false;
        app.chat.stream.clear();
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

fn disconnect(app: &mut state::App) {
    connector::auth::delete_api_key();
    app.chat.tokens = None;
    app.chat.model = None;
    app.chat.streaming = false;
    app.chat.stream.clear();
    app.chat.history.clear();
    app.chat.notice = None;
    CHAT_CANCEL.store(true, Ordering::Release);
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
    std::thread::spawn(move || unsafe {
        let hwnd_raw = HWND(hwnd as *mut _);
        let fail = |m: String| {
            let _ = PostMessageW(
                Some(hwnd_raw),
                WM_CHAT,
                WPARAM(CHAT_ERROR),
                LPARAM(Box::into_raw(Box::new(m)) as isize),
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
/// and shortcuts straight into it. Paste injects via the hook path so it
/// works without OS focus; everything else is browser-native.
fn handle_chat_key(app: &mut state::App, code: usize) {
    if code == KEY_COPY {
        island_web::forward_shortcut("copy");
        return;
    }
    if code == KEY_SELECT_ALL {
        island_web::forward_shortcut("selectAll");
        return;
    }
    if code == KEY_CUT {
        island_web::forward_shortcut("cut");
        return;
    }
    if code == KEY_PASTE {
        paste_into_page();
        return;
    }
    if app.chat.streaming {
        return; // silent while a stream runs
    }
    if code == VK_BACK.0 as usize
        || code == VK_RETURN.0 as usize
        || code == VK_ESCAPE.0 as usize
        || (32..=0x10FFFF).contains(&code)
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
            let key = v.get("key").and_then(|t| t.as_str()).unwrap_or("").to_string();
            save_api_key(app, &key);
        }
        Some("signin") => {
            // New page posts save_key with the key; an old cached page may
            // post a bare signin — accept an optional key either way.
            let key = v.get("key").and_then(|t| t.as_str()).unwrap_or("").to_string();
            if !key.trim().is_empty() {
                save_api_key(app, &key);
            }
        }
        Some("cancel_signin") => {
            // Validation is a short request; cancel just clears the flag.
            app.chat.signing_in = false;
            app.chat.notice = None;
            push_web(app);
        }
        Some("disconnect") => disconnect(app),
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
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
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
    app.chat.push_history(state::Role::User, text);
    app.chat.streaming = true;
    app.chat.stream.clear();
    app.chat.error = None;
    app.chat.notice = None;
    CHAT_CANCEL.store(false, Ordering::Release);
    // A sent prompt owns the island — drop any overlay (menu/notifications/
    // settings) so the live stream shows on the chat page.
    app.close_overlay();
    push_web(app);

    let hwnd = app.hwnd.0 as isize;
    let history: Vec<state::Msg> = app
        .chat
        .history
        .iter()
        .map(|(role, text)| state::Msg { role: *role, text: text.clone() })
        .collect();
    let model = app.chat.model.clone();
    std::thread::spawn(move || unsafe {
        let hwnd = HWND(hwnd as *mut _);
        let mut current = match model {
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
                                    "That Groq API key was rejected — enter a new key.".to_owned(),
                                )) as isize),
                            );
                        } else {
                            let _ = PostMessageW(
                                Some(hwnd),
                                WM_CHAT,
                                WPARAM(CHAT_ERROR),
                                LPARAM(Box::into_raw(Box::new(msg)) as isize),
                            );
                        }
                        return;
                    }
                }
            }
        };
        let post_delta = |d: String| {
            let _ = PostMessageW(
                Some(hwnd),
                WM_CHAT,
                WPARAM(CHAT_DELTA),
                LPARAM(Box::into_raw(Box::new(d)) as isize),
            );
        };
        let cancelled = || CHAT_CANCEL.load(Ordering::Relaxed);
        // One retry: a project-blocked model (403) falls through to the
        // next catalog model instead of failing the turn.
        let mut retried = false;
        loop {
            let result = connector::chat::stream_chat(&api_key.key, &current, &history, |ev| match ev {
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
                            LPARAM(Box::into_raw(Box::new(m)) as isize),
                        );
                    }
                    false
                }
            });
            match result {
                Ok(()) => {
                    let _ = PostMessageW(Some(hwnd), WM_CHAT, WPARAM(CHAT_DONE), LPARAM(0));
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
                                LPARAM(Box::into_raw(Box::new(m)) as isize),
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
                        LPARAM(Box::into_raw(Box::new(m)) as isize),
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
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            if app.chat.streaming {
                app.chat.stream.push_str(&s);
                push_web_throttled(app);
            }
        }
        CHAT_DONE => {
            if app.chat.streaming {
                if !app.chat.stream.is_empty() {
                    let done = std::mem::take(&mut app.chat.stream);
                    app.chat.push_history(state::Role::Assistant, done);
                }
                app.chat.streaming = false;
                push_web(app);
            }
        }
        CHAT_ERROR => {
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            if !CHAT_CANCEL.load(Ordering::Relaxed) {
                app.chat.streaming = false;
                app.chat.stream.clear();
                // A blocked model pick must not stick: drop it so the next
                // turn re-resolves from the catalog instead of retrying it.
                if s.as_str() == connector::chat::MODEL_UNAVAILABLE_MSG {
                    app.chat.model = None;
                }
                app.chat.error = Some(*s);
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
            app.chat.tokens = Some(*t);
            app.chat.signing_in = false;
            app.chat.notice = None;
            app.chat.error = None;
            push_web(app);
        }
        CHAT_SIGNED_OUT => {
            // Terminal auth failure (rejected key): the saved key is already
            // deleted. Clear the UI back to the key prompt.
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
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
