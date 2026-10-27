//! Transcription with whisper-large-v3, run by the prebuilt `whisper-cli` of a pinned whisper.cpp
//! release in a child process (a crashing CUDA kernel can't take the tray app down with it).
//!
//! Picking a backend that works is the hard part, so nothing is trusted untested:
//! 1. `gpu::plan` lists the CUDA backends this GPU and driver should support, best first. Without
//!    a usable CUDA GPU there is no transcription (no CPU fallback); recordings are still saved.
//! 2. Each is downloaded (pinned SHA-256) and must pass a self-test: transcribing a known clip
//!    (JFK, bundled) correctly, *on the GPU* (whisper-cli silently running on the CPU fails it).
//!    The verdict is cached per backend, GPU, driver and model, so a driver update re-tests.
//! 3. If a backend still fails on a real recording, the next one in the plan takes over.
use crate::config::{self, Config};
use crate::gpu::{self, Backend, Cuda, Gpu};
use crate::{audio, download, language};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

/// The whisper.cpp release whose prebuilt Windows binaries we run (b5454 = v1.9.5).
pub const WHISPER_CPP: &str = "b5454";

/// A downloadable file with its pinned size and SHA-256.
struct Asset {
    url: &'static str,
    file: &'static str,
    size: u64,
    sha256: &'static str,
}

/// Bumped when what goes into a runtime folder changes, so cached self-test verdicts are redone.
const RUNTIME_REV: u32 = 3;

/// NVIDIA libraries a backend's zip lacks, from NVIDIA's redistributable archives (checksums from
/// developer.download.nvidia.com/compute/cuda/redist/redistrib_<version>.json). Their DLLs are
/// copied next to `whisper-cli.exe`.
fn runtime_extras(b: Backend) -> &'static [Asset] {
    match b {
        // whisper.cpp's CUDA 11.8 zip has no cuBLAS, which its ggml-cuda.dll links against; without
        // it the CUDA backend silently fails to load and whisper-cli runs on the CPU.
        Backend::Cuda11 => &[Asset {
            url: "https://developer.download.nvidia.com/compute/cuda/redist/libcublas/windows-x86_64/libcublas-windows-x86_64-11.11.3.6-archive.zip",
            file: "libcublas-windows-x86_64-11.11.3.6-archive.zip",
            size: 420_850_025,
            sha256: "67b0934a6359e4ee26fff823c356021589d392c4fd49ca12624f570edc08e2b9",
        }],
        Backend::Cuda12 => &[],
    }
}

/// The Visual C++ runtime every whisper.cpp build needs; Windows doesn't always have it.
const VC_REDIST: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// Whether the Visual C++ 2015-2022 runtime (x64) is installed.
fn has_vc_runtime() -> bool {
    let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("System32");
    ["msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"].iter().all(|d| system.join(d).exists())
}

/// The whisper.cpp runtime zip of a backend.
fn runtime_asset(b: Backend) -> Asset {
    match b {
        Backend::Cuda12 => Asset {
            url: "https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-win-cuda-12.4.0-x64.zip",
            file: "whisper-bin-win-cuda-12.4.0-x64.zip",
            size: 684_913_404,
            sha256: "afef0b881c500958921c3f5523b50e59ee2ec9b6f5cbd25b324c51ed308a957a",
        },
        Backend::Cuda11 => Asset {
            url: "https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-win-cuda-11.8.0-x64.zip",
            file: "whisper-bin-win-cuda-11.8.0-x64.zip",
            size: 284_761_231,
            sha256: "e81c86c542be6e3d7b7245392d4e1c52ab6e5c61dbe38721754bc0ab59d4d027",
        },
    }
}

/// whisper-large-v3 in whisper.cpp's format: full precision, or 5-bit quantized for small GPUs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Model {
    LargeV3,
    LargeV3Q5,
}

impl Model {
    fn asset(self) -> Asset {
        match self {
            Model::LargeV3 => Asset {
                url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
                file: "ggml-large-v3.bin",
                size: 3_095_033_483,
                sha256: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
            },
            Model::LargeV3Q5 => Asset {
                url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-q5_0.bin",
                file: "ggml-large-v3-q5_0.bin",
                size: 1_081_140_203,
                sha256: "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
            },
        }
    }

