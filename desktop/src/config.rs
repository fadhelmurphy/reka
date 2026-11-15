//! Settings that survive restarts, and the folders Reka writes to.

use reka_studio::Settings;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// Remote Reka server for sharing with other people. `None` = embedded local server.
    pub server_url: Option<String>,
    /// Look of the Studio editor (including subtitle style) from the last session.
    pub look: Settings,
    /// "tiny" | "base" | "small" | "custom"
    pub whisper_model: String,
    pub whisper_custom: Option<String>,
    /// ISO code or "auto".
    pub language: String,
    pub studio: bool,
    pub auto_captions: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server_url: None,
            look: Settings::default(),
            whisper_model: "base".into(),
            whisper_custom: None,
            language: "auto".into(),
            studio: false,
            auto_captions: false,
        }
    }
}

pub struct Paths {
    pub data: PathBuf,
    pub recordings: PathBuf,
    pub models: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    pub fn new() -> std::io::Result<Self> {
        let data = std::env::var_os("REKA_DATA")
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|d| d.join("Reka")))
            .unwrap_or_else(|| std::env::temp_dir().join("Reka"));
        let p = Paths {
            recordings: data.join("recordings"),
            models: data.join("models"),
            config: data.join("config.json"),
            data,
        };
        std::fs::create_dir_all(&p.recordings)?;
        std::fs::create_dir_all(&p.models)?;
        Ok(p)
    }

    pub fn load(&self) -> Config {
        std::fs::read(&self.config).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, c: &Config) {
        if let Ok(b) = serde_json::to_vec_pretty(c) {
            let _ = std::fs::write(&self.config, b);
        }
    }

    /// Where a stock Whisper model lives (or will be downloaded to).
    pub fn model_file(&self, name: &str) -> PathBuf {
        self.models.join(format!("ggml-{name}.bin"))
    }
}

pub const MODELS: &[(&str, &str)] = &[
    ("tiny", "tiny — 75 MB, cepat, kurang akurat"),
    ("base", "base — 142 MB, seimbang"),
    ("small", "small — 466 MB, lebih akurat (disarankan untuk Bahasa Indonesia)"),
];

pub fn model_url(name: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin")
}

pub const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Deteksi otomatis"),
    ("id", "Indonesia"),
    ("en", "English"),
    ("ms", "Melayu"),
    ("jw", "Jawa"),
    ("su", "Sunda"),
    ("es", "Español"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("pt", "Português"),
    ("ja", "Japanese"),
    ("zh", "Chinese"),
    ("ar", "Arabic"),
];

/// Installed fonts as (label, path), for the subtitle font picker.
pub fn system_fonts() -> Vec<(String, PathBuf)> {
    let mut roots: Vec<PathBuf> = vec![];
    if cfg!(windows) {
        if let Some(w) = std::env::var_os("WINDIR") {
            roots.push(PathBuf::from(w).join("Fonts"));
        }
        if let Some(l) = dirs::data_local_dir() {
            roots.push(l.join("Microsoft/Windows/Fonts"));
        }
    } else if cfg!(target_os = "macos") {
        roots.extend(["/System/Library/Fonts", "/Library/Fonts"].map(PathBuf::from));
        if let Some(h) = dirs::home_dir() {
            roots.push(h.join("Library/Fonts"));
        }
    } else {
        roots.extend(["/usr/share/fonts", "/usr/local/share/fonts"].map(PathBuf::from));
        if let Some(h) = dirs::home_dir() {
            roots.push(h.join(".fonts"));
            roots.push(h.join(".local/share/fonts"));
        }
    }
    let mut out = vec![];
    fn walk(dir: &std::path::Path, depth: u32, out: &mut Vec<(String, PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && depth < 4 {
                walk(&p, depth + 1, out);
            } else if matches!(p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase).as_deref(), Some("ttf" | "otf")) {
                let label = p.file_stem().map(|s| s.to_string_lossy().replace(['-', '_'], " ")).unwrap_or_default();
                out.push((label, p));
            }
        }
    }
    for r in roots {
        walk(&r, 0, &mut out);
    }
    out.sort_by_key(|(l, _)| l.to_lowercase());
    out.dedup_by(|a, b| a.0 == b.0);
    out
}
