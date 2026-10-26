//! Notification-area icon: a small waveform ("whisper") glyph whose menu shows the version, checks
//! for updates, toggles autostart and quits. Balloon notifications report update results.
use crate::{update, win};
use std::cell::Cell;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

const WM_TRAY: u32 = WM_APP + 1;
const ID_VERSION: usize = 1;
const ID_UPDATE: usize = 2;
const ID_AUTOSTART: usize = 3;
const ID_QUIT: usize = 4;

thread_local! {
    static WINDOW: Cell<HWND> = Cell::new(HWND::default());
}

/// Creates the hidden window that receives the icon's mouse messages and adds the icon.
pub fn create() {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default().into();
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: w!("whisperer-tray"),
            ..Default::default()
        });
        // A hidden top-level window (not message-only): it must receive the "TaskbarCreated" broadcast.
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), w!("whisperer-tray"), w!("whisperer"), WINDOW_STYLE(0),
            0, 0, 0, 0, None, None, Some(instance), None)
        .expect("create tray window");
        WINDOW.set(hwnd);
    }
    notify(NIM_ADD, None);
}

/// Removes the icon from the notification area (on exit, so no stale icon remains).
pub fn remove() {
    notify(NIM_DELETE, None);
}

/// Shows a balloon notification with `title` and `text` next to the icon.
pub fn balloon(title: &str, text: &str) {
    notify(NIM_MODIFY, Some((title, text)));
}

/// Adds, updates or deletes (`op`) the notification-area icon, optionally with a balloon (title, text).
fn notify(op: NOTIFY_ICON_MESSAGE, info: Option<(&str, &str)>) {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: WINDOW.get(),
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon(),
        ..Default::default()
    };
    copy(&mut data.szTip, &format!("Whisperer v{}", crate::VERSION));
    if let Some((title, text)) = info {
        data.uFlags |= NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        copy(&mut data.szInfoTitle, title);
        copy(&mut data.szInfo, text);
    }
    unsafe {
        let _ = Shell_NotifyIconW(op, &data);
        let _ = DestroyIcon(data.hIcon); // the shell keeps its own copy
    }
}

/// Copies `s` into the fixed-size UTF-16 buffer `dst`, truncated so the terminating zero still fits.
fn copy(dst: &mut [u16], s: &str) {
    let room = dst.len() - 1;
    dst.iter_mut().zip(s.encode_utf16().take(room)).for_each(|(d, c)| *d = c);
}

/// Draws the icon at the system's small-icon size: five blue waveform bars on a transparent background.
/// The caller owns the returned icon.
fn icon() -> HICON {
    unsafe {
        let s = GetSystemMetrics(SM_CXSMICON);
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let color = CreateCompatibleBitmap(screen, s, s);
        // Monochrome AND mask, rows padded to 16 bits: 1 = transparent, cleared below wherever a bar is drawn.
        let stride = (s as usize).div_ceil(16) * 2;
        let mut mask_bits = vec![0xffu8; stride * s as usize];
        let old = SelectObject(dc, color.into());
        let black = CreateSolidBrush(COLORREF(0));
        FillRect(dc, &RECT { left: 0, top: 0, right: s, bottom: s }, black);
        let _ = DeleteObject(black.into());
        let bar = CreateSolidBrush(COLORREF(0xe89b4c)); // 0x4c9be8 as GDI's 0x00BBGGRR
        let u = (s / 16).max(1); // bar width 2u, gap u: 5 bars span 14u
        let left = (s - 14 * u) / 2;
        for (i, f) in [0.3, 0.6, 0.95, 0.6, 0.3].into_iter().enumerate() {
            let h = ((s as f32 * f) as i32).max(2);
            let r = RECT { left: left + 3 * u * i as i32, top: (s - h) / 2, right: left + 3 * u * i as i32 + 2 * u, bottom: (s - h) / 2 + h };
            FillRect(dc, &r, bar);
            for y in r.top..r.bottom {
                for x in r.left..r.right {
                    mask_bits[y as usize * stride + x as usize / 8] &= !(0x80 >> (x % 8));
                }
            }
        }
        let _ = DeleteObject(bar.into());
        SelectObject(dc, old);
        let mask = CreateBitmap(s, s, 1, 1, Some(mask_bits.as_ptr() as _));
        let icon = CreateIconIndirect(&ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() });
        let _ = (DeleteObject(color.into()), DeleteObject(mask.into()), DeleteDC(dc));
        ReleaseDC(None, screen);
        icon.unwrap_or_default()
    }
}

/// Shows the menu at the mouse pointer, waits for a choice and carries it out.
fn menu(hwnd: HWND) {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        let version = HSTRING::from(format!("Whisperer v{}", crate::VERSION));
        let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, ID_VERSION, &version);
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, ID_UPDATE, w!("Check for updates"));
        let _ = AppendMenuW(m, MF_STRING | check(win::autostart()), ID_AUTOSTART, w!("Start with Windows"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, ID_QUIT, w!("Quit"));
        let (x, y) = win::cursor();
        let _ = SetForegroundWindow(hwnd); // otherwise the menu doesn't close when clicking elsewhere
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, x, y, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(m);
        match cmd {
            ID_UPDATE => update::check_now(),
            ID_AUTOSTART => win::set_autostart(!win::autostart()),
            ID_QUIT => PostQuitMessage(0),
            _ => {}
        }
    }
}

/// Window procedure of the hidden tray window: opens the menu on a click on the icon, and re-adds
/// the icon after Explorer restarts.
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY if matches!(lp.0 as u32, WM_LBUTTONUP | WM_RBUTTONUP) => menu(hwnd),
        // Explorer restarted: the icon is gone and has to be added again.
        _ if msg == unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) } => notify(NIM_ADD, None),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}
