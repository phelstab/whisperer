#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod audio;
mod config;
mod download;
mod engine;
mod gpu;
mod language;
mod name_dialog;
mod record;
mod tray;
mod update;
mod win;

use engine::{Engine, Job, Report};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tray::Look;
use update::Outcome;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

/// This build's version: the topmost "== V<x.y.z>" heading of README.adoc (see build.rs).
pub const VERSION: &str = env!("WHISPERER_VERSION");

/// Passed to the new exe after an update, so it waits for the old one and says what happened.
const UPDATED_FLAG: &str = "--updated";

/// Something a background thread wants the main thread to handle (see `post`).
pub enum Event {
    Engine(Report),
    Update(Outcome),
}

/// A user action from the tray.
pub enum Command {
    Toggle,
    OpenOutput,
    Settings,
    SelfTest,
    CheckUpdates,
    /// Transcribe in this language code (or "auto") from now on.
    Language(String),
    Quit,
}

static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());
static RESTART: AtomicBool = AtomicBool::new(false);

/// Queues an event for the main thread and wakes it. Callable from any thread.
pub fn post(e: Event) {
    EVENTS.lock().unwrap().push(e);
    tray::wake();
}

/// UI state, owned by the main thread.
struct App {
    engine: Engine,
    /// The running recording and its start time (the files' timestamp).
    recording: Option<(record::Recorder, String)>,
    /// The engine has work queued.
    busy: bool,
    /// What the engine last said it is doing.
    status: String,
    /// An update was installed: restart once idle.
    restart_pending: bool,
}

thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }

/// Runs `f` on the app state. Must not be nested, and `f` must not run a message loop.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with_borrow_mut(|a| f(a.as_mut().expect("app initialized")))
}

/// Updates the tray icon and tooltip from the app state.
fn refresh(app: &App) {
    let (look, tip) = match &app.recording {
        Some((r, _)) => (Look::Recording, format!("recording {}", r.devices)),
        None if app.busy => (Look::Busy, app.status.clone()),
        None => (Look::Idle, app.status.clone()),
    };
    tray::show(look, &tip);
}

/// For the tray menu: whether we're recording, a status line, and the language setting (a valid
/// code or "auto").
pub fn menu_info() -> (bool, String, String) {
    let lang = config::Config::load().ok().and_then(|c| language::parse(&c.language)).unwrap_or_else(|| "auto".into());
    with_app(|a| (a.recording.is_some(), if a.status.is_empty() { "Starting…".into() } else { a.status.clone() }, lang))
}

/// Carries out a tray command.
pub fn command(c: Command) {
    match c {
        Command::Toggle => {
            if with_app(|a| a.recording.is_some()) { stop_recording() } else { start_recording() }
        }
        Command::OpenOutput => {
            let dir = load_config().output_dir;
            let _ = std::fs::create_dir_all(&dir);
            win::open(&dir);
        }
        Command::Settings => win::open(&config::ensure_file()),
        Command::SelfTest => with_app(|a| {
            a.engine.send(Job::SelfTest);
            a.busy = true;
            a.status = "GPU self-test queued…".into();
            refresh(a);
        }),
        Command::CheckUpdates => update::check_now(),
        Command::Language(code) => {
            if let Err(e) = config::set_language(&code) {
                tray::balloon("Could not save the language", &e, None);
            }
        }
        Command::Quit => {
            let (recording, busy) = with_app(|a| (a.recording.is_some(), a.busy));
            if recording || busy {
                let text = "A recording or transcription is in progress. Quit anyway?\n\nA running recording is saved as audio without a transcript.";
                let answer = unsafe { MessageBoxW(None, &HSTRING::from(text), w!("Whisperer"), MB_YESNO | MB_ICONQUESTION) };
                if answer != IDYES {
                    return;
                }
            }
            save_on_quit();
            unsafe { PostQuitMessage(0) };
        }
    }
}

/// Loads the settings, falling back to defaults (with a notice) if the file is broken.
fn load_config() -> config::Config {
    config::Config::load().unwrap_or_else(|e| {
        tray::balloon("Settings file has an error", &e, Some(config::path()));
        config::Config::default()
    })
}

