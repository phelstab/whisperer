//! Self-update from the public GitHub releases of phelstab/whisperer (no auth needed).
//!
//! A background thread checks the latest release shortly after start and then every few hours. When
//! its `vX.Y.Z` tag is newer than this build, the release's exe is downloaded next to ours, checked
//! against the SHA-256 digest GitHub reports, and swapped in: Windows lets a running exe be renamed,
//! so ours becomes `whisperer.exe.old` (deleted on the next start) and the download takes its name.
//! The main thread restarts into the new version once no recording or transcription is running.
use crate::download;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const LATEST: &str = "https://api.github.com/repos/phelstab/whisperer/releases/latest";
const STARTUP_DELAY: Duration = Duration::from_secs(30);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

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
static BUSY: AtomicBool = AtomicBool::new(false);

/// Path of this exe as started. Fixed at startup, because the file is renamed while an update installs.
pub fn exe() -> &'static Path {
    EXE.get_or_init(|| std::env::current_exe().expect("own exe path"))
}

/// Starts the periodic background check. Also deletes what a previous update left behind
/// (retrying while the old process may still be exiting).
pub fn start() {
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
        crate::post(crate::Event::Update(outcome));
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
    let release: Release = match download::agent().get(LATEST).header("Accept", "application/vnd.github+json").call() {
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
    let _ = fs::remove_file(download::with_suffix(&new, ".part")); // never resume into an exe
    let digest = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:"));
    download::fetch(&asset.browser_download_url, &new, digest, &|_, _| {})?;
    swap_in(&new)?;
    Ok(true)
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
    download::with_suffix(exe(), suffix)
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
