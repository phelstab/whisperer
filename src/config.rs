//! User settings (`%APPDATA%\Whisperer\config.toml`) and the app's folders.
use serde::Deserialize;
use std::path::PathBuf;
use std::{env, fs};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

#[derive(Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Folder for recordings and transcripts.
    pub output_dir: PathBuf,
    /// Spoken language ("en", "de", ...) or "auto".
    pub language: String,
    /// Ask for a name when a recording stops.
    pub ask_name: bool,
    /// "auto", "cuda12" or "cuda11".
    pub backend: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { output_dir: documents().join("Whisperer"), language: "auto".into(), ask_name: true, backend: "auto".into() }
    }
}

impl Config {
    /// Reads the settings file; a missing file means defaults.
    pub fn load() -> Result<Config, String> {
        match fs::read_to_string(path()) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path().display())),
            Err(_) => Ok(Config::default()),
        }
    }
}

/// The settings file.
pub fn path() -> PathBuf {
    PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join("Whisperer").join("config.toml")
}

/// Writes a commented settings file with the defaults if there is none yet; returns its path.
pub fn ensure_file() -> PathBuf {
    let p = path();
    if !p.exists() {
        let d = Config::default();
        let text = format!(
            "# Whisperer settings. Changes apply to the next recording.\n\n\
             # Folder for recordings (.wav) and transcripts (.txt, .srt).\n\
             output_dir = '{}'\n\n\
             # Spoken language as a code such as \"en\" or \"de\", or \"auto\" to detect it.\n\
             language = \"auto\"\n\n\
             # Ask for a name when a recording stops.\n\
             ask_name = true\n\n\
             # Transcription engine (needs an NVIDIA GPU): \"auto\" (best working one), \"cuda12\" or \"cuda11\".\n\
             backend = \"auto\"\n",
            d.output_dir.display()
        );
        let _ = fs::create_dir_all(p.parent().unwrap());
        let _ = fs::write(&p, text);
    }
    p
}

/// Sets `language` in the settings file, keeping everything else (comments included) as it is.
pub fn set_language(code: &str) -> Result<(), String> {
    let p = ensure_file();
    let text = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    fs::write(&p, with_language(&text, code)).map_err(|e| format!("{}: {e}", p.display()))
}

/// `text` with its `language = ...` line replaced by `code` (or one added).
fn with_language(text: &str, code: &str) -> String {
    let line = format!("language = \"{code}\"");
    let is_language = |l: &&str| l.trim_start().strip_prefix("language").is_some_and(|r| r.trim_start().starts_with('='));
    let mut lines: Vec<String> = text.lines().map(|l| if is_language(&l) { line.clone() } else { l.to_string() }).collect();
    if !text.lines().any(|l| is_language(&l)) {
        lines.push(line);
    }
    lines.join("\n") + "\n"
}

/// Where downloads (model, runtimes), temporary audio and logs live: `%LOCALAPPDATA%\Whisperer`,
/// or `WHISPERER_DATA_DIR` if set.
pub fn data_dir() -> PathBuf {
    env::var_os("WHISPERER_DATA_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Whisperer"))
}

/// The user's Documents folder (follows OneDrive redirection).
fn documents() -> PathBuf {
    unsafe {
        match SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None) {
            Ok(p) => {
                let path = PathBuf::from(p.to_string().unwrap_or_default());
                CoTaskMemFree(Some(p.0 as _));
                path
            }
            Err(_) => PathBuf::from(env::var_os("USERPROFILE").unwrap_or_default()).join("Documents"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_or_adds_the_language_line() {
        let text = "# Spoken language\nlanguage = \"auto\"\nask_name = true\n";
        assert_eq!(with_language(text, "de"), "# Spoken language\nlanguage = \"de\"\nask_name = true\n");
        assert_eq!(with_language("ask_name = false\n", "en"), "ask_name = false\nlanguage = \"en\"\n");
        let parsed: Config = toml::from_str(&with_language(text, "de")).unwrap();
        assert_eq!(parsed.language, "de");
    }
}
