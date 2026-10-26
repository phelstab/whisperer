#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod tray;
mod update;
mod win;

use update::Outcome;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

/// This build's version: the topmost "== V<x.y.z>" heading of README.adoc (see build.rs).
pub const VERSION: &str = env!("WHISPERER_VERSION");

/// Passed to the new exe after an update, so it waits for the old one and says what happened.
const UPDATED_FLAG: &str = "--updated";

/// Takes the per-user single-instance lock, or `None` if another instance holds it. After an update
/// the old process may still be exiting, so `wait` retries for a few seconds.
fn single_instance(wait: bool) -> Option<HANDLE> {
    for _ in 0..if wait { 50 } else { 1 } {
        let handle = unsafe { CreateMutexW(None, true, w!("Local\\whisperer")) }.ok()?;
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            return Some(handle);
        }
        unsafe { let _ = CloseHandle(handle); }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

/// Sets up the tray icon and the update checker, then runs the message loop until quit, or until
/// an update was installed, in which case it restarts into the new exe.
fn main() {
    let updated = std::env::args().any(|a| a == UPDATED_FLAG);
    let Some(lock) = single_instance(updated) else { return };
    unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }

    update::exe(); // remember our path before an update renames the file
    tray::create();
    if updated {
        tray::balloon("Whisperer updated", &format!("Now running v{VERSION}."));
    }
    update::start(unsafe { GetCurrentThreadId() });

    let mut restart = false;
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        if msg.hwnd.is_invalid() && msg.message == update::WM_UPDATE {
            for outcome in update::take_outcomes() {
                match outcome {
                    Outcome::UpToDate => tray::balloon("Whisperer is up to date", &format!("v{VERSION} is the latest release.")),
                    Outcome::Failed(e) => tray::balloon("Update check failed", &e),
                    Outcome::Installed => restart = true,
                }
            }
            if restart {
                break;
            }
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    tray::remove();
    unsafe { let _ = CloseHandle(lock); }
    if restart {
        let _ = std::process::Command::new(update::exe()).arg(UPDATED_FLAG).spawn();
    }
}