fn start_recording() {
    let stamp = timestamp();
    let tmp = config::data_dir().join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    match record::start(&tmp.join(&stamp)) {
        Ok(r) => with_app(|a| {
            a.recording = Some((r, stamp));
            refresh(a);
        }),
        Err(e) => tray::balloon("Recording failed", &e, None),
    }
}

/// Stops the recording, asks for a name and queues the transcription.
fn stop_recording() {
    let Some((recorder, stamp)) = with_app(|a| a.recording.take()) else { return };
    let tracks = recorder.stop();
    with_app(|a| { a.busy = true; refresh(a) });
    for e in tracks.iter().filter_map(|t| t.error.as_ref()) {
        tray::balloon("Part of the recording is missing", e, None);
    }
    let cfg = load_config();
    // The dialog runs a message loop, so this must happen outside `with_app`.
    let name = if cfg.ask_name { name_dialog::ask("Name this recording (optional):") } else { None };
    let base = output_base(&cfg.output_dir, &stamp, name.as_deref().map(name_dialog::sanitize).as_deref());
    let tracks = tracks.into_iter().map(|t| t.path).collect();
    with_app(|a| {
        a.engine.send(Job::Transcribe { tracks, base });
        refresh(a);
    });
}

/// On quit during a recording: saves what was recorded as audio (no transcript).
fn save_on_quit() {
    let Some((recorder, stamp)) = with_app(|a| a.recording.take()) else { return };
    let tracks = recorder.stop();
    let base = output_base(&load_config().output_dir, &stamp, None);
    let paths: Vec<&Path> = tracks.iter().map(|t| t.path.as_path()).collect();
    let _ = audio::mix(&paths, &base.with_extension("wav"));
    tracks.iter().for_each(|t| { let _ = std::fs::remove_file(&t.path); });
}

/// `<output>\<stamp> <name>` (without extension), made unique if such files exist already.
fn output_base(dir: &Path, stamp: &str, name: Option<&str>) -> PathBuf {
    let _ = std::fs::create_dir_all(dir);
    let stem = match name {
        Some(n) if !n.is_empty() => format!("{stamp} {n}"),
        _ => stamp.to_string(),
    };
    let taken = |s: &str| ["wav", "txt", "srt"].iter().any(|e| dir.join(format!("{s}.{e}")).exists());
    let unique = (1..).map(|i| if i == 1 { stem.clone() } else { format!("{stem} ({i})") }).find(|s| !taken(s)).unwrap();
    dir.join(unique)
}

/// Handles the events background threads posted (called on the tray window's wake message).
pub fn handle_events() {
    let events = std::mem::take(&mut *EVENTS.lock().unwrap());
    for e in events {
        match e {
            Event::Engine(Report::Status(s)) => with_app(|a| { a.status = s; a.busy = true; }),
            Event::Engine(Report::Ready(d)) => with_app(|a| a.status = format!("ready ({d})")),
            Event::Engine(Report::Unavailable(e)) => {
                with_app(|a| a.status = "transcription unavailable, see the last notification".into());
                tray::balloon("Transcription is not available yet", &e, None);
            }
            Event::Engine(Report::Done { audio, transcript, language: lang, error }) => {
                let name = audio.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                match (transcript, error) {
                    (Some(t), _) => {
                        let lang = lang.map(|l| format!(" · {}", language::describe(&l))).unwrap_or_default();
                        tray::balloon("Transcript ready", &format!("{name}{lang}\nClick to open."), Some(t));
                    }
                    (None, e) => tray::balloon("Transcription failed", &format!("{}\nThe audio was saved: {name}.wav", e.unwrap_or_default()),
                        audio.parent().map(Path::to_path_buf)),
                }
                with_app(|a| a.status = "ready".into());
            }
            Event::Engine(Report::SelfTest(path)) => {
                win::open(&path);
                with_app(|a| a.status = "self-test done".into());
            }
            Event::Engine(Report::Idle) => with_app(|a| a.busy = false),
            Event::Update(Outcome::UpToDate) => tray::balloon("Whisperer is up to date", &format!("v{VERSION} is the latest release."), None),
            Event::Update(Outcome::Failed(e)) => tray::balloon("Update check failed", &e, None),
            Event::Update(Outcome::Installed) => with_app(|a| a.restart_pending = true),
        }
    }
    let restart = with_app(|a| {
        refresh(a);
        a.restart_pending && !a.busy && a.recording.is_none()
    });
    if restart {
        RESTART.store(true, Ordering::Relaxed);
        unsafe { PostQuitMessage(0) };
    }
}

