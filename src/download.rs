//! HTTPS downloads with SHA-256 verification and resume, used for the model, the whisper.cpp
//! runtimes and self-updates.
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// HTTP client with a connect timeout and our user agent (GitHub's API requires one). TLS is
/// Windows' own (SChannel, system certificate store); ureq defaults to Rustls, which isn't built in
/// and would panic on the first https request.
pub fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Real download through GitHub's redirect to its CDN (8.9 MB). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn downloads_a_github_release_asset() {
        let dest = std::env::temp_dir().join(format!("whisperer-dl-test-{}.zip", std::process::id()));
        let url = "https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-x64.zip";
        let sha = "6ba69e3482d7826214f90a6a9c84ca07782aec1e1d0c6a7c30c994fd5d816ccb";
        let last = std::cell::Cell::new((0, 0));
        ensure(url, &dest, sha, &|d, t| last.set((d, t))).unwrap();
        assert_eq!(last.get(), (8_928_640, 8_928_640), "progress reaches the total");
        let _ = fs::remove_file(&dest);
        let _ = fs::remove_file(with_suffix(&dest, ".sha256"));
    }

    /// Real download from Hugging Face (redirects to its CDN), interrupted and resumed: the first
    /// 10 MB are kept as `.part` and the rest is fetched with a Range request (78 MB in total).
    #[test]
    #[ignore]
    fn resumes_a_hugging_face_download() {
        let dir = std::env::temp_dir().join(format!("whisperer-resume-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin";
        let sha = "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21";
        let full = dir.join("full.bin");
        fetch(url, &full, Some(sha), &|_, _| {}).unwrap();
        let resumed = dir.join("resumed.bin");
        let bytes = fs::read(&full).unwrap();
        fs::write(with_suffix(&resumed, ".part"), &bytes[..10 << 20]).unwrap();
        let first = std::cell::Cell::new(None);
        fetch(url, &resumed, Some(sha), &|d, t| { if first.get().is_none() { first.set(Some((d, t))) } }).unwrap();
        let (done, total) = first.get().unwrap();
        assert!(done >= 10 << 20, "continued after the kept part, not from 0 ({done})");
        assert_eq!(total, 77_691_713);
        fs::remove_dir_all(&dir).unwrap();
    }
}