    /// The full model needs about 4 GiB of GPU memory; smaller GPUs get the quantized one.
    pub fn for_gpu(gpu: &Gpu) -> Model {
        if gpu.vram < 6 << 30 { Model::LargeV3Q5 } else { Model::LargeV3 }
    }
}

/// Silero voice activity detection in whisper.cpp's format. Only speech is passed to whisper:
/// on silence large-v3 makes up phrases ("Danke fürs Zuschauen") and repeats them for minutes.
const VAD_MODEL: Asset = Asset {
    url: "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin",
    file: "ggml-silero-v5.1.2.bin",
    size: 885_098,
    sha256: "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf",
};

/// Self-test clip (public domain, from whisper.cpp's samples) and words its transcript must contain.
const JFK_WAV: &[u8] = include_bytes!("../assets/jfk.wav");
const JFK_WORDS: &str = "ask not what your country can do for you";

/// Work for the engine thread.
pub enum Job {
    /// Download what's needed and find a working backend (at start, so the first recording is quick).
    Prepare,
    /// Mix a finished recording into `<base>.wav` and transcribe it to `<base>.txt` / `.srt`.
    Transcribe { tracks: Vec<PathBuf>, base: PathBuf },
    /// Transcribe an existing audio file again, replacing the `.txt` / `.srt` next to it.
    Retranscribe(PathBuf),
    /// Test every backend this machine supports (ignoring cached verdicts) and write a report.
    SelfTest,
}

/// What the engine tells the UI.
pub enum Report {
    /// Current activity, for the tooltip.
    Status(String),
    /// Transcription is ready: on which GPU, engine and driver.
    Ready(Ready),
    /// A download is running: `done` of `total` bytes of `what`.
    Download { what: String, done: u64, total: u64 },
    /// Preparing failed; transcription will be retried with the next recording.
    Unavailable(String),
    /// A recording was saved; the transcript too, unless `error`.
    /// `language` is the one transcribed: detected (with "auto") or the fixed setting.
    Done { audio: PathBuf, transcript: Option<PathBuf>, language: Option<String>, error: Option<String> },
    /// A self-test report was written.
    SelfTest(PathBuf),
    /// No more queued jobs.
    Idle,
}

/// Where transcription runs, for the user.
#[derive(Clone, PartialEq, Debug)]
pub struct Ready {
    pub gpu: String,
    pub vram_gib: f64,
    /// The whisper.cpp build's CUDA version, e.g. "CUDA 12.4".
    pub engine: &'static str,
    /// Newest CUDA version the driver supports, e.g. "13.4".
    pub driver_cuda: String,
    pub model: &'static str,
}

impl Ready {
    /// "NVIDIA GeForce RTX 5090 · CUDA 12.4"
    pub fn short(&self) -> String {
        format!("{} · {}", self.gpu, self.engine)
    }

