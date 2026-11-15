//! Screens and microphones, in the form each OS's ffmpeg input expects.

use crate::recorder::{ffmpeg_cmd, Screen};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Mic {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Camera {
    /// v4l2 path, dshow device name or avfoundation index.
    pub id: String,
    pub label: String,
}

#[derive(Default, Clone, Serialize)]
pub struct Devices {
    pub screens: Vec<Screen>,
    pub mics: Vec<Mic>,
    pub cameras: Vec<Camera>,
    pub warning: Option<String>,
}

pub fn list() -> Devices {
    let mut warning = None;
    let probe = ffmpeg_cmd().arg("-version").output();
    if probe.map(|o| !o.status.success()).unwrap_or(true) {
        warning = Some(
            "ffmpeg tidak ditemukan. Instal ffmpeg (brew/winget/apt) atau taruh di samping aplikasi.".into(),
        );
    }
    #[cfg(target_os = "linux")]
    if std::env::var("XDG_SESSION_TYPE").map(|v| v == "wayland").unwrap_or(false) {
        warning.get_or_insert_with(|| {
            "Sesi Wayland terdeteksi: x11grab hanya merekam aplikasi XWayland. Login dengan sesi X11 untuk hasil penuh.".into()
        });
    }

    #[cfg(target_os = "macos")]
    {
        let out = ffmpeg_stderr(&["-f", "avfoundation", "-list_devices", "true", "-i", ""]);
        let (mut screens, mics) = parse_avfoundation(&out);
        let cameras = parse_avfoundation_cameras(&out);
        // Studio mode needs each screen's rectangle in points (what the mouse hook reports);
        // avfoundation lists screens in the same order as the OS display list.
        let infos = display_info::DisplayInfo::all().unwrap_or_default();
        for (i, sc) in screens.iter_mut().enumerate() {
            if let Some(d) = infos.get(i) {
                sc.x = d.x;
                sc.y = d.y;
                sc.w = d.width;
                sc.h = d.height;
            }
        }
        return Devices { screens, mics, cameras, warning };
    }
    #[cfg(not(target_os = "macos"))]
    {
        let screens = monitors();
        #[cfg(target_os = "windows")]
        let (mics, cameras) = {
            let out = ffmpeg_stderr(&["-list_devices", "true", "-f", "dshow", "-i", "dummy"]);
            (parse_dshow(&out), parse_dshow_cameras(&out))
        };
        #[cfg(not(target_os = "windows"))]
        let (mics, cameras) = (pulse_sources(), v4l2_cameras());
        Devices { screens, mics, cameras, warning }
    }
}

#[allow(dead_code)]
fn ffmpeg_stderr(args: &[&str]) -> String {
    ffmpeg_cmd()
        .arg("-hide_banner")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default()
}

/// Screens in the pixel space the capture device uses (physical pixels; points on macOS).
#[cfg(not(target_os = "macos"))]
fn monitors() -> Vec<Screen> {
    let mut v: Vec<Screen> = display_info::DisplayInfo::all()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, d)| {
            // display-info reports Linux/X11 geometry divided by the scale factor; x11grab wants pixels.
            let f = if cfg!(target_os = "linux") { d.scale_factor.max(1.0) } else { 1.0 };
            let (x, y) = ((d.x as f32 * f).round() as i32, (d.y as f32 * f).round() as i32);
            let (w, h) = ((d.width as f32 * f).round() as u32, (d.height as f32 * f).round() as u32);
            let name = if d.friendly_name.is_empty() { format!("Layar {}", i + 1) } else { d.friendly_name.clone() };
            let star = if d.is_primary { " (utama)" } else { "" };
            Screen { label: format!("{name}{star} — {w}×{h}"), mac_index: None, x, y, w, h }
        })
        .collect();
    // Primary first so it is the default choice.
    v.sort_by_key(|s| !s.label.contains("(utama)"));
    v
}

/// Capture nodes of /dev/video* (the odd nodes are usually metadata streams).
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn v4l2_cameras() -> Vec<Camera> {
    let mut v = vec![];
    for e in std::fs::read_dir("/sys/class/video4linux").into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.starts_with("video") {
            continue;
        }
        let index = std::fs::read_to_string(e.path().join("index")).unwrap_or_default();
        if index.trim() != "0" && !index.trim().is_empty() {
            continue;
        }
        let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_else(|_| n.clone()).trim().to_string();
        v.push(Camera { id: format!("/dev/{n}"), label: format!("{name} ({n})") });
    }
    v.sort_by(|a, b| a.id.cmp(&b.id));
    if std::env::var_os("REKA_FAKE_CAMERA").is_some() {
        v.push(Camera { id: "fake".into(), label: "Kamera uji (testsrc)".into() });
    }
    v
}

