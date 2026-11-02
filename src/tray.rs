// tray.rs — tray icon + context menu: Start with Windows (checkbox) /
// (separator) / Exit, opened at the cursor with RETURNCMD.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, PostMessageW,
    TrackPopupMenu, HMENU, IDI_APPLICATION, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_BYCOMMAND,
    MF_UNCHECKED, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, TPM_RETURNCMD, WM_NULL,
};

pub const TRAY_CALLBACK: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 9; // 0x8009

const MENU_START_WITH_WINDOWS: usize = 100;
const MENU_EXIT: usize = 104;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub unsafe fn add_tray(hwnd: HWND) -> windows::core::Result<()> {
    let hicon = LoadIconW(None, IDI_APPLICATION)?;
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK,
        hIcon: hicon,
        ..Default::default()
    };
    let tip = wide("Dyland");
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

pub unsafe fn remove_tray(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

fn append(hmenu: HMENU, flags: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS, text: &str, id: usize) {
    unsafe {
        let _ = AppendMenuW(hmenu, flags, id, PCWSTR(wide(text).as_ptr()));
    }
}

/// Builds + shows the menu; returns the chosen command id (0 if dismissed).
pub unsafe fn popup_menu(hwnd: HWND) -> usize {
    let hmenu = match CreatePopupMenu() {
        Ok(h) => h,
        Err(_) => return 0,
    };
    append(hmenu, MF_STRING, "Start with Windows", MENU_START_WITH_WINDOWS);
    let mode = if crate::prefs::is_start_with_windows() { MF_CHECKED } else { MF_UNCHECKED };
    CheckMenuItem(hmenu, MENU_START_WITH_WINDOWS as u32, (MF_BYCOMMAND | mode).0);
    append(hmenu, MF_SEPARATOR, "", 0);
    append(hmenu, MF_STRING, "Exit", MENU_EXIT);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    // SetForegroundWindow before TrackPopupMenu so the menu keeps focus.
    let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(
        hmenu,
        TPM_RIGHTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    ); // RETURNCMD: loword = command (0 = dismissed)
    let _ = PostMessageW(hwnd, WM_NULL, windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(0));
    // the handle is ours to free — without this every right-click leaks an HMENU
    let _ = DestroyMenu(hmenu);
    chosen.0.max(0) as usize
}

pub const ID_START: u32 = MENU_START_WITH_WINDOWS as u32;
pub const ID_EXIT: u32 = MENU_EXIT as u32;