/// Local time as "2026-10-26 21-05-33" (sorts by time, valid in file names).
pub fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

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

/// Command-line modes for testing on a machine: `--selftest` tests every backend and prints the
/// report; `--record <seconds> [name]` records, then saves and transcribes like the tray does.
fn cli(args: &[String]) -> Option<i32> {
    let mode = args.get(1)?.as_str();
    if !matches!(mode, "--selftest" | "--record") {
        return None;
    }
    unsafe { let _ = AttachConsole(ATTACH_PARENT_PROCESS); }
    let last: std::sync::Arc<Mutex<Option<Report>>> = Default::default();
    let keep = last.clone();
    let mut worker = engine::Worker::new(Box::new(move |r| match r {
        Report::Status(s) => println!("  {s}"),
        Report::Idle => {}
        other => *keep.lock().unwrap() = Some(other),
    }));
    let cfg = config::Config::load().unwrap_or_default();
    match mode {
        "--selftest" => {
            worker.handle(Job::SelfTest);
            if let Some(Report::SelfTest(p)) = last.lock().unwrap().take() {
                println!("\n{}", std::fs::read_to_string(&p).unwrap_or_default());
                println!("Report: {}", p.display());
            }
            Some(0)
        }
        _ => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            let stamp = timestamp();
            let tmp = config::data_dir().join("tmp");
            let _ = std::fs::create_dir_all(&tmp);
            let recorder = match record::start(&tmp.join(&stamp)) {
                Ok(r) => r,
                Err(e) => { eprintln!("{e}"); return Some(1); }
            };
            println!("Recording {} for {secs} s…", recorder.devices);
            std::thread::sleep(std::time::Duration::from_secs(secs));
            let tracks = recorder.stop();
            for t in &tracks {
                println!("  track {} ({}){}", t.path.display(), t.device, t.error.as_ref().map(|e| format!(" error: {e}")).unwrap_or_default());
            }
            let base = output_base(&cfg.output_dir, &stamp, args.get(3).map(|n| name_dialog::sanitize(n)).as_deref());
            worker.handle(Job::Transcribe { tracks: tracks.into_iter().map(|t| t.path).collect(), base });
            match last.lock().unwrap().take() {
                Some(Report::Done { audio, transcript, language: lang, error }) => {
                    println!("Audio: {}", audio.display());
                    if let Some(l) = lang {
                        println!("Language: {}", language::describe(&l));
                    }
                    if let Some(t) = transcript {
                        println!("Transcript: {}\n{}", t.display(), std::fs::read_to_string(&t).unwrap_or_default());
                    }
                    if let Some(e) = error {
                        println!("Error: {e}");
                        return Some(1);
                    }
                    Some(0)
                }
                _ => Some(1),
            }
        }
    }
}

/// Sets up the tray icon, the engine (which starts downloading and testing right away) and the
/// update checker, then runs the message loop until quit; restarts into a newly installed exe.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    update::exe(); // remember our path before an update renames the file
    if let Some(code) = cli(&args) {
        std::process::exit(code);
    }
    let updated = args.iter().any(|a| a == UPDATED_FLAG);
    let Some(lock) = single_instance(updated) else { return };
    unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }

    tray::create();
    let engine = Engine::spawn(|r| post(Event::Engine(r)));
    APP.set(Some(App { engine, recording: None, busy: true, status: "starting…".into(), restart_pending: false }));
    with_app(|a| refresh(a));
    if updated {
        tray::balloon("Whisperer updated", &format!("Now running v{VERSION}."), None);
    }
    update::start();

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    tray::remove();
    unsafe { let _ = CloseHandle(lock); }
    if RESTART.load(Ordering::Relaxed) {
        let _ = std::process::Command::new(update::exe()).arg(UPDATED_FLAG).spawn();
    }
}
