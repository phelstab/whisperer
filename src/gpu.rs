//! NVIDIA GPU detection through the CUDA driver API (`nvcuda.dll`, installed with every NVIDIA
//! driver) and the choice of which transcription backends to try, best first.
//!
//! The prebuilt whisper.cpp runtimes need, roughly:
//! - CUDA 12.4 build: a driver for CUDA 12.0+ (R525+; minor-version compatibility) and compute
//!   capability 5.0+ (Maxwell or newer). GPUs newer than the build (e.g. Blackwell, 12.x) run its
//!   PTX code, which the driver compiles on first use.
//! - CUDA 11.8 build: a driver for CUDA 11.0+ (R450+) and compute capability 3.5+ (Kepler or newer).
//!
//! These are only the candidates: each one has to pass a real transcription self-test (engine.rs)
//! before it is used. There is deliberately no CPU fallback: without a usable CUDA GPU,
//! transcription is unavailable and the user is told why.
use std::ffi::{CStr, c_char, c_int};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{s, w};

/// One CUDA device as the driver reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Gpu {
    /// CUDA device ordinal (what `CUDA_VISIBLE_DEVICES` selects).
    pub index: u32,
    pub name: String,
    /// Compute capability (major, minor), e.g. (12, 0) for an RTX 5090.
    pub cc: (i32, i32),
    /// Total memory in bytes.
    pub vram: u64,
}

/// What the CUDA driver offers on this machine.
#[derive(Clone, Debug, PartialEq)]
pub struct Cuda {
    /// Newest CUDA version the driver supports, as 1000 * major + 10 * minor (13040 = 13.4).
    pub driver: i32,
    pub gpus: Vec<Gpu>,
}

impl Cuda {
    /// The GPU to transcribe on: the one with the most memory among those any backend supports.
    pub fn best(&self) -> Option<&Gpu> {
        self.gpus.iter().filter(|g| g.cc >= (3, 5)).max_by_key(|g| g.vram)
    }

    /// The driver's CUDA version as text, e.g. "13.4".
    pub fn driver_text(&self) -> String {
        format!("{}.{}", self.driver / 1000, self.driver % 1000 / 10)
    }
}

/// A transcription runtime (a prebuilt whisper.cpp build).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Cuda12,
    Cuda11,
}

impl Backend {
    pub const ALL: [Backend; 2] = [Backend::Cuda12, Backend::Cuda11];

    /// Name used in the settings file, folder names and logs.
    pub fn id(self) -> &'static str {
        match self {
            Backend::Cuda12 => "cuda12",
            Backend::Cuda11 => "cuda11",
        }
    }

    /// Name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Cuda12 => "CUDA 12.4",
            Backend::Cuda11 => "CUDA 11.8",
        }
    }

    /// Whether this backend can run on `gpu` with a driver supporting CUDA `driver` (see module docs).
    pub fn supports(self, driver: i32, gpu: &Gpu) -> bool {
        match self {
            Backend::Cuda12 => driver >= 12000 && gpu.cc >= (5, 0),
            Backend::Cuda11 => driver >= 11000 && gpu.cc >= (3, 5),
        }
    }
}

/// The backends to try, best first, for the `setting` "auto" or a backend id. "auto" tries every
/// backend the GPU supports; a fixed backend is tried alone. Fails if nothing fits the machine.
pub fn plan(cuda: Option<&Cuda>, setting: &str) -> Result<Vec<Backend>, String> {
    let usable = |b: Backend| cuda.is_some_and(|c| c.best().is_some_and(|g| b.supports(c.driver, g)));
    if setting == "auto" {
        let plan: Vec<Backend> = Backend::ALL.into_iter().filter(|&b| usable(b)).collect();
        if plan.is_empty() {
            return Err(no_cuda(cuda));
        }
        return Ok(plan);
    }
    let b = Backend::ALL.into_iter().find(|b| b.id() == setting)
        .ok_or_else(|| format!("Unknown backend \"{setting}\" in the settings (use auto, cuda12 or cuda11)"))?;
    if !usable(b) {
        return Err(format!("{} is not supported by this machine's GPU/driver ({})", b.label(), describe(cuda)));
    }
    Ok(vec![b])
}

