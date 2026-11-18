// island_web.rs — WebView2 content host inside the Win32 island.
//
// The Win32 window + Direct2D shell + DComp springs stay exactly as they
// were. All *content* (greeting, status, history, inputs) now renders in
// HTML/CSS inside a WebView2 child window positioned over the island.
//
// Threading: everything here runs on the UI thread (the message pump).
// The low-level keyboard hook never calls in directly — it posts WM_KEY
// like before, and the wndproc forwards into `forward_key`.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use windows::core::{Interface, PCWSTR};
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
// Cat logo for the island brand mark (96px PNG data URI in ui/logo.b64).
const LOGO_DATAURI: &str = include_str!("../ui/logo.b64");

fn composed_html() -> String {
    let css = format!("<style>{ISLAND_CSS}</style>");
    let js = format!("<script>{ISLAND_JS}</script>");
    INDEX_HTML
        .replacen("<!--NOTCHBUD_CSS-->", &css, 1)
        .replacen("<!--NOTCHBUD_JS-->", &js, 1)
        .replace("__NOTCHBUD_LOGO__", LOGO_DATAURI.trim())
}

/// Island content bounds in physical px: edge-to-edge with the shell body
/// (ISLAND_X..ISLAND_X+ISLAND_W, 0..ISLAND_H) so the page draws to the
/// island edges. The WebView is rectangular while the shell has rounded
/// bottom corners (ISLAND_BOT_R) — the page clips itself with a matching
/// CSS border-radius + overflow:hidden (see ui/styles.css #island), so
/// content never overfloats outside the silhouette. ceil/floor keeps the
/// rect just inside the shell AA edge.
pub fn island_rect(scale: f32) -> RECT {
    let left = (ISLAND_X * scale).ceil() as i32;
    let right = ((ISLAND_X + ISLAND_W) * scale).floor() as i32;
    RECT {
        left,
        top: 0,
        right,
        bottom: (ISLAND_H * scale).floor() as i32,
    }
}

/// Explicit per-user profile dir for WebView2. `None` means "no usable
/// base" — the caller falls back to the default environment.
///
/// The dir must exist AND pass the write probe: a dir that exists but is
/// held by another live instance (or carries foreign ACLs) makes
/// environment creation fail, which used to abort init silently and leave
/// a black island. Rejecting it here routes init to the fallback instead.
pub fn pick_user_data_dir(base: Option<&OsStr>) -> Option<PathBuf> {
    pick_user_data_dir_with(base, &default_write_probe)
}

pub fn pick_user_data_dir_with(base: Option<&OsStr>, probe: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let base = base?;
    // Explicit user data folder under LOCALAPPDATA so WebView2 never
    // depends on the executable-relative default. The installer default
    // (installer/notchbud.iss DefaultDirName) is
    // {localappdata}\Programs\Notchbud — writable today — but the folder
    // picker lets the user choose any directory, so an exe-relative folder
    // could land somewhere non-writable. LOCALAPPDATA is always writable
    // for this per-user (PrivilegesRequired=lowest) install.
    let mut dir = PathBuf::from(base);
    dir.push("notchbud");
    dir.push("EBWebView");
    // Ensure the path exists; on failure the caller falls back to the
    // default environment creation.
    std::fs::create_dir_all(&dir).ok()?;
    if !dir.is_absolute() {
        return None;
    }
    if !probe(&dir) {
        return None;
    }
    Some(dir)
}

/// Writability probe: the dir must accept a file create+remove. Cheap,
/// synchronous, and the exact capability WebView2 needs from the profile.
pub fn default_write_probe(dir: &Path) -> bool {
    let probe = dir.join(".notchbud-write-probe");
    match std::fs::OpenOptions::new().write(true).create(true).open(&probe) {
        Ok(_) => std::fs::remove_file(&probe).is_ok(),
        Err(_) => false,
    }
}

