//! Small Win32 helpers: the mouse position, the "start with Windows" Run-key entry and the file picker.
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

/// Asks for audio files to transcribe again, starting in `dir`; empty if cancelled.
pub fn pick_audio_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::*;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(dialog) = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) else { return Vec::new() };
        let _ = dialog.SetOptions(dialog.GetOptions().unwrap_or_default() | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM);
        let _ = dialog.SetFileTypes(&[COMDLG_FILTERSPEC { pszName: w!("Audio (*.wav, *.mp3, *.flac)"), pszSpec: w!("*.wav;*.mp3;*.flac") }]);
        let _ = dialog.SetTitle(w!("Transcribe again (replaces the transcript next to each file)"));
        if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&windows::core::HSTRING::from(dir.as_os_str()), None) {
            let _ = dialog.SetFolder(&folder);
        }
        if dialog.Show(None).is_err() {
            return Vec::new(); // cancelled
        }
        let Ok(items) = dialog.GetResults() else { return Vec::new() };
        (0..items.GetCount().unwrap_or(0)).filter_map(|i| {
            let name = items.GetItemAt(i).ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = name.to_string().ok();
            CoTaskMemFree(Some(name.0 as _));
            path.map(std::path::PathBuf::from)
        }).collect()
    }
}

/// Opens `path` (a file or folder) with its default program, like double-clicking it in Explorer.
pub fn open(path: &std::path::Path) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let target = windows::core::HSTRING::from(path.as_os_str());
    unsafe { ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL); }
}
