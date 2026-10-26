//! HTTPS downloads with SHA-256 verification and resume, used for the model, the whisper.cpp
//! runtimes and self-updates.
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// HTTP client with a connect timeout and our user agent (GitHub's API requires one).
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(format!("whisperer/{}", crate::VERSION))
        .build()
        .into()
}

/// Downloads `url` to `dest`, verifying `sha256` (hex) when given. Bytes arrive in `dest` + ".part"
/// first: a later call continues an interrupted download, and `dest` only appears once verified.
/// `progress` gets (bytes done, bytes total; 0 if unknown).
pub fn fetch(url: &str, dest: &Path, sha256: Option<&str>, progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    let name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let fail = |e: &dyn std::fmt::Display| format!("Download of {name} failed: {e}");
    let part = with_suffix(dest, ".part");
    let mut hash = Sha256::new();
    let mut done = fs::metadata(&part).map_or(0, |m| m.len());
    if done > 0 {
        let mut f = File::open(&part).map_err(|e| fail(&e))?;
        copy(&mut f, None, &mut hash, &mut 0, 0, &|_, _| {}).map_err(|e| fail(&e))?;
    }

    let mut request = agent().get(url);
    if done > 0 {
        request = request.header("Range", format!("bytes={done}-"));
    }
    let response = request.call().map_err(|e| fail(&e))?;
    if done > 0 && response.status() != 206 {
        done = 0; // server ignored the range: start over
        hash = Sha256::new();
    }
    let length = response.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let total = length.map_or(0, |l| l + done);
    let mut file = OpenOptions::new().create(true).write(true).append(done > 0).truncate(done == 0)
        .open(&part).map_err(|e| fail(&e))?;
    let mut reader = response.into_body().into_reader();
    copy(&mut reader, Some(&mut file), &mut hash, &mut done, total, progress).map_err(|e| fail(&e))?;
    file.sync_all().map_err(|e| fail(&e))?;
    drop(file);

    if let Some(expected) = sha256 {
        let actual = hex(hash);
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&part);
            return Err(fail(&"checksum mismatch (the partial file was deleted, try again)"));
        }
    }
    fs::rename(&part, dest).map_err(|e| fail(&e))
}

/// Makes sure `dest` holds the file with `sha256`, downloading it from `url` if needed. A verified
/// file gets a `.sha256` marker next to it, so it is only hashed once (big models take a while).
pub fn ensure(url: &str, dest: &Path, sha256: &str, progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    let marker = with_suffix(dest, ".sha256");
    if dest.exists() && fs::read_to_string(&marker).is_ok_and(|m| m.trim() == sha256) {
        return Ok(());
    }
    if dest.exists() && hash_file(dest).is_ok_and(|h| h.eq_ignore_ascii_case(sha256)) {
        return fs::write(&marker, sha256).map_err(|e| e.to_string());
    }
    let _ = fs::remove_file(dest);
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fetch(url, dest, Some(sha256), progress)?;
    fs::write(&marker, sha256).map_err(|e| e.to_string())
}

/// SHA-256 of the file at `path`, as lowercase hex.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    copy(&mut File::open(path)?, None, &mut hash, &mut 0, 0, &|_, _| {})?;
    Ok(hex(hash))
}

/// Copies `from` into `to` (if any) while hashing, advancing `done` and reporting progress.
fn copy(from: &mut dyn Read, mut to: Option<&mut File>, hash: &mut Sha256, done: &mut u64, total: u64,
    progress: &dyn Fn(u64, u64)) -> std::io::Result<()> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut reported = 0;
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            progress(*done, total);
            return Ok(());
        }
        hash.update(&buf[..n]);
        if let Some(f) = to.as_deref_mut() {
            f.write_all(&buf[..n])?;
        }
        *done += n as u64;
        if *done - reported >= 8 << 20 {
            reported = *done;
            progress(*done, total);
        }
    }
}

fn hex(hash: Sha256) -> String {
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `path` with `suffix` appended to its file name, e.g. `model.bin.part`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}
