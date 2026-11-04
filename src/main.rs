// dyland — minimal notch: idle sliver at the top of the screen; hovering
// expands the island and shows "Hello Ankit"; leaving collapses it.

use dyland::{connector, render, state};

mod prefs;
mod tray;

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::core::{w, Result};use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
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
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    GetSystemMetrics, GetWindowRect, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SetWindowsHookExW, SetWindowLongPtrW, SetTimer,
    SetWindowPos, GetWindowLongPtrW, ShowWindow, TranslateMessage, UnhookWindowsHookEx,
    GWLP_USERDATA, HHOOK, HTCLIENT, HTTRANSPARENT, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
    SM_CXSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNOACTIVATE, WNDCLASSEXW, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCHITTEST, WM_PAINT, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER,
    SW_SHOWNORMAL, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
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
// WM_CHAT wparam codes (lp carries a Box pointer where noted)
const CHAT_DELTA: usize = 0; // *String
const CHAT_DONE: usize = 1;
const CHAT_ERROR: usize = 2; // *String
const CHAT_CONNECTED: usize = 3; // *Tokens
const CHAT_SHOW_CODE: usize = 4; // *String (formatted user code)
const CHAT_MODEL: usize = 5; // *String (resolved model)

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
        // only plain typing: no Ctrl/Alt chords (shortcuts pass through)
        if GetAsyncKeyState(VK_CONTROL.0 as i32) >= 0 && GetAsyncKeyState(VK_MENU.0 as i32) >= 0 {
            let vk = kb.vkCode as u32;
            let post = |c: usize| {
                let _ = PostMessageW(
                    HWND(HOOK_HWND.load(Ordering::Relaxed) as *mut _),
                    WM_KEY,
                    WPARAM(c),
                    LPARAM(0),
                );
            };
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

        WM_MOUSEMOVE => {
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
        let chosen = tray::popup_menu(app.hwnd, app.chat.connected());
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
    connector::auth::delete_tokens();
    app.chat.tokens = None;
    app.chat.model = None;
    app.chat.streaming = false;
    app.chat.stream.clear();
    app.chat.history.clear();
    app.chat.user_code = None;
    CHAT_CANCEL.store(true, Ordering::Release);
    app.chat.error = Some("Disconnected from ChatGPT.".into());
    render::draw_now(app);
}

/// Tray "Connect ChatGPT": start the device flow on a thread, open the
/// approval page, clipboard the user code. The code also shows in the slot.
fn start_signin(app: &mut state::App) {
    if app.chat.tokens.is_some() || app.chat.signing_in {
        return;
    }
    app.chat.signing_in = true;
    let hwnd = app.hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        let hwnd_raw = HWND(hwnd as *mut _);
        let post_err = |m: String| {
            let _ = PostMessageW(hwnd_raw, WM_CHAT, WPARAM(CHAT_ERROR), LPARAM(Box::into_raw(Box::new(m)) as isize));
        };
        let post_tokens = |t: connector::auth::Tokens| {
            let _ = PostMessageW(hwnd_raw, WM_CHAT, WPARAM(CHAT_CONNECTED), LPARAM(Box::into_raw(Box::new(t)) as isize));
        };
        let login = match connector::auth::start_device_login() {
            Ok(l) => l,
            Err(e) => {
                post_err(e.msg);
                return;
            }
        };
        let code = format!("{}-{}", &login.user_code[..4], &login.user_code[4..]);
        let _ = PostMessageW(hwnd_raw, WM_CHAT, WPARAM(CHAT_SHOW_CODE), LPARAM(Box::into_raw(Box::new(code.clone())) as isize));
        copy_to_clipboard(hwnd_raw, &code);
        open_browser(hwnd_raw, connector::auth::VERIFICATION_URL);
        // poll until the user approves (or the code expires)
        loop {
            std::thread::sleep(std::time::Duration::from_millis(login.interval_ms));
            match connector::auth::poll_device_login(&login) {
                Ok(connector::auth::Poll::Pending) => continue,
                Ok(connector::auth::Poll::Connected(t)) => {
                    let _ = connector::auth::save_tokens(&t);
                    post_tokens(t);
                    break;
                }
                Err(e) => {
                    post_err(e.msg);
                    break;
                }
            }
        }
    });
}

fn copy_to_clipboard(hwnd: HWND, text: &str) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    unsafe {
        if OpenClipboard(hwnd).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(13 /* CF_UNICODETEXT */, HANDLE(h.0));
            }
        }
        let _ = CloseClipboard();
    }
}

