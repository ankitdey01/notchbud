// island_web.rs — WebView2 content host inside the Win32 island.
//
// The Win32 window + Direct2D shell + DComp springs stay exactly as they
// were. All *content* (greeting, status, history, inputs) now renders in
// HTML/CSS inside a WebView2 child window positioned over the island.
//
// Threading: everything here runs on the UI thread (the message pump).
// The low-level keyboard hook never calls in directly — it posts WM_KEY
// like before, and the wndproc forwards into `forward_key`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::cell::RefCell;

use windows::core::Interface;
use windows::Win32::Foundation::{HWND, RECT, WPARAM, LPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use webview2_com::{
    Microsoft::Web::WebView2::Win32::*,
    CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler,
    NavigationCompletedEventHandler, WebMessageReceivedEventHandler, take_pwstr,
};

use crate::state::{ISLAND_H, ISLAND_W, ISLAND_X};

/// Web -> Rust intents arrive here. wparam unused; lparam is
/// `Box<String>` (JSON text) to free.
pub const WM_WEBMSG: u32 = WM_APP + 20;

static READY: AtomicBool = AtomicBool::new(false);
static PARENT_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

// COM interfaces are apartment-threaded: UI thread only, hence thread-local.
thread_local! {
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    static WEBVIEW: RefCell<Option<ICoreWebView2>> = const { RefCell::new(None) };
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn with_controller(f: impl FnOnce(&ICoreWebView2Controller)) {
    CONTROLLER.with_borrow(|c| {
        if let Some(ctrl) = c.as_ref() {
            f(ctrl);
        }
    });
}

fn with_webview(f: impl FnOnce(&ICoreWebView2)) {
    WEBVIEW.with_borrow(|w| {
        if let Some(wv) = w.as_ref() {
            f(wv);
        }
    });
}

// --- embedded UI (one navigation, no local server, no file dependency) ---

const INDEX_HTML: &str = include_str!("../ui/index.html");
const ISLAND_CSS: &str = include_str!("../ui/styles.css");
const ISLAND_JS: &str = include_str!("../ui/app.js");
// Cat logo for the island brand mark (96px PNG data URI, regenerated with
// `python -c ... web/b694186d-374a-49e0-a172-f7abfd6d9327.png ... ui/logo.b64`).
const LOGO_DATAURI: &str = include_str!("../ui/logo.b64");

fn composed_html() -> String {
    let css = format!("<style>{ISLAND_CSS}</style>");
    let js = format!("<script>{ISLAND_JS}</script>");
    INDEX_HTML
        .replacen("<!--NOTCHBUD_CSS-->", &css, 1)
        .replacen("<!--NOTCHBUD_JS-->", &js, 1)
        .replace("__NOTCHBUD_LOGO__", LOGO_DATAURI.trim())
}

/// Island content bounds in physical px: the shell with a small inset so
/// text never kisses the rounded corners.
pub fn island_rect(scale: f32) -> RECT {
    let left = (ISLAND_X + 12.0) * scale;
    let right = (ISLAND_X + ISLAND_W - 12.0) * scale;
    RECT {
        left: left.round() as i32,
        top: (10.0 * scale).round() as i32,
        right: right.round() as i32,
        bottom: ((ISLAND_H - 12.0) * scale).round() as i32,
    }
}

/// Create the WebView2 child. Called once, on the UI thread, after the
/// Win32 window exists. Blocks briefly (pumping messages) until the
/// controller is ready, then navigates to the embedded island page.
pub fn init(parent: HWND, scale: f32) {
    unsafe { let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); }

    PARENT_HWND.store(parent.0 as isize, Ordering::Relaxed);

    // --- environment ---
    let (etx, erx) = mpsc::channel();
    let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result: windows::core::Result<()>, env: Option<ICoreWebView2Environment>| {
            let _ = etx.send((result, env));
            Ok(())
        },
    ));
    let env = unsafe {
        match CreateCoreWebView2Environment(&env_handler) {
            Ok(()) => match webview2_com::wait_with_pump(erx) {
                Ok((Ok(()), Some(env))) => env,
                Ok((Err(e), _)) => {
                    eprintln!("[notchbud] webview env failed: {e:#}");
                    return;
                }
                _ => {
                    eprintln!("[notchbud] webview env failed: no environment");
                    return;
                }
            },
            Err(e) => {
                eprintln!("[notchbud] CreateCoreWebView2Environment: {e:#}");
                return;
            }
        }
    };

    // --- controller (child window of the island) ---
    let (ctx, crx) = mpsc::channel();
    let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |result: windows::core::Result<()>, ctrl: Option<ICoreWebView2Controller>| {
            let _ = ctx.send((result, ctrl));
            Ok(())
        },
    ));
    let controller = unsafe {
        match env.CreateCoreWebView2Controller(parent, &ctrl_handler) {
            Ok(()) => match webview2_com::wait_with_pump(crx) {
                Ok((Ok(()), Some(c))) => c,
                Ok((Err(e), _)) => {
                    eprintln!("[notchbud] webview controller failed: {e:#}");
                    return;
                }
                _ => {
                    eprintln!("[notchbud] webview controller failed: none");
                    return;
                }
            },
            Err(e) => {
                eprintln!("[notchbud] CreateCoreWebView2Controller: {e:#}");
                return;
            }
        }
    };

    unsafe {
        let bounds = island_rect(scale);
        let _ = controller.SetBounds(bounds);
        let _ = controller.SetIsVisible(false); // shown on first expand

        let wv = match controller.CoreWebView2() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[notchbud] CoreWebView2: {e:#}");
                return;
            }
        };

        // Transparent so the Direct2D shell shows through; HTML paints text.
        if let Ok(ctrl2) = controller.cast::<ICoreWebView2Controller2>() {
            let _ = ctrl2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                A: 0,
                R: 0,
                G: 0,
                B: 0,
            });
        }
        if let Ok(settings) = wv.Settings() {
            let _ = settings.SetAreDefaultContextMenusEnabled(false);
            let _ = settings.SetAreDevToolsEnabled(cfg!(debug_assertions));
            let _ = settings.SetIsScriptEnabled(true);
        }

        // Rust <- Web intents: repost to the main hwnd (stays on UI thread).
        let mut token = 0i64;
        let raw_parent = parent.0 as isize;
        let _ = wv.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(
                move |_wv: Option<ICoreWebView2>,
                      args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
                    let Some(a) = args else { return Ok(()) };
                    let mut msg = windows::core::PWSTR::null();
                    if a.WebMessageAsJson(&mut msg).is_ok() {
                        let json = take_pwstr(msg);
                        let hwnd = HWND(raw_parent as *mut _);
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_WEBMSG,
                            WPARAM(0),
                            LPARAM(Box::into_raw(Box::new(json)) as isize),
                        );
                    }
                    Ok(())
                },
            )),
            &mut token,
        );

        // Flush the last state once the page is live.
        let mut nav_token = 0i64;
        let _ = wv.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(
                move |_wv: Option<ICoreWebView2>, _args: Option<ICoreWebView2NavigationCompletedEventArgs>| {
                    flush_pending();
                    Ok(())
                },
            )),
            &mut nav_token,
        );

        let html = composed_html();
        let pw = CoTaskMemPWSTR::from(html.as_str());
        if let Err(e) = wv.NavigateToString(*pw.as_ref().as_pcwstr()) {
            eprintln!("[notchbud] NavigateToString: {e:#}");
            return;
        }

        CONTROLLER.with_borrow_mut(|c| *c = Some(controller.clone()));
        WEBVIEW.with_borrow_mut(|w| *w = Some(wv));
        READY.store(true, Ordering::Release);
    }
}

