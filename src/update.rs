//! Self-update from the public GitHub releases of phelstab/whisperer (no auth needed).
//!
//! A background thread checks the latest release shortly after start and then every few hours. When
//! its `vX.Y.Z` tag is newer than this build, the release's exe is downloaded next to ours, checked
//! against the SHA-256 digest GitHub reports, and swapped in: Windows lets a running exe be renamed,
//! so ours becomes `whisperer.exe.old` (deleted on the next start) and the download takes its name.
//! The main thread is then told to restart into the new version.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use std::{fs, thread};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

const LATEST: &str = "https://api.github.com/repos/phelstab/whisperer/releases/latest";
const STARTUP_DELAY: Duration = Duration::from_secs(30);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Thread message telling the main thread that `take_outcomes` has something.
pub const WM_UPDATE: u32 = WM_APP + 2;

/// Result of an update check, handed to the main thread.
pub enum Outcome {
    /// No newer release (or none published yet). Only reported for checks the user asked for.
    UpToDate,
    /// A newer version was installed in place of this exe: restart into it.
    Installed,
    /// The check or download failed. Only reported for checks the user asked for.
    Failed(String),
}

static EXE: OnceLock<PathBuf> = OnceLock::new();
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static BUSY: AtomicBool = AtomicBool::new(false);
static OUTCOMES: Mutex<Vec<Outcome>> = Mutex::new(Vec::new());

/// Path of this exe as started. Fixed at startup, because the file is renamed while an update installs.
pub fn exe() -> &'static Path {
    EXE.get_or_init(|| std::env::current_exe().expect("own exe path"))
}

/// Starts the periodic background check, reporting to thread `main_thread`. Also deletes what a
/// previous update left behind (retrying while the old process may still be exiting).
pub fn start(main_thread: u32) {
    MAIN_THREAD.store(main_thread, Ordering::Relaxed);
    let (old, new) = (sibling(".old"), sibling(".new"));
    thread::spawn(move || {
        for _ in 0..20 {
            let _ = fs::remove_file(&new);
            if fs::remove_file(&old).is_ok() || !old.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        thread::sleep(STARTUP_DELAY);
        loop {
            run(false);
            thread::sleep(INTERVAL);
        }
    });
}

/// Checks for an update right away on a background thread and reports the outcome either way.
pub fn check_now() {
    thread::spawn(|| run(true));
}

/// Takes the outcomes reported since the last call (main thread, on `WM_UPDATE`).
pub fn take_outcomes() -> Vec<Outcome> {
    std::mem::take(&mut *OUTCOMES.lock().unwrap())
}

/// One check (skipped if another is running); `manual` also reports "up to date" and failures.
fn run(manual: bool) {
    if BUSY.swap(true, Ordering::Acquire) {
        return;
    }
    let outcome = match check_and_install() {
        Ok(true) => Outcome::Installed,
        Ok(false) => Outcome::UpToDate,
        Err(e) => Outcome::Failed(e),
    };
    BUSY.store(false, Ordering::Release);
    if manual || matches!(outcome, Outcome::Installed) {
        OUTCOMES.lock().unwrap().push(outcome);
        unsafe { let _ = PostThreadMessageW(MAIN_THREAD.load(Ordering::Relaxed), WM_UPDATE, WPARAM(0), LPARAM(0)); }
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// "sha256:<hex>", reported by GitHub for release assets.
    digest: Option<String>,
}

/// Installs the latest release if it is newer than this build; returns whether it did.
fn check_and_install() -> Result<bool, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(format!("whisperer/{}", crate::VERSION))
        .build()
        .into();
    let release: Release = match agent.get(LATEST).header("Accept", "application/vnd.github+json").call() {
        Ok(mut r) => r.body_mut().read_json().map_err(|e| format!("Unreadable release info: {e}"))?,
        Err(ureq::Error::StatusCode(404)) => return Ok(false), // nothing released yet
        Err(e) => return Err(format!("Could not reach GitHub: {e}")),
    };
    let (Some(latest), Some(current)) = (parse_version(&release.tag_name), parse_version(crate::VERSION)) else {
        return Err(format!("Unexpected release tag {}", release.tag_name));
    };
    if latest <= current {
        return Ok(false);
    }
    let wanted = format!("whisperer-{}.exe", release.tag_name);
    let asset = release.assets.iter().find(|a| a.name == wanted)
        .ok_or_else(|| format!("Release {} has no {wanted}", release.tag_name))?;

    let new = sibling(".new");
    download(&agent, asset, &new).inspect_err(|_| { let _ = fs::remove_file(&new); })?;
    swap_in(&new)?;
    Ok(true)
}

/// Downloads `asset` to `to`, verifying its SHA-256 digest when GitHub reports one.
fn download(agent: &ureq::Agent, asset: &Asset, to: &Path) -> Result<(), String> {
    let fail = |e: &dyn std::fmt::Display| format!("Download of {} failed: {e}", asset.name);
    let response = agent.get(&asset.browser_download_url).call().map_err(|e| fail(&e))?;
    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(to).map_err(|e| fail(&e))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| fail(&e))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| fail(&e))?;
    }
    file.sync_all().map_err(|e| fail(&e))?;
    if let Some(expected) = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")) {
        let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(fail(&"checksum mismatch"));
        }
    }
    Ok(())
}

/// Puts the downloaded exe `new` in place of ours, moving ours aside to `.old` (restored on failure).
fn swap_in(new: &Path) -> Result<(), String> {
    let (exe, old) = (exe(), sibling(".old"));
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).map_err(|e| format!("Could not replace {}: {e}", exe.display()))?;
    fs::rename(new, exe).map_err(|e| {
        let _ = fs::rename(&old, exe);
        format!("Could not replace {}: {e}", exe.display())
    })
}

/// Our exe's path with `suffix` appended, e.g. `whisperer.exe.old`.
fn sibling(suffix: &str) -> PathBuf {
    let mut p = exe().as_os_str().to_owned();
    p.push(suffix);
    p.into()
}

/// Parses "1.2.3" or "v1.2.3" into comparable numbers.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.parse().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_tags_and_versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert_eq!(parse_version("v1.2.x"), None);
    }

    #[test]
    fn compares_numerically() {
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
        assert!(parse_version("v1.0.0") > parse_version("v0.99.99"));
    }

    #[test]
    fn this_build_has_a_version() {
        assert!(parse_version(crate::VERSION).is_some());
    }
}
