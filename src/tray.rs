//! Notification-area icon: a small waveform ("whisper") glyph, blue when idle, red while recording,
//! amber while preparing or transcribing. A left click starts/stops recording; the menu holds the
//! rest. Balloon notifications report results; clicking one opens its file.
use crate::{Command, language, win};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

const WM_TRAY: u32 = WM_APP + 1;
/// Posted by `wake` from any thread: there are events for `crate::handle_events`.
const WM_EVENTS: u32 = WM_APP + 2;
const ID_VERSION: usize = 1;
const ID_STATUS: usize = 2;
const ID_TOGGLE: usize = 3;
const ID_OUTPUT: usize = 4;
const ID_SETTINGS: usize = 5;
const ID_SELFTEST: usize = 6;
const ID_UPDATE: usize = 7;
const ID_AUTOSTART: usize = 8;
const ID_QUIT: usize = 9;
const ID_LANG_AUTO: usize = 100;
/// `ID_LANG_AUTO + 1 + i` = `language::MENU[i]`; the one after those is a code set in the file.
const ID_LANG_OTHER: usize = ID_LANG_AUTO + 1 + language::MENU.len();

/// What the icon shows.
#[derive(Clone, Copy, PartialEq)]
pub enum Look {
    Idle,
    Recording,
    Busy,
}

thread_local! {
    static WINDOW: Cell<HWND> = Cell::new(HWND::default());
    static LOOK: Cell<Look> = const { Cell::new(Look::Idle) };
    static TIP: RefCell<String> = const { RefCell::new(String::new()) };
    /// File opened when the current balloon is clicked.
    static BALLOON_FILE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}
static HWND_FOR_WAKE: AtomicIsize = AtomicIsize::new(0);

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
        HWND_FOR_WAKE.store(hwnd.0 as isize, Ordering::Release);
    }
    notify(NIM_ADD, None);
}

/// Wakes the main thread to handle queued events. Callable from any thread.
pub fn wake() {
    let h = HWND_FOR_WAKE.load(Ordering::Acquire);
    if h != 0 {
        unsafe { let _ = PostMessageW(Some(HWND(h as _)), WM_EVENTS, WPARAM(0), LPARAM(0)); }
    }
}

/// Removes the icon from the notification area (on exit, so no stale icon remains).
pub fn remove() {
    notify(NIM_DELETE, None);
}

/// Sets the icon's color and its tooltip line under the app name.
pub fn show(look: Look, tip: &str) {
    if LOOK.get() == look && TIP.with_borrow(|t| t == tip) {
        return;
    }
    LOOK.set(look);
    TIP.set(tip.to_string());
    notify(NIM_MODIFY, None);
}

/// Shows a balloon notification; clicking it opens `file`, if given.
pub fn balloon(title: &str, text: &str, file: Option<PathBuf>) {
    BALLOON_FILE.set(file);
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
        hIcon: icon(LOOK.get()),
        ..Default::default()
    };
    let tip = TIP.with_borrow(|t| if t.is_empty() { format!("Whisperer v{}", crate::VERSION) } else { format!("Whisperer: {t}") });
    copy(&mut data.szTip, &tip);
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