fn open_browser(hwnd: HWND, url: &str) {
    unsafe {
        let mut wide: Vec<u16> = url.encode_utf16().collect();
        wide.push(0);
        let _ = ShellExecuteW(
            hwnd,
            w!("open"),
            windows::core::PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// WM_KEY from the hook: edit the chat input line. Enter submits.
fn handle_chat_key(app: &mut state::App, code: usize) {
    if !app.chat.connected() || app.chat.streaming {
        return; // silent while a stream runs; sign-in is tray-driven
    }
    // typing targets the input line at the bottom of the slot — snap back
    app.chat.scroll = 0.0;
    match code {
        c if c == VK_BACK.0 as usize => {
            app.chat.input.pop();
        }
        c if c == VK_ESCAPE.0 as usize => {
            app.chat.input.clear();
        }
        c if c == VK_RETURN.0 as usize => submit_chat(app),
        c if (32..=0x10FFFF).contains(&c) => {
            if let Some(ch) = char::from_u32(c as u32) {
                if app.chat.input.chars().count() < 400 {
                    app.chat.input.push(ch);
                }
            }
        }
        _ => {}
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
    let Some(tokens) = app.chat.tokens.clone() else { return };
    if app.chat.streaming {
        return;
    }
    app.chat.input.clear();
    app.chat.push_history(state::Role::User, text);
    app.chat.streaming = true;
    app.chat.stream.clear();
    app.chat.error = None;
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
    std::thread::spawn(move || unsafe {
        let hwnd = HWND(hwnd as *mut _);
        // 5-min-margin refresh; OpenAI rotates the refresh token per use, and
        // this thread is the only refresher while a turn runs.
        let tokens = match connector::auth::needs_refresh(&tokens) {
            false => tokens,
            true => match connector::auth::refresh_tokens(&tokens) {
                Ok(t) => {
                    let _ = connector::auth::save_tokens(&t);
                    let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_CONNECTED), LPARAM(Box::into_raw(Box::new(t.clone())) as isize));
                    t
                }
                Err(e) if e.expired => {
                    let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_ERROR), LPARAM(Box::into_raw(Box::new(e.msg)) as isize));
                    return;
                }
                // transient: the still-valid access token usually works
                Err(_) => tokens,
            },
        };
        let model = match model {
            Some(m) => m,
            None => {
                // plan's own model listing first; luna fallback — cached in App
                let m = connector::auth::pick_model(&tokens);
                let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_MODEL), LPARAM(Box::into_raw(Box::new(m.clone())) as isize));
                m
            }
        };
        let post_delta = |d: String| {
            let _ = PostMessageW(hwnd, WM_CHAT, WPARAM(CHAT_DELTA), LPARAM(Box::into_raw(Box::new(d)) as isize));
        };
        let cancelled = || CHAT_CANCEL.load(Ordering::Relaxed);
        let result = connector::chat::stream_chat(&tokens, &model, &history, |ev| match ev {
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
            app.chat.user_code = None;
            app.chat.error = None;
            render::draw_now(app);
        }
        CHAT_SHOW_CODE => {
            let s = unsafe { Box::from_raw(lparam.0 as *mut String) };
            app.chat.user_code = Some(*s);
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

        // --- single instance: Local\Dyland.SingleInstance (env dev override) ---
        let mut name_wide: Vec<u16> = match std::env::var("DYLAND_MUTEX_NAME") {
            Ok(s) => format!(r"Local\{s}").encode_utf16().collect(),
            Err(_) => r"Local\Dyland.SingleInstance".encode_utf16().collect(),
        };
        name_wide.push(0); // explicit NUL — r"...\0" is a literal backslash-zero
        let _mutex = CreateMutexW(None, true, windows::core::PCWSTR(name_wide.as_ptr()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(()); // already running — exit silently
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
            lpszClassName: w!("DylandWindow"),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        // --- 640×190 layout-units × DPI scale, centered top of the primary screen ---
        let width = (scale * state::WINDOW_UNITS_W).round() as i32;
        let height = (scale * state::WINDOW_UNITS_H).round() as i32;
        let x = (GetSystemMetrics(SM_CXSCREEN) - width) / 2;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW,
            w!("DylandWindow"),
            w!("Dyland"),
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
        app.win_x = x;
        app.win_y = 0;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);

        // --- one-time Run-key self-heal (exe moved -> rewrite the path) ---
        prefs::heal_start_with_windows();

        // --- tray icon + explorer-restart hook ---
        tray::add_tray(hwnd)?;
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_MSG.store(taskbar_created, Ordering::Relaxed);

        // --- keyboard hook for the chat input (never activates the window) ---
        install_key_hook(hwnd, hinstance);

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

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
