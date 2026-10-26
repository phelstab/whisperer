//! Small Win32 helpers: the mouse position and the "start with Windows" Run-key entry.
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{PCWSTR, w};

/// The mouse pointer's screen position.
pub fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe { let _ = GetCursorPos(&mut p); }
    (p.x, p.y)
}

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("whisperer");

/// Whether the current user's Run key starts us with Windows.
pub fn autostart() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None).is_ok() }
}

/// Adds or removes the Run-key entry pointing at this executable.
pub fn set_autostart(on: bool) {
    unsafe {
        if !on {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return;
        }
        let exe = crate::update::exe();
        let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0, Some(value.as_ptr() as _), value.len() as u32 * 2);
    }
}