/// Draws the icon at the system's small-icon size: five waveform bars on a transparent background,
/// colored by `look`. The caller owns the returned icon.
fn icon(look: Look) -> HICON {
    let rgb: u32 = match look {
        Look::Idle => 0x4c9be8,
        Look::Recording => 0xe8484c,
        Look::Busy => 0xe8a33c,
    };
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
        let bar = CreateSolidBrush(COLORREF((rgb & 0xff) << 16 | (rgb & 0xff00) | (rgb >> 16 & 0xff))); // GDI wants 0x00BBGGRR
        let u = (s / 16).max(1); // bar width 2u, gap u: 5 bars span 14u
        let left = (s - 14 * u) / 2;
        for (i, f) in [0.3, 0.6, 0.95, 0.6, 0.3].into_iter().enumerate() {
            let h = ((s as f32 * f) as i32).max(2);
            let x = left + 3 * u * i as i32;
            let r = RECT { left: x, top: (s - h) / 2, right: x + 2 * u, bottom: (s - h) / 2 + h };
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

/// Shows the menu at the mouse pointer, waits for a choice and hands it to `crate::command`.
fn menu(hwnd: HWND) {
    let (recording, status, lang) = crate::menu_info();
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        let version = HSTRING::from(format!("Whisperer v{}", crate::VERSION));
        let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, ID_VERSION, &version);
        let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, ID_STATUS, &HSTRING::from(status));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let toggle = if recording { w!("Stop recording") } else { w!("Start recording") };
        let _ = AppendMenuW(m, MF_STRING | MF_DEFAULT, ID_TOGGLE, toggle);
        let _ = AppendMenuW(m, MF_STRING, ID_OUTPUT, w!("Open output folder"));
        let _ = AppendMenuW(m, MF_STRING, ID_SETTINGS, w!("Settings"));
        if let Ok(sub) = CreatePopupMenu() {
            let item = |id, code: &str| {
                let _ = AppendMenuW(sub, MF_STRING | check(code == lang), id, &HSTRING::from(language::describe(code)));
            };
            item(ID_LANG_AUTO, "auto");
            let _ = AppendMenuW(sub, MF_SEPARATOR, 0, None);
            language::MENU.iter().enumerate().for_each(|(i, code)| item(ID_LANG_AUTO + 1 + i, code));
            if lang != "auto" && !language::MENU.contains(&lang.as_str()) {
                item(ID_LANG_OTHER, &lang); // another code from the settings file
            }
            let _ = AppendMenuW(sub, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(sub, MF_STRING, ID_SETTINGS, w!("Other language: edit settings…"));
            let title = HSTRING::from(format!("Language: {}", language::describe(&lang)));
            let _ = AppendMenuW(m, MF_POPUP, sub.0 as usize, &title);
        }
        let _ = AppendMenuW(m, MF_STRING, ID_SELFTEST, w!("Run GPU self-test"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, ID_UPDATE, w!("Check for updates"));
        let _ = AppendMenuW(m, MF_STRING | check(win::autostart()), ID_AUTOSTART, w!("Start with Windows"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, ID_QUIT, w!("Quit"));
        let (x, y) = win::cursor();
        let _ = SetForegroundWindow(hwnd); // otherwise the menu doesn't close when clicking elsewhere
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, x, y, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(m);
        let command = match cmd {
            ID_TOGGLE => Command::Toggle,
            ID_OUTPUT => Command::OpenOutput,
            ID_SETTINGS => Command::Settings,
            ID_SELFTEST => Command::SelfTest,
            ID_UPDATE => Command::CheckUpdates,
            ID_AUTOSTART => return win::set_autostart(!win::autostart()),
            ID_QUIT => Command::Quit,
            ID_LANG_AUTO => Command::Language("auto".into()),
            id if id > ID_LANG_AUTO && id < ID_LANG_OTHER => Command::Language(language::MENU[id - ID_LANG_AUTO - 1].into()),
            _ => return,
        };
        crate::command(command);
    }
}

/// Window procedure of the hidden tray window: left click toggles recording, right click opens the
/// menu, a balloon click opens its file; also handles queued events and re-adds the icon after
/// Explorer restarts.
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => match lp.0 as u32 {
            WM_LBUTTONUP => crate::command(Command::Toggle),
            WM_RBUTTONUP => menu(hwnd),
            NIN_BALLOONUSERCLICK => {
                if let Some(f) = BALLOON_FILE.take() {
                    win::open(&f);
                }
            }
            _ => {}
        },
        WM_EVENTS => crate::handle_events(),
        // Explorer restarted: the icon is gone and has to be added again.
        _ if msg == unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) } => notify(NIM_ADD, None),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}