/// Why transcription can't run here, for the user.
pub fn no_cuda(cuda: Option<&Cuda>) -> String {
    format!("No CUDA GPU detected: {}. Whisperer needs an NVIDIA GPU (Kepler or newer) with a driver for CUDA 11 or newer; recordings are still saved.", describe(cuda))
}

/// One line about the GPU and driver, for status and reports.
pub fn describe(cuda: Option<&Cuda>) -> String {
    match cuda {
        None => "no NVIDIA GPU/driver found".into(),
        Some(c) => match c.best() {
            None => format!("no supported NVIDIA GPU (driver CUDA {})", c.driver_text()),
            Some(g) => format!("{} (compute {}.{}, {:.1} GiB), driver CUDA {}", g.name, g.cc.0, g.cc.1,
                g.vram as f64 / (1u64 << 30) as f64, c.driver_text()),
        },
    }
}

type CuInit = unsafe extern "system" fn(u32) -> c_int;
type CuDriverGetVersion = unsafe extern "system" fn(*mut c_int) -> c_int;
type CuDeviceGetCount = unsafe extern "system" fn(*mut c_int) -> c_int;
type CuDeviceGet = unsafe extern "system" fn(*mut c_int, c_int) -> c_int;
type CuDeviceGetName = unsafe extern "system" fn(*mut c_char, c_int, c_int) -> c_int;
type CuDeviceGetAttribute = unsafe extern "system" fn(*mut c_int, c_int, c_int) -> c_int;
type CuDeviceTotalMem = unsafe extern "system" fn(*mut usize, c_int) -> c_int;
const COMPUTE_CAPABILITY_MAJOR: c_int = 75;
const COMPUTE_CAPABILITY_MINOR: c_int = 76;
const CUDA_ERROR_NO_DEVICE: c_int = 100;

