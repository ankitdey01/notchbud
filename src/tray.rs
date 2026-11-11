// tray.rs — tray icon + context menu: Start with Windows (checkbox) /
// (separator) / Exit, opened at the cursor with RETURNCMD.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, POINT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreatePopupMenu, DestroyMenu, EnableMenuItem, GetCursorPos,
    LoadIconW, LoadImageW, PostMessageW, TrackPopupMenu, HICON, HMENU, IDI_APPLICATION,
    IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    MF_BYCOMMAND, MF_UNCHECKED, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, TPM_RETURNCMD, WM_NULL,
};

pub const TRAY_CALLBACK: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 9; // 0x8009

const MENU_START_WITH_WINDOWS: usize = 100;
const MENU_CONNECT: usize = 101;
const MENU_DISCONNECT: usize = 102;
const MENU_EXIT: usize = 104;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub unsafe fn add_tray(hwnd: HWND) -> windows::core::Result<()> {
    // Cat logo from the exe resources (assets/app.rc, group id 1).
    // Falls back to the default application icon so tray registration
    // proceeds without resource ID 1. LR_SHARED keeps the handle shared
    // so re-adding the tray never leaks an owned HICON; the fallback is
    // shared too, so remove_tray needs no DestroyIcon to stay consistent.
    let hmodule = GetModuleHandleW(None)?;
    let hicon: HICON = LoadImageW(
        Some(HINSTANCE(hmodule.0)),
        PCWSTR(1 as *const u16),
        IMAGE_ICON,
        32,
        32,
        LR_DEFAULTCOLOR | LR_SHARED,
    )
    .map(|h| HICON(h.0))
    .or_else(|_| LoadIconW(None, IDI_APPLICATION))?;
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK,
        hIcon: hicon,
        ..Default::default()
    };
    let tip = wide("Notchbud");
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        return Err(windows::core::Error::from_thread());
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
/// Connect stays enabled when signed in without plan usage, so the user can
/// authorize it separately; Disconnect is only meaningful while signed in.
pub unsafe fn popup_menu(hwnd: HWND, chat_connected: bool, plan_ready: bool) -> usize {
    let hmenu = match CreatePopupMenu() {
        Ok(h) => h,
        Err(_) => return 0,
    };
    append(hmenu, MF_STRING, "Connect ChatGPT", MENU_CONNECT);
    append(hmenu, MF_STRING, "Disconnect", MENU_DISCONNECT);
    if chat_connected && plan_ready {
        let _ = EnableMenuItem(hmenu, MENU_CONNECT as u32, MF_GRAYED);
    } else if !chat_connected {
        let _ = EnableMenuItem(hmenu, MENU_DISCONNECT as u32, MF_GRAYED);
    }
    append(hmenu, MF_SEPARATOR, "", 0);
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
        Some(0),
        hwnd,
        None,
    ); // RETURNCMD: loword = command (0 = dismissed)
    let _ = PostMessageW(Some(hwnd), WM_NULL, windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(0));
    // the handle is ours to free — without this every right-click leaks an HMENU
    let _ = DestroyMenu(hmenu);
    chosen.0.max(0) as usize
}

pub const ID_START: u32 = MENU_START_WITH_WINDOWS as u32;
pub const ID_CONNECT: u32 = MENU_CONNECT as u32;
pub const ID_DISCONNECT: u32 = MENU_DISCONNECT as u32;
pub const ID_EXIT: u32 = MENU_EXIT as u32;

#[cfg(test)]
mod tests {
    use super::*;

    // Crossed menu ids silently run Exit instead of Connect (or vice versa).
    // Pure consts — no window needed.
    #[test]
    fn menu_ids_distinct_and_wired() {
        let ids = [ID_START, ID_CONNECT, ID_DISCONNECT, ID_EXIT];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "menu ids must be unique");
            }
        }
        assert_eq!(ID_START, MENU_START_WITH_WINDOWS as u32);
        assert_eq!(ID_CONNECT, MENU_CONNECT as u32);
        assert_eq!(ID_DISCONNECT, MENU_DISCONNECT as u32);
        assert_eq!(ID_EXIT, MENU_EXIT as u32);
        assert_eq!(ID_EXIT, 104, "matches the documented Exit slot");
    }

    #[test]
    fn tray_callback_is_wm_app_plus_9() {
        assert_eq!(TRAY_CALLBACK, 0x8009);
    }
}