    /// Two lines with everything: GPU and memory; engine, driver and model.
    pub fn long(&self) -> String {
        format!("{} ({:.0} GB)
{} engine · driver supports CUDA {} · whisper {}", self.gpu, self.vram_gib, self.engine, self.driver_cuda, self.model)
    }
}

/// Handle to the engine thread.
pub struct Engine {
    tx: mpsc::Sender<Job>,
}

impl Engine {
    /// Starts the engine thread; it prepares right away. `report` is called from that thread.
    pub fn spawn(report: impl Fn(Report) + Send + 'static) -> Engine {
        let (tx, rx) = mpsc::channel::<Job>();
        let _ = tx.send(Job::Prepare);
        thread::spawn(move || {
            let mut worker = Worker::new(Box::new(report));
            for job in rx.iter() {
                worker.handle_safely(job);
                while let Ok(next) = rx.try_recv() {
                    worker.handle_safely(next);
                }
                (worker.report)(Report::Idle);
            }
        });
        Engine { tx }
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

/// The backend in use and the files it runs with.
#[derive(Clone)]
struct Choice {
    backend: Backend,
    gpu: Gpu,
    /// The driver's CUDA version, e.g. "13.4".
    driver_cuda: String,
    cli: PathBuf,
    model: PathBuf,
    model_kind: Model,
    vad: PathBuf,
}

impl Choice {
    fn describe(&self) -> String {
        format!("{} on {}", self.backend.label(), self.gpu.name)
    }

    fn ready(&self) -> Ready {
        Ready {
            gpu: self.gpu.name.clone(),
            vram_gib: self.gpu.vram as f64 / (1u64 << 30) as f64,
            engine: self.backend.label(),
            driver_cuda: self.driver_cuda.clone(),
            model: match self.model_kind { Model::LargeV3 => "large-v3", Model::LargeV3Q5 => "large-v3 (5-bit)" },
        }
    }
}

/// Cached self-test verdicts by fingerprint (backend, runtime, GPU, driver, model).
#[derive(Serialize, Deserialize, Default)]
struct State {
    verdicts: HashMap<String, Verdict>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Verdict {
    ok: bool,
    detail: String,
    /// Unix seconds; failures are retried after `RETRY_FAILED`.
    at: u64,
}

const RETRY_FAILED: u64 = 3 * 24 * 60 * 60;
const SELFTEST_TIMEOUT: Duration = Duration::from_secs(30 * 60); // first run may JIT-compile kernels

pub struct Worker {
    report: Box<dyn Fn(Report) + Send>,
    choice: Option<Choice>,
    data: PathBuf,
}

impl Worker {
    pub fn new(report: Box<dyn Fn(Report) + Send>) -> Worker {
        Worker { report, choice: None, data: config::data_dir() }
    }

    /// `handle`, but a panic (a bug) is reported instead of silently ending the engine thread.
    fn handle_safely(&mut self, job: Job) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(job)));
        if let Err(panic) = result {
            let what = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()));
            self.choice = None;
            (self.report)(Report::Unavailable(format!("Internal error: {}", what.unwrap_or_default())));
        }
    }

    pub fn handle(&mut self, job: Job) {
        let cfg = Config::load().unwrap_or_default();
        match job {
            Job::Prepare => {
                if let Err(e) = self.prepare(&cfg, &[]) {
                    (self.report)(Report::Unavailable(e));
                }
            }
            Job::Transcribe { tracks, base } => {
                let done = self.transcribe(&cfg, &tracks, &base);
                (self.report)(done);
            }
            Job::Retranscribe(audio) => {
                let done = self.transcribe_file(&cfg, &audio);
                (self.report)(done);
            }
            Job::SelfTest => {
                let path = self.selftest_report(&cfg);
                (self.report)(Report::SelfTest(path));
            }
        }
    }

    fn status(&self, s: impl Into<String>) {
        (self.report)(Report::Status(s.into()));
    }

    /// Finds the best backend that passes its self-test, downloading as needed; skips `exclude`.
    fn prepare(&mut self, cfg: &Config, exclude: &[Backend]) -> Result<Choice, String> {
        if let Some(c) = &self.choice {
            if !exclude.contains(&c.backend) && c.cli.exists() && c.model.exists() && c.vad.exists() {
                return Ok(c.clone());
            }
        }
        if !has_vc_runtime() {
            return Err(format!("The Microsoft Visual C++ Redistributable (x64) is missing. Install it from {VC_REDIST} and try again."));
        }
        let cuda = gpu::detect();
        let plan = gpu::plan(cuda.as_ref(), &cfg.backend)?;
        let Some((cuda, gpu)) = cuda.as_ref().and_then(|c| Some((c, c.best()?.clone()))) else {
            return Err(gpu::no_cuda(cuda.as_ref())); // a non-empty plan implies both
        };
        let model = Model::for_gpu(&gpu);
        let mut state = self.load_state();
        let mut problems = Vec::new();
        for b in plan.into_iter().filter(|b| !exclude.contains(b)) {
            let key = fingerprint(b, cuda, &gpu, model);
            if let Some(v) = state.verdicts.get(&key) {
                if !v.ok && now() - v.at < RETRY_FAILED {
                    problems.push(format!("{}: {}", b.label(), v.detail));
                    continue;
                }
            }
            let cli = match self.ensure_runtime(b) {
                Ok(c) => c,
                Err(e) => {
                    problems.push(format!("{}: {e}", b.label()));
                    continue;
                }
            };
            let model_path = self.ensure_model(model)?;
            let vad = self.ensure_vad()?;
            let choice = Choice { backend: b, gpu: gpu.clone(), driver_cuda: cuda.driver_text(), cli, model: model_path, model_kind: model, vad };
            if state.verdicts.get(&key).is_some_and(|v| v.ok) {
                self.choice = Some(choice.clone());
                (self.report)(Report::Ready(choice.ready()));
                return Ok(choice);
            }
            self.status(format!("Testing {}…", choice.describe()));
            let result = self.selftest(&choice);
            let verdict = Verdict { ok: result.is_ok(), detail: result.clone().unwrap_or_else(|e| e), at: now() };
            state.verdicts.insert(key, verdict);
            self.save_state(&state);
            match result {
                Ok(_) => {
                    self.choice = Some(choice.clone());
                    (self.report)(Report::Ready(choice.ready()));
                    return Ok(choice);
                }
                Err(e) => problems.push(format!("{}: {e}", b.label())),
            }
        }
        Err(format!("No working transcription engine. {}", problems.join(" | ")))
    }

    /// Makes sure `b`'s runtime, with its extra NVIDIA libraries, is downloaded and unpacked;
    /// returns its `whisper-cli.exe`.
    fn ensure_runtime(&self, b: Backend) -> Result<PathBuf, String> {
        let cli = self.ensure_runtime_zip(b)?;
        for extra in runtime_extras(b) {
            let marker = cli.with_file_name(format!(".{}.complete", extra.file));
            if marker.exists() {
                continue;
            }
            let zip = self.data.join("downloads").join(extra.file);
            self.download(extra, &zip, &format!("{} libraries", b.label()))?;
            self.status(format!("Unpacking {} libraries…", b.label()));
            let mut archive = fs::File::open(&zip).map_err(|e| e.to_string()).and_then(|f| zip::ZipArchive::new(f).map_err(|e| e.to_string()))?;
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
                let Some(name) = entry.enclosed_name().and_then(|p| p.file_name().map(|n| n.to_owned())) else { continue };
                if entry.is_file() && name.to_string_lossy().to_lowercase().ends_with(".dll") {
                    let mut out = fs::File::create(cli.with_file_name(&name)).map_err(|e| e.to_string())?;
                    std::io::copy(&mut entry, &mut out).map_err(|e| format!("Unpacking {}: {e}", extra.file))?;
                }
            }
            fs::write(&marker, extra.sha256).map_err(|e| e.to_string())?;
            let _ = fs::remove_file(&zip);
            let _ = fs::remove_file(download::with_suffix(&zip, ".sha256"));
        }
        Ok(cli)
    }

    /// Makes sure `b`'s whisper.cpp zip is downloaded and unpacked; returns its `whisper-cli.exe`.
    fn ensure_runtime_zip(&self, b: Backend) -> Result<PathBuf, String> {
        let dir = self.data.join("runtime").join(format!("{WHISPER_CPP}-{}", b.id()));
        let complete = dir.join(".complete");
        if complete.exists() {
            if let Some(cli) = find_file(&dir, "whisper-cli.exe") {
                return Ok(cli);
            }
        }
        let a = runtime_asset(b);
        let zip = self.data.join("downloads").join(a.file);
        self.download(&a, &zip, &format!("{} engine", b.label()))?;
        self.status(format!("Unpacking {} engine…", b.label()));
        let tmp = download::with_suffix(&dir, ".unpacking");
        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::remove_dir_all(&dir);
        let file = fs::File::open(&zip).map_err(|e| e.to_string())?;
        zip::ZipArchive::new(file).and_then(|mut z| z.extract(&tmp)).map_err(|e| format!("Unpacking {}: {e}", a.file))?;
        fs::rename(&tmp, &dir).map_err(|e| e.to_string())?;
        fs::write(&complete, a.sha256).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(&zip);
        let _ = fs::remove_file(download::with_suffix(&zip, ".sha256"));
        find_file(&dir, "whisper-cli.exe").ok_or_else(|| format!("{} has no whisper-cli.exe", a.file))
    }

    /// Makes sure `model` is downloaded; returns its path.
    fn ensure_model(&self, model: Model) -> Result<PathBuf, String> {
        let a = model.asset();
        let path = self.data.join("models").join(a.file);
        self.download(&a, &path, "whisper-large-v3 model")?;
        Ok(path)
    }

    /// Makes sure the VAD model is downloaded; returns its path.
    fn ensure_vad(&self) -> Result<PathBuf, String> {
        let path = self.data.join("models").join(VAD_MODEL.file);
        self.download(&VAD_MODEL, &path, "voice detection model")?;
        Ok(path)
    }

    fn download(&self, a: &Asset, dest: &Path, what: &str) -> Result<(), String> {
        let verified = download::with_suffix(dest, ".sha256");
        if !(dest.exists() && fs::read_to_string(&verified).is_ok_and(|m| m.trim() == a.sha256)) {
            self.status(format!("Downloading {what}…"));
        }
        let report = |done: u64, total: u64| {
            let total = if total > 0 { total } else { a.size };
            (self.report)(Report::Download { what: what.to_string(), done, total });
        };
        download::ensure(a.url, dest, a.sha256, &report)
    }

    /// Transcribes the bundled JFK clip with `c` and checks the words; returns timing on success.
    fn selftest(&self, c: &Choice) -> Result<String, String> {
        let tmp = self.tmp();
        let wav = tmp.join("jfk.wav");
        fs::write(&wav, JFK_WAV).map_err(|e| e.to_string())?;
        let prefix = tmp.join(format!("selftest-{}", c.backend.id()));
        let _ = fs::remove_file(prefix.with_extension("txt"));
        let start = Instant::now();
        let (run, _) = self.run_cli(c, &wav, &prefix, "en", &["-otxt"], Some(SELFTEST_TIMEOUT), &|_| {})?;
        let text = fs::read_to_string(prefix.with_extension("txt")).map_err(|_| "no transcript written".to_string())?;
        let _ = fs::remove_file(prefix.with_extension("txt"));
        if !normalize(&text).contains(JFK_WORDS) {
            return Err(format!("wrong transcript: \"{}\"", text.trim()));
        }
        Ok(format!("passed in {:.1} s{}", start.elapsed().as_secs_f64(), run))
    }

    /// Runs `whisper-cli` on `wav` writing `prefix`.{txt,srt,...}. `progress` gets percentages. Fails
    /// on a non-zero exit, a timeout, or a GPU backend that didn't actually use the GPU. Every run's
    /// output goes to a log file. Returns a note about the device used and, with `language` "auto",
    /// the language whisper detected.
    fn run_cli(&self, c: &Choice, wav: &Path, prefix: &Path, language: &str, outputs: &[&str],
        timeout: Option<Duration>, progress: &dyn Fn(u32)) -> Result<(String, Option<String>), String> {
        let logs = self.data.join("logs");
        let _ = fs::create_dir_all(&logs);
        let log_path = logs.join(format!("{}-{}.log", crate::timestamp(), c.backend.id()));
        let threads = thread::available_parallelism().map_or(4, |n| n.get()).min(8); // CPU-side work only
        let mut cmd = Command::new(&c.cli);
        cmd.arg("-m").arg(short(&c.model)).arg("-f").arg(short(wav)).arg("-of").arg(short_prefix(prefix))
            .arg("-l").arg(language).arg("-t").arg(threads.to_string()).arg("-pp").args(outputs)
            // Each 30 s window is decoded without the previous windows' text as prompt: with it, one
            // repeated sentence becomes the prompt for the next window and loops until the end.
            .arg("-mc").arg("0")
            .arg("--vad").arg("-vm").arg(short(&c.vad))
            .current_dir(c.cli.parent().unwrap())
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
            .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd.env("CUDA_VISIBLE_DEVICES", c.gpu.index.to_string())
            .env("CUDA_CACHE_MAXSIZE", "4294967296"); // keep JIT-compiled kernels (PTX on newer GPUs)
        let mut log = format!("{:?}\n\n", cmd);
        let mut child = cmd.spawn().map_err(|e| format!("could not start whisper-cli: {e}"))?;
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let started = Instant::now();
        let mut timed_out = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(line) => {
                    if let Some(p) = line.split("progress =").nth(1).and_then(|r| r.trim().trim_end_matches('%').trim().parse().ok()) {
                        progress(p);
                    }
                    log.push_str(&line);
                    log.push('\n');
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if timeout.is_some_and(|t| started.elapsed() > t) {
                let _ = child.kill();
                timed_out = true;
                break;
            }
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        log.push_str(&format!("\n{status:?} after {:.1} s\n", started.elapsed().as_secs_f64()));
        let _ = fs::write(&log_path, &log);

        if timed_out {
            return Err(format!("timed out after {} min", timeout.unwrap().as_secs() / 60));
        }
        if !status.success() {
            return Err(format!("whisper-cli failed ({}): {}", exit_text(status.code()), last_error(&log)));
        }
        let device = log.lines().find(|l| l.contains("using CUDA") || l.contains("CUDA0")).map(str::trim);
        if device.is_none() {
            return Err(format!("did not use the GPU (see {})", log_path.display()));
        }
        // e.g. "whisper_full_with_state: auto-detected language: de (p = 0.97)"
        let detected = log.lines().find_map(|l| l.split("auto-detected language:").nth(1))
            .and_then(|r| r.split_whitespace().next()).map(str::to_string);
        Ok((device.map(|d| format!(" ({d})")).unwrap_or_default(), detected))
    }

    /// Mixes the tracks into `<base>.wav`, then transcribes it. The audio is kept even if
    /// transcription fails.
    fn transcribe(&mut self, cfg: &Config, tracks: &[PathBuf], base: &Path) -> Report {
        let name = base.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let audio_path = base.with_extension("wav");
        self.status(format!("Saving {name}…"));
        let track_refs: Vec<&Path> = tracks.iter().map(PathBuf::as_path).collect();
        let mixed = audio::mix(&track_refs, &audio_path);
        tracks.iter().for_each(|t| { let _ = fs::remove_file(t); });
        if let Err(e) = mixed {
            return Report::Done { audio: audio_path, transcript: None, language: None, error: Some(e) };
        }
        self.transcribe_file(cfg, &audio_path)
    }

    /// Transcribes `audio_path` to the `.txt` / `.srt` next to it (replacing them), falling back to
    /// the next backend if one fails.
    fn transcribe_file(&mut self, cfg: &Config, audio_path: &Path) -> Report {
        let name = audio_path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let fail = |error: String| Report::Done { audio: audio_path.to_path_buf(), transcript: None, language: None, error: Some(error) };
        if !audio_path.exists() {
            return fail(format!("{} not found", audio_path.display()));
        }

        let tmp = self.tmp();
        let wav16 = tmp.join("job.wav");
        // Our recordings get our resampler; whisper-cli decodes anything else (other WAVs, MP3, FLAC) itself.
        let input = if audio::is_recording(audio_path) {
            if let Err(e) = audio::to_16k_mono(audio_path, &wav16) {
                return fail(e);
            }
            wav16.clone()
        } else {
            audio_path.to_path_buf()
        };
        let prefix = tmp.join("job");
        let Some(language) = language::parse(&cfg.language) else {
            let _ = fs::remove_file(&wav16);
            return fail(format!("Unknown language \"{}\" in the settings; use auto or a code such as en or de", cfg.language));
        };
        let mut excluded = Vec::new();
        let result = loop {
            let choice = match self.prepare(cfg, &excluded) {
                Ok(c) => c,
                Err(e) => break Err(e),
            };
            self.status(format!("Transcribing {name} ({})…", choice.backend.label()));
            let progress = |p: u32| self.status(format!("Transcribing {name} ({}): {p}%", choice.backend.label()));
            match self.run_cli(&choice, &input, &prefix, &language, &["-otxt", "-osrt"], None, &progress) {
                Ok((_, detected)) => break Ok(detected.unwrap_or_else(|| language.clone())),
                Err(e) => {
                    // e.g. out of GPU memory: let the next backend take over for this recording
                    self.status(format!("{} failed ({e}), trying the next engine…", choice.backend.label()));
                    excluded.push(choice.backend);
                    self.choice = None;
                    if excluded.len() == Backend::ALL.len() {
                        break Err(e);
                    }
                }
            }
        };
        let _ = fs::remove_file(&wav16);
        let language = match result {
            Ok(l) => l,
            Err(e) => return fail(e),
        };
        for ext in ["txt", "srt"] {
            if let Err(e) = move_file(&prefix.with_extension(ext), &audio_path.with_extension(ext)) {
                return fail(e);
            }
        }
        Report::Done { audio: audio_path.to_path_buf(), transcript: Some(audio_path.with_extension("txt")), language: Some(language), error: None }
    }

    /// Tests every backend the machine should support, ignoring cached verdicts, and writes a
    /// report to the output folder. Downloads every runtime needed for that.
    fn selftest_report(&mut self, cfg: &Config) -> PathBuf {
        let cuda = gpu::detect();
        let mut out = format!("Whisperer v{} GPU self-test, {}\nwhisper.cpp {WHISPER_CPP}\n\n", crate::VERSION, crate::timestamp());
        match &cuda {
            None => out.push_str("CUDA driver: not found (no NVIDIA driver)\n"),
            Some(c) => {
                out.push_str(&format!("CUDA driver: supports CUDA {}\n", c.driver_text()));
                for g in &c.gpus {
                    out.push_str(&format!("GPU {}: {} (compute {}.{}, {:.1} GiB)\n", g.index, g.name, g.cc.0, g.cc.1, g.vram as f64 / (1u64 << 30) as f64));
                }
            }
        }
        out.push('\n');
        let plan = gpu::plan(cuda.as_ref(), "auto").unwrap_or_default();
        let target = cuda.as_ref().and_then(|c| Some((c, c.best()?.clone())));
        let Some((cuda, gpu)) = target.filter(|_| !plan.is_empty()) else {
            out.push_str(&format!("{}\n", gpu::no_cuda(cuda.as_ref())));
            return self.write_report(cfg, out);
        };
        out.push_str(&format!("Testing on GPU {}\n\n", gpu.index));
        let model = Model::for_gpu(&gpu);
        let mut state = self.load_state();
        for b in Backend::ALL {
            if !plan.contains(&b) {
                out.push_str(&format!("{:10} not supported by this GPU/driver\n", b.label()));
                continue;
            }
            let result = self.ensure_runtime(b).and_then(|cli| {
                let choice = Choice { backend: b, gpu: gpu.clone(), driver_cuda: cuda.driver_text(), cli, model: self.ensure_model(model)?, model_kind: model, vad: self.ensure_vad()? };
                self.status(format!("Self-test: {}…", choice.describe()));
                self.selftest(&choice)
            });
            out.push_str(&format!("{:10} {:?}: {}\n", b.label(), model, match &result { Ok(d) => format!("OK, {d}"), Err(e) => format!("FAILED, {e}") }));
            let key = fingerprint(b, cuda, &gpu, model);
            state.verdicts.insert(key, Verdict { ok: result.is_ok(), detail: result.unwrap_or_else(|e| e), at: now() });
        }
        self.save_state(&state);
        self.choice = None;
        let chosen = self.prepare(&Config { backend: "auto".into(), ..cfg.clone() }, &[]);
        out.push_str(&format!("\nIn use: {}\n", chosen.map_or_else(|e| e, |c| c.describe())));
        out.push_str(&format!("Logs: {}\n", self.data.join("logs").display()));
        self.write_report(cfg, out)
    }

    /// Writes a self-test report to the output folder; returns its path.
    fn write_report(&self, cfg: &Config, out: String) -> PathBuf {
        let _ = fs::create_dir_all(&cfg.output_dir);
        let path = cfg.output_dir.join(format!("whisperer-selftest {}.txt", crate::timestamp()));
        let _ = fs::write(&path, out);
        path
    }

    fn tmp(&self) -> PathBuf {
        let t = self.data.join("tmp");
        let _ = fs::create_dir_all(&t);
        t
    }

    fn load_state(&self) -> State {
        fs::read_to_string(self.data.join("state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn save_state(&self, s: &State) {
        let _ = fs::write(self.data.join("state.json"), serde_json::to_string_pretty(s).unwrap_or_default());
    }
}

/// Identifies a backend + runtime + GPU + driver + model combination for cached verdicts.
fn fingerprint(b: Backend, cuda: &Cuda, g: &Gpu, model: Model) -> String {
    format!("{}|{WHISPER_CPP}.{RUNTIME_REV}|{}|{}.{}|{}|{model:?}", b.id(), g.name, g.cc.0, g.cc.1, cuda.driver)
}

/// Lowercase letters, digits and single spaces only, for comparing transcripts.
fn normalize(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most telling line of a failed run's log (a CUDA or other error message).
fn last_error(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty() && !l.contains("progress =")).collect();
    lines.iter().rev().find(|l| { let l = l.to_lowercase(); l.contains("error") || l.contains("failed") || l.contains("out of memory") })
        .or(lines.iter().rev().nth(1))
        .map_or_else(String::new, |l| l.trim().chars().take(300).collect())
}

/// Exit code as text; crashes show as NTSTATUS hex (e.g. 0xC0000005 access violation).
fn exit_text(code: Option<i32>) -> String {
    match code {
        Some(c) if c as u32 == 0xC000_0135 => format!("a DLL it needs is missing; install the Visual C++ Redistributable from {VC_REDIST}"),
        Some(c) if c < 0 => format!("crash 0x{:08X}", c as u32),
        Some(c) => format!("exit code {c}"),
        None => "killed".into(),
    }
}

/// First file named `name` under `dir` (zips may nest it in a folder).
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Some(p);
        }
    }
    None
}

