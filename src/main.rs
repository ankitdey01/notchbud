// dyland — minimal notch: idle sliver at the top of the screen; hovering
// expands the island and shows "Hello Ankit"; leaving collapses it.

mod prefs;
mod render;
mod state;
mod tray;

use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{w, Result};
use windows::Win32::Foundation::{
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
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetSystemMetrics, LoadCursorW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetWindowLongPtrW, SetWindowPos,
    GetWindowLongPtrW, ShowWindow, TranslateMessage, GWLP_USERDATA, HTCLIENT,
    HTTRANSPARENT, MSG, SM_CXSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNOACTIVATE, WNDCLASSEXW,
    WM_DESTROY, WM_DPICHANGED, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST,
    WM_PAINT, WM_RBUTTONUP, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST,
};

// "TaskbarCreated" message id; re-adds the tray icon when explorer restarts
static TASKBAR_MSG: AtomicU32 = AtomicU32::new(0);

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
                render::draw_now(app);
                render::animate(true);
                after_state_change(app); // island region now, so it stays clickable while the clip animates open
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        WM_MOUSELEAVE => {
            app.mouse_inside = false;
            app.expanded = false;
            render::draw_now(app);
            render::animate(false);
            after_state_change(app); // back to sliver: the frame under (former) island lets clicks through at once
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
            tray::remove_tray(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => {
            if msg == tray::TRAY_CALLBACK {
                // left click toggles the island, right click opens the menu
                let lowword = (lparam.0 & 0xFFFF) as u16 as i32;
                if lowword == WM_RBUTTONUP as i32 {
                    handle_tray_menu(app);
                } else if lowword == WM_LBUTTONUP as i32 {
                    app.expanded = !app.expanded;
                    render::draw_now(app);
                    render::animate(app.expanded);
                    after_state_change(app);
                }
                LRESULT(0)
            } else if TASKBAR_MSG.load(Ordering::Relaxed) != 0
                && msg == TASKBAR_MSG.load(Ordering::Relaxed)
            {
                let _ = tray::add_tray(hwnd); // explorer restarted — re-register
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// behind the handlers
// ---------------------------------------------------------------------------

fn handle_tray_menu(app: &mut state::App) {
    unsafe {
        let chosen = tray::popup_menu(app.hwnd);
        match chosen as u32 {
            tray::ID_START => {
                let on = !prefs::is_start_with_windows();
                let _ = prefs::set_start_with_windows(on);
            }
            tray::ID_EXIT => PostQuitMessage(0),
            _ => {}
        }
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
    let step = |s: &str| eprintln!("[step] {s}");
    unsafe {
        // --- DPI awareness (PER_MONITOR_AWARE_V2) ---
        step("dpi");
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;

        // --- single instance: Local\Dyland.SingleInstance (env dev override) ---
        step("mutex");
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
        step("scale");

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
        step("class");

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
        let app = Box::new(state::App::new(hwnd, scale, width, height));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
        step("app");

        // --- tray icon + explorer-restart hook ---
        tray::add_tray(hwnd)?;
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_MSG.store(taskbar_created, Ordering::Relaxed);
        step("tray");

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        // first frame
        if let Some(app) = app_from(hwnd) {
            render::draw_now(app);
            after_state_change(app);
        }
        step("pump");

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
