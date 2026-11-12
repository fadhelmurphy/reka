//! Command-line front end, handy for tuning the look without the desktop app.
//!
//!   reka-studio export <in.mp4> <events.json> <out.mp4> [settings.json]
//!   reka-studio frame  <in.mp4> <events.json> <t_ms> <out.png> [settings.json]

use reka_studio::{export, preview, InputEvent, Settings};
use std::path::{Path, PathBuf};

fn settings(p: Option<&String>) -> Settings {
    p.and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn events(p: &str) -> Vec<InputEvent> {
    serde_json::from_slice(&std::fs::read(p).expect("events file")).expect("events json")
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let ffmpeg = PathBuf::from(std::env::var("REKA_FFMPEG").unwrap_or_else(|_| "ffmpeg".into()));
    let res = match a.first().map(String::as_str) {
        Some("export") if a.len() >= 4 => export(
            &ffmpeg,
            Path::new(&a[1]),
            &events(&a[2]),
            &settings(a.get(4)),
            Path::new(&a[3]),
            |p| eprint!("\r{p}%"),
        ),
        Some("frame") if a.len() >= 5 => preview(&ffmpeg, Path::new(&a[1]), &events(&a[2]), &settings(a.get(5)), a[3].parse().unwrap_or(0), 1920)
            .and_then(|png| std::fs::write(&a[4], png).map_err(|e| e.to_string())),
        _ => Err("usage: reka-studio export <in> <events.json> <out> [settings] | frame <in> <events.json> <t_ms> <out.png> [settings]".into()),
    };
    if let Err(e) = res {
        eprintln!("\nerror: {e}");
        std::process::exit(1);
    }
    eprintln!();
}