/// Video inputs of the avfoundation list (screens excluded).
#[allow(dead_code)]
pub fn parse_avfoundation_cameras(out: &str) -> Vec<Camera> {
    let mut v = vec![];
    let mut video = false;
    for line in out.lines() {
        if line.contains("AVFoundation video devices") {
            video = true;
            continue;
        }
        if line.contains("AVFoundation audio devices") {
            video = false;
        }
        if !video {
            continue;
        }
        let Some(p) = line.find("] [") else { continue };
        let rest = &line[p + 3..];
        let Some(end) = rest.find(']') else { continue };
        let Ok(idx) = rest[..end].parse::<u32>() else { continue };
        let name = rest[end + 1..].trim().to_string();
        if !name.starts_with("Capture screen") {
            v.push(Camera { id: idx.to_string(), label: name });
        }
    }
    v
}

/// DirectShow video devices (`"Name" (video)` or the older section layout).
#[allow(dead_code)]
pub fn parse_dshow_cameras(out: &str) -> Vec<Camera> {
    let mut v: Vec<Camera> = vec![];
    let mut in_video = false;
    for line in out.lines() {
        if line.contains("DirectShow video devices") {
            in_video = true;
            continue;
        }
        if line.contains("DirectShow audio devices") {
            in_video = false;
            continue;
        }
        if line.contains("Alternative name") {
            continue;
        }
        let (Some(a), true) = (line.find('"'), line.contains("(video)") || in_video) else { continue };
        let Some(b) = line[a + 1..].find('"') else { continue };
        let name = line[a + 1..a + 1 + b].to_string();
        if !v.iter().any(|c| c.id == name) {
            v.push(Camera { id: name.clone(), label: name });
        }
    }
    v
}

/// Lines look like `[AVFoundation indev @ 0x7f..] [1] Capture screen 0`.
#[allow(dead_code)]
pub fn parse_avfoundation(out: &str) -> (Vec<Screen>, Vec<Mic>) {
    let (mut screens, mut mics) = (vec![], vec![]);
    let mut section = "";
    for line in out.lines() {
        if line.contains("AVFoundation video devices") {
            section = "video";
            continue;
        }
        if line.contains("AVFoundation audio devices") {
            section = "audio";
            continue;
        }
        let Some(p) = line.find("] [") else { continue };
        let rest = &line[p + 3..];
        let Some(end) = rest.find(']') else { continue };
        let Ok(idx) = rest[..end].parse::<u32>() else { continue };
        let name = rest[end + 1..].trim().to_string();
        match section {
            "video" if name.starts_with("Capture screen") => {
                let n = name.trim_start_matches("Capture screen").trim().parse::<u32>().unwrap_or(0);
                screens.push(Screen { label: format!("Layar {}", n + 1), mac_index: Some(idx), x: 0, y: 0, w: 0, h: 0 });
            }
            "audio" => mics.push(Mic { id: idx.to_string(), label: name }),
            _ => {}
        }
    }
    (screens, mics)
}

/// Handles both ffmpeg ≥4.4 (`"Name" (audio)`) and older section-based output.
#[allow(dead_code)]
pub fn parse_dshow(out: &str) -> Vec<Mic> {
    let mut mics = vec![];
    let mut in_audio = false;
    for line in out.lines() {
        if line.contains("DirectShow audio devices") {
            in_audio = true;
            continue;
        }
        if line.contains("DirectShow video devices") {
            in_audio = false;
            continue;
        }
        if line.contains("Alternative name") {
            continue;
        }
        let quoted = || {
            let a = line.find('"')?;
            let b = line[a + 1..].find('"')? + a + 1;
            Some(line[a + 1..b].to_string())
        };
        if line.contains("(audio)") || (in_audio && !line.contains("(video)")) {
            if let Some(name) = quoted() {
                if !mics.iter().any(|m: &Mic| m.id == name) {
                    mics.push(Mic { id: name.clone(), label: name });
                }
            }
        }
    }
    mics
}