/// HRESULT_FROM_WIN32(ERROR_BUSY): `CreateCoreWebView2Controller` reports
/// "the requested resource is in use" when the profile is held by another
/// live WebView2 (stale notchbud/msedgewebview2 after a kill). The plain
/// write probe still passes in that state, so only this classification
/// tells init to retry on a fresh profile instead of leaving a black island.
pub const HRESULT_BUSY: windows::core::HRESULT =
    windows::core::HRESULT(0x800700AAu32 as i32);

/// True when a controller/env error is the retryable profile-locked case.
pub fn is_busy_hresult(hr: windows::core::HRESULT) -> bool {
    hr == HRESULT_BUSY
}

/// Fresh per-process profile dir: a brand-new dir cannot be locked by a
/// stale process, so a BUSY controller retries here. Same parent (still
/// per-user LOCALAPPDATA), pid-suffixed name so a restart reuses it.
pub fn busy_fallback_dir(primary: &Path) -> PathBuf {
    let parent = primary.parent();
    let base = primary
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("EBWebView");
    let name = format!("{base}.{}", std::process::id());
    match parent {
        Some(p) => p.join(name),
        None => PathBuf::from(name),
    }
}

/// Ordered environment attempts: the explicit dir first, a fresh
/// per-process profile second (the BUSY retry that avoids black when a
/// stale process holds the primary profile), the default environment last.
#[derive(Debug, PartialEq, Clone)]
pub enum EnvDir {
    Explicit(PathBuf),
    Default,
}

pub fn env_attempt_order(primary: Option<PathBuf>) -> Vec<EnvDir> {
    match primary {
        Some(dir) => {
            let fresh = busy_fallback_dir(&dir);
            // Degenerate input (dir == its own fallback): keep two attempts.
            if fresh == dir {
                vec![EnvDir::Explicit(dir), EnvDir::Default]
            } else {
                vec![
                    EnvDir::Explicit(dir),
                    EnvDir::Explicit(fresh),
                    EnvDir::Default,
                ]
            }
        }
        None => vec![EnvDir::Default],
    }
}

/// Diagnostics for WebView2 startup. Release builds have no console
/// (windows_subsystem), so every environment outcome is also appended to
/// %TEMP%\notchbud-webview.log — a black island finally leaves a trace.
fn log_webview_event(line: &str) {
    eprintln!("[notchbud] {line}");
    let mut path = std::env::temp_dir();
    path.push("notchbud-webview.log");
    // Cap the file: release builds append every launch with no rotation.
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 512 * 1024 {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "[notchbud] {line}");
    }
}

/// One environment attempt: explicit profile dir or the default
/// environment. `None` = unusable/failed — the caller moves to the next
/// attempt instead of leaving a black island. Every outcome is logged to
/// the file log (release builds have no console).
fn create_environment(attempt: &EnvDir) -> Option<ICoreWebView2Environment> {
    let user_data_wide: Option<Vec<u16>> = match attempt {
        EnvDir::Explicit(dir) => {
            // The fresh per-process fallback may not exist yet; the primary
            // was already created by the picker. WebView2 also creates the
            // dir itself, so a failure here just falls through.
            let _ = std::fs::create_dir_all(dir);
            Some(
                dir.to_string_lossy()
                    .into_owned()
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect(),
            )
        }
        EnvDir::Default => None,
    };
    let (etx, erx) = mpsc::channel();
    let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result: windows::core::Result<()>, env: Option<ICoreWebView2Environment>| {
            let _ = etx.send((result, env));
            Ok(())
        },
    ));
    let outcome = unsafe {
        // Keep the wide string alive for the whole creation call.
        let create_result = match user_data_wide.as_ref() {
            Some(wide) => CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                PCWSTR(wide.as_ptr()),
                None::<&ICoreWebView2EnvironmentOptions>,
                &env_handler,
            ),
            None => CreateCoreWebView2Environment(&env_handler),
        };
        match create_result {
            Ok(()) => match webview2_com::wait_with_pump(erx) {
                Ok((Ok(()), Some(env))) => Some(env),
                Ok((Err(e), _)) => {
                    log_webview_event(&format!("webview env failed ({attempt:?}): {e:#}"));
                    None
                }
                _ => {
                    log_webview_event(&format!("webview env failed ({attempt:?}): no environment"));
                    None
                }
            },
            Err(e) => {
                log_webview_event(&format!("CreateCoreWebView2Environment ({attempt:?}): {e:#}"));
                None
            }
        }
    };
    if outcome.is_some() {
        log_webview_event(&format!("webview env ready ({attempt:?})"));
    }
    outcome
}

