//! The small "name this recording" window shown when a recording stops.
use std::cell::{Cell, RefCell};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

const ID_EDIT: i32 = 100;

thread_local! {
    static RESULT: RefCell<Option<String>> = const { RefCell::new(None) };
    static OPEN: Cell<bool> = const { Cell::new(false) };
    static EDIT: Cell<HWND> = Cell::new(HWND::default());
}

/// Asks for a name; returns what was typed (trimmed, possibly empty), or `None` if skipped.
/// Runs its own message loop, so the tray keeps working meanwhile.
pub fn ask(prompt: &str) -> Option<String> {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default().into();
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszClassName: w!("whisperer-name"),
            ..Default::default()
        });
        let scale = |v: i32| v * GetDpiForSystem() as i32 / 96;
        let (w, h) = (scale(420), scale(150));
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_DLGMODALFRAME, w!("whisperer-name"), w!("Whisperer"),
            WS_CAPTION | WS_SYSMENU, (work.left + work.right - w) / 2, (work.top + work.bottom - h) / 2, w, h,
            None, None, Some(instance), None);
        let Ok(hwnd) = hwnd else { return Some(String::new()) };

        let mut metrics = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ = SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, metrics.cbSize, Some(&mut metrics as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let font = CreateFontIndirectW(&metrics.lfMessageFont);
        let child = |class, text: &HSTRING, style: WINDOW_STYLE, ex, x, y, cw, ch, id: i32| {
            let c = CreateWindowExW(ex, class, text, WS_CHILD | WS_VISIBLE | style, scale(x), scale(y), scale(cw), scale(ch),
                Some(hwnd), Some(HMENU(id as _)), Some(instance), None).unwrap_or_default();
            SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            c
        };
        child(w!("STATIC"), &HSTRING::from(prompt), WINDOW_STYLE(0), WINDOW_EX_STYLE(0), 12, 12, 380, 20, 0);
        let edit = child(w!("EDIT"), &HSTRING::new(), WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32), WS_EX_CLIENTEDGE, 12, 36, 380, 24, ID_EDIT);
        child(w!("BUTTON"), &HSTRING::from("Save"), WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32), WINDOW_EX_STYLE(0), 210, 72, 88, 28, IDOK.0);
        child(w!("BUTTON"), &HSTRING::from("Skip"), WS_TABSTOP, WINDOW_EX_STYLE(0), 304, 72, 88, 28, IDCANCEL.0);
        EDIT.set(edit);
        RESULT.set(None);
        OPEN.set(true);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(edit));
        let mut msg = MSG::default();
        while OPEN.get() {
            match GetMessageW(&mut msg, None, 0, 0).0 {
                0 => { PostQuitMessage(msg.wParam.0 as i32); break; } // pass WM_QUIT on to the main loop
                -1 => break,
                _ => {}
            }
            // Enter = Save (IDOK), Esc = Skip (IDCANCEL), Tab moves between controls
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = DeleteObject(font.into());
        RESULT.take()
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND if (wp.0 & 0xffff) as i32 == IDOK.0 => {
                let edit = EDIT.get();
                let mut buf = vec![0u16; GetWindowTextLengthW(edit) as usize + 1];
                let n = GetWindowTextW(edit, &mut buf) as usize;
                RESULT.set(Some(String::from_utf16_lossy(&buf[..n]).trim().to_string()));
                let _ = DestroyWindow(hwnd);
            }
            WM_COMMAND if (wp.0 & 0xffff) as i32 == IDCANCEL.0 => { let _ = DestroyWindow(hwnd); }
            WM_CLOSE => { let _ = DestroyWindow(hwnd); }
            WM_DESTROY => OPEN.set(false),
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        LRESULT(0)
    }
}

/// Makes `name` safe as part of a file name: drops characters Windows forbids, trims, caps length.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control() && !r#"<>:"/\|?*"#.contains(*c)).take(100).collect();
    cleaned.trim().trim_end_matches('.').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(sanitize("  Team call: Q4/plan?  "), "Team call Q4plan");
        assert_eq!(sanitize("Besprechung Müller"), "Besprechung Müller");
        assert_eq!(sanitize("..."), "");
        assert_eq!(sanitize("a\tb"), "ab");
        assert_eq!(sanitize(&"x".repeat(300)).len(), 100);
    }
}