/// Reposition the child over the island (DPI change / resize).
pub fn layout(scale: f32) {
    let bounds = island_rect(scale);
    with_controller(|c| unsafe {
        let _ = c.SetBounds(bounds);
    });
}

/// Show the web content only at rest scale. Never called mid-spring:
/// showing/hiding the HWND forces a DWM topology rebuild that hitches a
/// running scale animation — visibility changes happen post-settle only.
pub fn set_visible(visible: bool) {
    with_controller(|c| unsafe {
        let _ = c.SetIsVisible(visible);
    });
}

/// Fade the page out via CSS opacity (composited inside Chromium): the
/// close path uses this so DWM topology stays untouched while the shell
/// shrinks. The real hide is deferred to the post-settle region timer.
pub fn fade_out() {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    exec("window.notchbud&&window.notchbud.fadeOut()");
}

/// Fade the page back in after the open spring settles.
pub fn fade_in() {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    exec("window.notchbud&&window.notchbud.fadeIn()");
}

fn exec(js: &str) {
    let js_owned = js.to_owned();
    with_webview(|wv| unsafe {
        let pw = CoTaskMemPWSTR::from(js_owned.as_str());
        let handler =
            ExecuteScriptCompletedHandler::create(Box::new(|_code, _result| Ok(())));
        let _ = wv.ExecuteScript(*pw.as_ref().as_pcwstr(), &handler);
    });
}

fn flush_pending() {
    let pending = PENDING.with_borrow(|p| p.clone());
    if let Some(js) = pending {
        exec(&js);
    }
}

/// Push Rust state into the page. Best-effort before load finishes —
/// the pending copy replays on NavigationCompleted and on next expand.
pub fn push_state(json_state: &str) {
    let js = format!("window.notchbud&&window.notchbud.render({json_state})");
    PENDING.with_borrow_mut(|p| *p = Some(js.clone()));
    if READY.load(Ordering::Acquire) {
        exec(&js);
    }
}

/// Forward a typed key from the low-level hook (the window never
/// activates, so the page can't receive it directly).
pub fn forward_key(code: usize) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    exec(&format!("window.notchbud&&window.notchbud.key({code})"));
}

/// Forward Ctrl+key shortcuts (copy / select-all / cut / paste).
pub fn forward_shortcut(name: &str) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    exec(&format!("window.notchbud&&window.notchbud.shortcut(\"{safe}\")"));
}