/// One controller attempt on an environment. `None` = failed — the caller
/// retries the next profile instead of leaving a black island. A BUSY
/// result (profile held by another live WebView2) is logged distinctly so
/// the file log tells stale-process apart from real failures.
fn create_controller(
    env: &ICoreWebView2Environment,
    parent: HWND,
    attempt: &EnvDir,
) -> Option<ICoreWebView2Controller> {
    let (ctx, crx) = mpsc::channel();
    let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |result: windows::core::Result<()>, ctrl: Option<ICoreWebView2Controller>| {
            let _ = ctx.send((result, ctrl));
            Ok(())
        },
    ));
    unsafe {
        match env.CreateCoreWebView2Controller(parent, &ctrl_handler) {
            Ok(()) => match webview2_com::wait_with_pump(crx) {
                Ok((Ok(()), Some(c))) => Some(c),
                Ok((Err(e), _)) => {
                    if is_busy_hresult(e.code()) {
                        log_webview_event(&format!(
                            "webview controller busy ({attempt:?}): profile held by another live WebView2; retrying next profile"
                        ));
                    } else {
                        log_webview_event(&format!(
                            "webview controller failed ({attempt:?}): {e:#}"
                        ));
                    }
                    None
                }
                _ => {
                    log_webview_event(&format!(
                        "webview controller failed ({attempt:?}): none"
                    ));
                    None
                }
            },
            Err(e) => {
                log_webview_event(&format!(
                    "CreateCoreWebView2Controller ({attempt:?}): {e:#}"
                ));
                None
            }
        }
    }
}

/// Create the WebView2 child. Called once, on the UI thread, after the
/// Win32 window exists. Blocks briefly (pumping messages) until the
/// controller is ready, then navigates to the embedded island page.
pub fn init(parent: HWND, scale: f32) {
    // Idempotent: a second call (e.g. DPI re-init racing startup) must not
    // create a second controller on the same HWND — that fails BUSY too.
    if READY.load(Ordering::Acquire) {
        return;
    }
    let already = CONTROLLER.with_borrow(|c| c.is_some());
    if already {
        return;
    }
    unsafe { let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); }

    PARENT_HWND.store(parent.0 as isize, Ordering::Relaxed);

    // --- environment + controller, with profile retry ---
    // A failed explicit environment used to abort init silently and leave
    // a black island; now every attempt is logged (release builds have no
    // console — see %TEMP%\notchbud-webview.log) and the per-process fresh
    // profile is always tried before giving up, so a stale lock on the
    // primary profile degrades to a fresh profile instead of black.
    let primary = pick_user_data_dir(std::env::var_os("LOCALAPPDATA").as_deref());
    let mut pair: Option<(ICoreWebView2Environment, ICoreWebView2Controller)> = None;
    for attempt in env_attempt_order(primary) {
        let Some(env) = create_environment(&attempt) else {
            continue;
        };
        if let Some(controller) = create_controller(&env, parent, &attempt) {
            log_webview_event(&format!("webview controller ready ({attempt:?})"));
            pair = Some((env, controller));
            break;
        }
    }
    let (env, controller) = match pair {
        Some(p) => p,
        None => {
            log_webview_event("webview unavailable in all attempts; island content disabled");
            return;
        }
    };
    // Keep the environment alive for the whole init (as before: the
    // controller holds its own refs, this is just lifetime parity).
    let _env = env;

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