/// Renames `from` to `to`, copying instead when they're on different drives.
fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to).or_else(|_| fs::copy(from, to).map(|_| { let _ = fs::remove_file(from); }))
        .map_err(|e| format!("{}: {e}", to.display()))
}

/// whisper-cli reads its arguments in the ANSI code page, so paths with other characters (e.g.
/// a user name in another script) are passed as their 8.3 short form, which is plain ASCII.
fn short(p: &Path) -> std::ffi::OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    if p.as_os_str().to_str().is_some_and(|s| s.is_ascii()) {
        return p.as_os_str().to_owned();
    }
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetShortPathNameW(windows::core::PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
    if n == 0 || n > buf.len() {
        return p.as_os_str().to_owned();
    }
    std::ffi::OsString::from_wide(&buf[..n])
}

/// `short` for an output prefix, whose files don't exist yet: shortens the folder only.
fn short_prefix(prefix: &Path) -> std::ffi::OsString {
    let mut s = short(prefix.parent().unwrap());
    s.push("\\");
    s.push(prefix.file_name().unwrap());
    s
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_transcripts() {
        let t = " And so, my fellow Americans, ask not what your country can do for you, ask what you can do for your country.";
        assert!(normalize(t).contains(JFK_WORDS));
        assert!(!normalize("[BLANK_AUDIO]").contains(JFK_WORDS));
    }

    #[test]
    fn small_gpus_get_the_quantized_model() {
        let g = |gib: u64| Gpu { index: 0, name: "g".into(), cc: (8, 6), vram: gib << 30 };
        assert_eq!(Model::for_gpu(&g(4)), Model::LargeV3Q5);
        assert_eq!(Model::for_gpu(&g(8)), Model::LargeV3);
    }

    #[test]
    fn explains_crashes() {
        assert_eq!(exit_text(Some(0xC0000409u32 as i32)), "crash 0xC0000409");
        assert_eq!(exit_text(Some(1)), "exit code 1");
        assert!(exit_text(Some(0xC0000135u32 as i32)).contains("Visual C++"));
    }

    #[test]
    fn finds_the_error_line() {
        let log = "load\nggml_cuda_compute_forward: CUDA error: no kernel image is available\nprogress = 0%\nexit\n";
        assert!(last_error(log).contains("no kernel image"));
    }

    #[test]
    fn fingerprint_changes_with_driver() {
        let g = Gpu { index: 0, name: "RTX".into(), cc: (8, 6), vram: 8 << 30 };
        let c1 = Cuda { driver: 12040, gpus: vec![g.clone()] };
        let c2 = Cuda { driver: 12080, gpus: vec![g.clone()] };
        assert_ne!(fingerprint(Backend::Cuda12, &c1, &g, Model::LargeV3), fingerprint(Backend::Cuda12, &c2, &g, Model::LargeV3));
    }
}
