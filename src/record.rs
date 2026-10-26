//! Records the default microphone and, through WASAPI loopback, whatever the default output device
//! plays. Each source runs on its own thread and streams into its own temporary WAV file
//! (48 kHz stereo 16-bit; Windows converts from the device format), so long recordings don't
//! need memory. `audio::mix` combines them afterwards.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::*;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// A recorded source: its temporary WAV file, the device's name, and why it ended early, if it did.
pub struct Track {
    pub path: PathBuf,
    pub device: String,
    pub error: Option<String>,
}

/// A running recording; `stop` ends it.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Option<Track>>>,
    /// "Mic name + Speakers name", for the tooltip.
    pub devices: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Mic,
    Loopback,
}

/// Starts recording the default mic and the default output into `<tmp>-mic.wav` / `<tmp>-system.wav`.
/// Fails only if neither source can be opened; a missing mic or output device is left out.
pub fn start(tmp: &Path) -> Result<Recorder, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    let mut names = Vec::new();
    let mut errors = Vec::new();
    for (source, suffix) in [(Source::Mic, "-mic.wav"), (Source::Loopback, "-system.wav")] {
        let path = crate::download::with_suffix(tmp, suffix);
        let (ready_tx, ready_rx) = mpsc::channel();
        let stop = stop.clone();
        let handle = thread::spawn(move || capture(source, path, stop, ready_tx));
        match ready_rx.recv() {
            Ok(Ok(name)) => {
                names.push(name);
                threads.push(handle);
            }
            Ok(Err(e)) => errors.push(e),
            Err(_) => errors.push("audio thread ended unexpectedly".into()),
        }
    }
    if threads.is_empty() {
        return Err(format!("Could not record: {}", errors.join("; ")));
    }
    Ok(Recorder { stop, threads, devices: names.join(" + ") })
}

impl Recorder {
    /// Stops all sources and returns the tracks that were recorded.
    pub fn stop(self) -> Vec<Track> {
        self.stop.store(true, Ordering::Relaxed);
        self.threads.into_iter().filter_map(|t| t.join().ok().flatten()).collect()
    }
}

/// Thread body: opens `source`, reports its device name (or the error) on `ready`, then records
/// into `path` until `stop`. Returns the track, or `None` if the source never opened.
fn capture(source: Source, path: PathBuf, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<String, String>>) -> Option<Track> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let label = if source == Source::Mic { "microphone" } else { "output device" };
    let opened = unsafe { Stream::open(source) }.map_err(|e| format!("{label}: {}", e.message()));
    let stream = match opened {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e));
            return None;
        }
    };
    let spec = hound::WavSpec { channels: CHANNELS, sample_rate: RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut wav = match hound::WavWriter::create(&path, spec) {
        Ok(w) => w,
        Err(e) => {
            let _ = ready.send(Err(format!("{}: {e}", path.display())));
            return None;
        }
    };
    let device = stream.name.clone();
    let _ = ready.send(Ok(device.clone()));
    let error = unsafe { stream.run(&mut wav, &stop) }.err().map(|e| format!("{label} stopped: {}", e.message()));
    let _ = wav.finalize();
    Some(Track { path, device, error })
}

/// An opened capture stream, plus the silent playback stream that keeps a loopback stream running.
struct Stream {
    name: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    keepalive: Option<Silence>,
}

impl Stream {
    /// Opens the default endpoint for `source` in shared mode, asking Windows to deliver 48 kHz
    /// stereo float whatever the device's own format is.
    unsafe fn open(source: Source) -> windows::core::Result<Stream> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let flow = if source == Source::Mic { eCapture } else { eRender };
            let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
            let name = friendly_name(&device);
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: CHANNELS,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * CHANNELS as u32 * 4,
                nBlockAlign: CHANNELS * 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if source == Source::Loopback {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            }
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, None)?; // 200 ms buffer
            let capture: IAudioCaptureClient = client.GetService()?;
            let keepalive = if source == Source::Loopback { Some(Silence::start(&device)?) } else { None };
            client.Start()?;
            Ok(Stream { name, client, capture, keepalive })
        }
    }

    /// Moves captured audio into `wav` every 10 ms until `stop`, converting float to 16-bit.
    unsafe fn run(&self, wav: &mut hound::WavWriter<std::io::BufWriter<std::fs::File>>, stop: &AtomicBool) -> windows::core::Result<()> {
        let result = (|| unsafe {
            while !stop.load(Ordering::Relaxed) {
                if let Some(k) = &self.keepalive {
                    k.feed()?;
                }
                while self.capture.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let n = frames as usize * CHANNELS as usize;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        (0..n).for_each(|_| { let _ = wav.write_sample(0i16); });
                    } else {
                        for &s in std::slice::from_raw_parts(data as *const f32, n) {
                            let _ = wav.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                        }
                    }
                    self.capture.ReleaseBuffer(frames)?;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        unsafe {
            let _ = self.client.Stop();
            if let Some(k) = &self.keepalive {
                let _ = k.client.Stop();
            }
        }
        result
    }
}

/// Plays silence on the output device. Loopback only delivers packets while something plays, so
/// without this, quiet stretches would be missing from the system track and it would drift out
/// of sync with the mic.
struct Silence {
    client: IAudioClient,
    render: IAudioRenderClient,
    size: u32,
}

impl Silence {
    unsafe fn start(device: &IMMDevice) -> windows::core::Result<Silence> {
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, format, None);
            CoTaskMemFree(Some(format as _));
            init?;
            let size = client.GetBufferSize()?;
            let render: IAudioRenderClient = client.GetService()?;
            let s = Silence { client, render, size };
            s.feed()?;
            s.client.Start()?;
            Ok(s)
        }
    }

    /// Tops the playback buffer up with silence.
    unsafe fn feed(&self) -> windows::core::Result<()> {
        unsafe {
            let free = self.size - self.client.GetCurrentPadding()?;
            if free > 0 {
                self.render.GetBuffer(free)?;
                self.render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
            Ok(())
        }
    }
}

/// The device's name as shown in Windows' sound settings.
unsafe fn friendly_name(device: &IMMDevice) -> String {
    unsafe {
        device.OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "unknown device".into())
    }
}
