// prefs.rs — the HKCU Run key for "start with windows".

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_ROUTINE_FLAGS, REG_SAM_FLAGS, REG_SZ,
    RRF_RT_REG_SZ,
};

// --- "start with windows" (HKCU\...\Run value "Dyland") ---
const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Dyland");

fn win32(err: WIN32_ERROR) -> windows::core::Result<()> {
    if err.is_ok() { Ok(()) } else { Err(windows::core::Error::from(err)) }
}

pub fn set_start_with_windows(on: bool) -> windows::core::Result<()> {
    unsafe {
        let mut hk = HKEY::default();
        win32(RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, 0, REG_SAM_FLAGS(KEY_SET_VALUE.0 | KEY_READ.0), &mut hk))?;
        if on {
            // value = quoted path to our exe
            let mut buf = [0u16; 512];
            let n = windows::Win32::System::LibraryLoader::GetModuleFileNameW(None, &mut buf);
            let cmd = format!("\"{}\"", String::from_utf16_lossy(&buf[..n as usize]));
            let mut wide: Vec<u16> = cmd.encode_utf16().collect();
            wide.push(0);
            win32(RegSetValueExW(
                hk,
                RUN_VALUE,
                0,
                REG_SZ,
                Some(std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2)),
            ))?;
        } else {
            win32(RegDeleteValueW(hk, RUN_VALUE))?;
        }
        let _ = RegCloseKey(hk);
        Ok(())
    }
}

pub fn is_start_with_windows() -> bool {
    unsafe {
        let mut data = [0u16; 256];
        let mut size = (data.len() * 2) as u32;
        win32(RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            RUN_VALUE,
            REG_ROUTINE_FLAGS(RRF_RT_REG_SZ.0),
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        ))
        .is_ok()
    }
}