/// Asks the CUDA driver for its version and devices; `None` without an NVIDIA driver (or if it
/// fails to initialize, e.g. no GPU).
pub fn detect() -> Option<Cuda> {
    unsafe {
        let lib = LoadLibraryW(w!("nvcuda.dll")).ok()?;
        macro_rules! sym {
            ($name:expr, $ty:ty) => { std::mem::transmute::<_, $ty>(GetProcAddress(lib, $name)?) };
        }
        let init = sym!(s!("cuInit"), CuInit);
        let version = sym!(s!("cuDriverGetVersion"), CuDriverGetVersion);
        let count = sym!(s!("cuDeviceGetCount"), CuDeviceGetCount);
        let get = sym!(s!("cuDeviceGet"), CuDeviceGet);
        let name = sym!(s!("cuDeviceGetName"), CuDeviceGetName);
        let attribute = sym!(s!("cuDeviceGetAttribute"), CuDeviceGetAttribute);
        let total_mem = sym!(s!("cuDeviceTotalMem_v2"), CuDeviceTotalMem);

        let mut driver = 0;
        if version(&mut driver) != 0 {
            return None;
        }
        match init(0) {
            0 => {}
            CUDA_ERROR_NO_DEVICE => return Some(Cuda { driver, gpus: Vec::new() }), // driver, but no usable GPU
            _ => return None,
        }
        let mut n = 0;
        if count(&mut n) != 0 {
            return None;
        }
        let mut gpus = Vec::new();
        for i in 0..n {
            let mut dev = 0;
            if get(&mut dev, i) != 0 {
                continue;
            }
            let mut buf = [0 as c_char; 256];
            let (mut major, mut minor, mut mem) = (0, 0, 0usize);
            name(buf.as_mut_ptr(), buf.len() as c_int, dev);
            attribute(&mut major, COMPUTE_CAPABILITY_MAJOR, dev);
            attribute(&mut minor, COMPUTE_CAPABILITY_MINOR, dev);
            total_mem(&mut mem, dev);
            let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
            gpus.push(Gpu { index: i as u32, name, cc: (major, minor), vram: mem as u64 });
        }
        Some(Cuda { driver, gpus })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Backend::*;

    const GIB: u64 = 1 << 30;

    fn machine(driver: i32, gpus: &[((i32, i32), u64)]) -> Cuda {
        let gpus = gpus.iter().enumerate()
            .map(|(i, &(cc, vram))| Gpu { index: i as u32, name: format!("gpu{i}"), cc, vram: vram * GIB })
            .collect();
        Cuda { driver, gpus }
    }

    fn auto(c: Option<&Cuda>) -> Vec<Backend> {
        plan(c, "auto").unwrap_or_default()
    }

    #[test]
    fn no_usable_nvidia_gpu_means_no_cuda() {
        for c in [None, Some(machine(12080, &[])), Some(machine(12040, &[((3, 0), 8)])), Some(machine(10020, &[((7, 5), 8)]))] {
            let e = plan(c.as_ref(), "auto").unwrap_err();
            assert!(e.starts_with("No CUDA GPU detected"), "{e}");
        }
    }

    /// (driver CUDA version, compute capability, expected order) across GPU generations and drivers.
    #[test]
    fn plans_across_generations_and_drivers() {
        let cases: &[(i32, (i32, i32), &[Backend])] = &[
            // Blackwell (RTX 50xx), current and older-but-sufficient drivers
            (13040, (12, 0), &[Cuda12, Cuda11]),
            (12080, (12, 0), &[Cuda12, Cuda11]),
            // Hopper / Ada / Ampere / Turing / Volta / Pascal / Maxwell
            (12040, (9, 0), &[Cuda12, Cuda11]),
            (12020, (8, 9), &[Cuda12, Cuda11]),
            (12000, (8, 6), &[Cuda12, Cuda11]),
            (12000, (7, 5), &[Cuda12, Cuda11]),
            (12000, (7, 0), &[Cuda12, Cuda11]),
            (12000, (6, 1), &[Cuda12, Cuda11]),
            (12000, (5, 0), &[Cuda12, Cuda11]),
            // Old drivers that only know CUDA 11.x
            (11080, (8, 6), &[Cuda11]),
            (11040, (7, 5), &[Cuda11]),
            (11000, (6, 1), &[Cuda11]),
            // Kepler: CUDA 12 dropped it, 11.8 still has sm_35/37
            (12040, (3, 7), &[Cuda11]),
            (11040, (3, 5), &[Cuda11]),
            // Too old for either build
            (12040, (3, 0), &[]),
            (10020, (7, 5), &[]),
        ];
        for &(driver, cc, expected) in cases {
            let m = machine(driver, &[(cc, 8)]);
            assert_eq!(auto(Some(&m)), expected, "driver {driver}, compute {cc:?}");
        }
    }

    #[test]
    fn picks_the_supported_gpu_with_most_memory() {
        let m = machine(12080, &[((8, 6), 12), ((3, 0), 48), ((8, 9), 24), ((7, 5), 8)]);
        assert_eq!(m.best().unwrap().index, 2);
    }

    #[test]
    fn fixed_backend_is_validated() {
        let m = machine(11080, &[((8, 6), 12)]);
        assert_eq!(plan(Some(&m), "cuda11").unwrap(), [Cuda11]);
        assert!(plan(Some(&m), "cpu").is_err());
        assert!(plan(Some(&m), "cuda12").is_err());
        assert!(plan(None, "cuda11").is_err());
        assert!(plan(Some(&m), "vulkan").is_err());
    }

    #[test]
    fn driver_version_text() {
        assert_eq!(machine(13040, &[]).driver_text(), "13.4");
        assert_eq!(machine(12000, &[]).driver_text(), "12.0");
    }
}
