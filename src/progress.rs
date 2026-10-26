//! The setup window: shows what the engine is preparing (downloads with a progress bar, size,
//! speed and time left; unpacking; the GPU self-test) and then which GPU and CUDA versions are in
//! use. It stays on top at the bottom right above the tray and can be hidden; preparing goes on
//! anyway.
use std::cell::{Cell, RefCell};
use std::time::Instant;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

const ID_BUTTON: i32 = 2; // IDCANCEL, so Esc works too
const TIMER_CLOSE: usize = 1;

/// The window's controls.
#[derive(Clone, Copy)]
struct Controls {
    window: HWND,
    title: HWND,
    bar: HWND,
    detail: HWND,
    button: HWND,
}

thread_local! {
    static CONTROLS: Cell<Option<Controls>> = const { Cell::new(None) };
    static MARQUEE: Cell<bool> = const { Cell::new(false) };
    /// The download being shown and when / at how many bytes it was first seen, for the speed.
    static SPEED: RefCell<Option<(String, Instant, u64)>> = const { RefCell::new(None) };
}

/// Whether the window is open.
pub fn is_open() -> bool {
    CONTROLS.get().is_some()
}

/// Opens the window (if it isn't) with `title` and a moving bar.
pub fn open(title: &str) {
    if !is_open() {
        create();
    }
    activity(title, "");
}

/// Shows a download: `done` of `total` bytes of `what`, with speed and time left.
pub fn download(what: &str, done: u64, total: u64) {
    let Some(c) = CONTROLS.get() else { return };
    let (started, first) = SPEED.with_borrow_mut(|s| match s {
        Some((w, t, b)) if w == what && *b <= done => (*t, *b),
        _ => {
            *s = Some((what.to_string(), Instant::now(), done));
            (Instant::now(), done)
        }
    });
    let secs = started.elapsed().as_secs_f64();
    let rate = if secs > 1.0 { (done - first) as f64 / secs } else { 0.0 };
    let mut detail = format!("{} of {}", gb(done), gb(total));
    if rate > 0.0 {
        let left = (total.saturating_sub(done)) as f64 / rate;
        detail += &format!("  ·  {:.1} MB/s  ·  {} left", rate / 1e6, duration(left));
    }
    set_marquee(c.bar, false);
    unsafe {
        SendMessageW(c.bar, PBM_SETPOS, Some(WPARAM((done * 1000 / total.max(1)) as usize)), None);
    }
    set_text(c.title, &format!("Downloading {what}…"));
    set_text(c.detail, &detail);
}

/// Shows a step without a known length (unpacking, testing) with a moving bar.
pub fn activity(title: &str, detail: &str) {
    let Some(c) = CONTROLS.get() else { return };
    set_marquee(c.bar, true);
    set_text(c.title, title);
    set_text(c.detail, detail);
}

/// Shows the final result (full bar, button "Close") and closes the window after a while.
pub fn finish(title: &str, detail: &str, ok: bool) {
    let Some(c) = CONTROLS.get() else { return };
    set_marquee(c.bar, false);
    unsafe {
        SendMessageW(c.bar, PBM_SETPOS, Some(WPARAM(if ok { 1000 } else { 0 })), None);
        SetTimer(Some(c.window), TIMER_CLOSE, if ok { 15_000 } else { 60_000 }, None);
    }
    set_text(c.title, title);
    set_text(c.detail, detail);
    set_text(c.button, "Close");
}

fn create() {
    unsafe {
        let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_PROGRESS_CLASS };
        let _ = InitCommonControlsEx(&icc);
        let instance = GetModuleHandleW(None).unwrap_or_default().into();
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszClassName: w!("whisperer-progress"),
            ..Default::default()
        });
        let scale = |v: i32| v * GetDpiForSystem() as i32 / 96;
        let (w, h) = (scale(480), scale(170));
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let Ok(window) = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, w!("whisperer-progress"), w!("Whisperer setup"),
            WS_CAPTION | WS_SYSMENU, work.right - w - scale(16), work.bottom - h - scale(16), w, h, None, None, Some(instance), None)
        else { return };

        let mut metrics = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ = SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, metrics.cbSize, Some(&mut metrics as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let font = CreateFontIndirectW(&metrics.lfMessageFont);
        let child = |class, style: WINDOW_STYLE, x, y, cw, ch, id: i32| {
            let c = CreateWindowExW(WINDOW_EX_STYLE(0), class, &HSTRING::new(), WS_CHILD | WS_VISIBLE | style,
                scale(x), scale(y), scale(cw), scale(ch), Some(window), Some(HMENU(id as _)), Some(instance), None).unwrap_or_default();
            SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            c
        };
        let controls = Controls {
            window,
            title: child(w!("STATIC"), WINDOW_STYLE(0x4000), // SS_ENDELLIPSIS
            14, 12, 440, 20, 0),
            bar: child(PROGRESS_CLASSW, WINDOW_STYLE(0), 14, 38, 440, 18, 0),
            detail: child(w!("STATIC"), WINDOW_STYLE(0), // wraps to a second line
            14, 62, 440, 36, 0),
            button: child(w!("BUTTON"), WS_TABSTOP, 366, 100, 88, 28, ID_BUTTON),
        };
        SendMessageW(controls.bar, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(1000)));
        SetWindowTextW(controls.button, w!("Hide")).ok();
        CONTROLS.set(Some(controls));
        MARQUEE.set(false);
        let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
    }
}

fn set_text(h: HWND, s: &str) {
    unsafe { let _ = SetWindowTextW(h, &HSTRING::from(s)); }
}

/// Switches the bar between a moving (unknown length) and a normal bar.
fn set_marquee(bar: HWND, on: bool) {
    if MARQUEE.get() == on {
        return;
    }
    MARQUEE.set(on);
    unsafe {
        let style = GetWindowLongW(bar, GWL_STYLE);
        let style = if on { style | PBS_MARQUEE as i32 } else { style & !(PBS_MARQUEE as i32) };
        SetWindowLongW(bar, GWL_STYLE, style);
        SendMessageW(bar, PBM_SETMARQUEE, Some(WPARAM(on as usize)), Some(LPARAM(30)));
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND if (wp.0 & 0xffff) as i32 == ID_BUTTON => { let _ = DestroyWindow(hwnd); }
            WM_TIMER if wp.0 == TIMER_CLOSE => { let _ = DestroyWindow(hwnd); }
            WM_CLOSE => { let _ = DestroyWindow(hwnd); }
            WM_DESTROY => {
                CONTROLS.set(None);
                SPEED.set(None);
            }
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        LRESULT(0)
    }
}

/// "1.23 GB" / "456 MB".
pub fn gb(bytes: u64) -> String {
    if bytes >= 1_000_000_000 { format!("{:.2} GB", bytes as f64 / 1e9) } else { format!("{} MB", bytes / 1_000_000) }
}

/// "45 s" / "3 min 20 s" / "1 h 5 min".
fn duration(secs: f64) -> String {
    let s = secs.round() as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_durations() {
        assert_eq!(gb(3_095_033_483), "3.10 GB");
        assert_eq!(gb(684_913_404), "684 MB");
        assert_eq!(duration(44.6), "45 s");
        assert_eq!(duration(200.0), "3 min 20 s");
        assert_eq!(duration(3900.0), "1 h 5 min");
    }
}