/// PulseAudio/PipeWire sources. `.monitor` sources capture what the speakers play.
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn pulse_sources() -> Vec<Mic> {
    let out = std::process::Command::new("pactl").args(["list", "short", "sources"]).output();
    // No PulseAudio/PipeWire (or no pactl): offer no audio rather than a device ffmpeg cannot open.
    parse_pactl(&out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default())
}

#[allow(dead_code)]
pub fn parse_pactl(out: &str) -> Vec<Mic> {
    let mut v: Vec<Mic> = out
        .lines()
        .filter_map(|l| l.split('\t').nth(1).or_else(|| l.split_whitespace().nth(1)))
        .map(|name| {
            let label = if name.ends_with(".monitor") {
                format!("Audio sistem — {}", name.trim_end_matches(".monitor"))
            } else {
                name.to_string()
            };
            Mic { id: name.to_string(), label }
        })
        .collect();
    v.sort_by_key(|m| m.id.ends_with(".monitor")); // real mics first
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avfoundation() {
        let out = r#"[AVFoundation indev @ 0x7f9] AVFoundation video devices:
[AVFoundation indev @ 0x7f9] [0] FaceTime HD Camera
[AVFoundation indev @ 0x7f9] [1] Capture screen 0
[AVFoundation indev @ 0x7f9] [2] Capture screen 1
[AVFoundation indev @ 0x7f9] AVFoundation audio devices:
[AVFoundation indev @ 0x7f9] [0] MacBook Pro Microphone
[AVFoundation indev @ 0x7f9] [1] BlackHole 2ch
: Input/output error"#;
        let (s, m) = parse_avfoundation(out);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].mac_index, Some(1));
        assert_eq!(s[1].label, "Layar 2");
        assert_eq!(m, vec![
            Mic { id: "0".into(), label: "MacBook Pro Microphone".into() },
            Mic { id: "1".into(), label: "BlackHole 2ch".into() },
        ]);
    }

    #[test]
    fn dshow_new_and_old() {
        let new = r#"[dshow @ 000001] "Integrated Camera" (video)
[dshow @ 000001]   Alternative name "@device_pnp_\\?\usb#vid"
[dshow @ 000001] "Microphone Array (Realtek(R) Audio)" (audio)
[dshow @ 000001]   Alternative name "@device_cm_{33D9A762}\wave_{ABC}""#;
        assert_eq!(parse_dshow(new), vec![Mic {
            id: "Microphone Array (Realtek(R) Audio)".into(),
            label: "Microphone Array (Realtek(R) Audio)".into()
        }]);
        let old = r#"[dshow @ 01] DirectShow video devices (some may be both video and audio devices)
[dshow @ 01]  "Integrated Camera"
[dshow @ 01] DirectShow audio devices
[dshow @ 01]  "Mikrofon (USB Audio)"
[dshow @ 01]     Alternative name "@device_cm_x""#;
        assert_eq!(parse_dshow(old).len(), 1);
        assert_eq!(parse_dshow(old)[0].id, "Mikrofon (USB Audio)");
        assert_eq!(parse_dshow_cameras(new), vec![Camera { id: "Integrated Camera".into(), label: "Integrated Camera".into() }]);
        assert_eq!(parse_dshow_cameras(old)[0].id, "Integrated Camera");
    }

    #[test]
    fn avfoundation_cameras_skip_screens() {
        let out = "[AVFoundation indev @ 0x7f9] AVFoundation video devices:\n[AVFoundation indev @ 0x7f9] [0] FaceTime HD Camera\n[AVFoundation indev @ 0x7f9] [1] Capture screen 0\n[AVFoundation indev @ 0x7f9] AVFoundation audio devices:\n[AVFoundation indev @ 0x7f9] [0] Mic";
        assert_eq!(parse_avfoundation_cameras(out), vec![Camera { id: "0".into(), label: "FaceTime HD Camera".into() }]);
    }

    #[test]
    fn pactl() {
        let out = "48\talsa_output.pci.analog-stereo.monitor\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n49\talsa_input.pci.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n";
        let v = parse_pactl(out);
        assert_eq!(v[0].id, "alsa_input.pci.analog-stereo");
        assert!(v[1].label.starts_with("Audio sistem"));
    }
}
