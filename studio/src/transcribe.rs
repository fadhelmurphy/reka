//! Speech-to-text for automatic subtitles.
//!
//! Audio is pulled out with ffmpeg as 16 kHz mono floats; recognition runs in-process on
//! whisper.cpp (cargo feature `whisper`). Builds without the feature still compile and report
//! a clear error, so the app can be developed without cmake.

use crate::captions::{is_noise, split_caption, Caption};
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

/// Whether this build can transcribe.
pub const AVAILABLE: bool = cfg!(feature = "whisper");

/// Longest caption line, in characters, before a segment is split.
pub const MAX_CHARS: usize = 42;

/// Decode the audio track to 16 kHz mono f32 samples (what Whisper expects).
pub fn extract_audio(ffmpeg: &Path, video: &Path) -> Result<Vec<f32>, String> {
    let mut child = crate::command(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(video)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "f32le", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg tidak bisa dijalankan: {e}"))?;
    let mut bytes = Vec::new();
    child.stdout.take().ok_or("no stdout")?.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("gagal mengambil audio: {}", err.lines().last().unwrap_or("")));
    }
    if bytes.len() < 4 * 16000 / 2 {
        return Err("video ini tidak punya suara (rekam dengan mikrofon aktif)".into());
    }
    Ok(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// Turn raw recognised segments `(start_ms, end_ms, text)` into display captions.
pub fn segments_to_captions(segs: impl IntoIterator<Item = (u32, u32, String)>) -> Vec<Caption> {
    let mut out: Vec<Caption> = vec![];
    for (s, e, text) in segs {
        let text = text.trim();
        if is_noise(text) {
            continue;
        }
        out.extend(split_caption(text, s, e.max(s + 400), MAX_CHARS));
    }
    // Never let two captions overlap on screen.
    for i in 1..out.len() {
        if out[i - 1].end > out[i].start {
            out[i - 1].end = out[i].start;
        }
    }
    out.retain(|c| c.end > c.start);
    out
}

/// Recognise `audio`. `lang` is an ISO code such as "id" or "en"; `None` auto-detects.
#[cfg(feature = "whisper")]
pub fn transcribe(
    model: &Path,
    audio: &[f32],
    lang: Option<&str>,
    progress: impl FnMut(i32) + 'static,
) -> Result<Vec<Caption>, String> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    let ctx = WhisperContext::new_with_params(model, WhisperContextParameters::default())
        .map_err(|e| format!("model whisper tidak bisa dimuat ({}): {e}", model.display()))?;
    let mut state = ctx.create_state().map_err(|e| e.to_string())?;
    let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    p.set_language(lang.filter(|l| *l != "auto"));
    p.set_translate(false);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_special(false);
    p.set_print_timestamps(false);
    p.set_n_threads(std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(4).min(8));
    p.set_progress_callback_safe(progress);
    state.full(p, audio).map_err(|e| format!("transkripsi gagal: {e}"))?;

    let n = state.full_n_segments();
    let mut segs = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let Some(seg) = state.get_segment(i) else { continue };
        let text = seg.to_str_lossy().map_err(|e| e.to_string())?.into_owned();
        // Whisper timestamps are in centiseconds.
        segs.push((seg.start_timestamp().max(0) as u32 * 10, seg.end_timestamp().max(0) as u32 * 10, text));
    }
    Ok(segments_to_captions(segs))
}

#[cfg(not(feature = "whisper"))]
pub fn transcribe(
    _model: &Path,
    _audio: &[f32],
    _lang: Option<&str>,
    _progress: impl FnMut(i32) + 'static,
) -> Result<Vec<Caption>, String> {
    Err("build ini tanpa whisper. Bangun ulang dengan: cargo build --release --features whisper".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_become_clean_non_overlapping_captions() {
        let caps = segments_to_captions(vec![
            (0, 1200, "[BLANK_AUDIO]".to_string()),
            (1000, 5000, " Selamat datang di video ini, kali ini kita akan membahas cara memakai aplikasinya.".to_string()),
            (4800, 7000, "Mari kita mulai.".to_string()),
        ]);
        assert!(caps.iter().all(|c| c.text.chars().count() <= MAX_CHARS));
        assert!(caps.windows(2).all(|w| w[0].end <= w[1].start));
        assert_eq!(caps.first().unwrap().start, 1000);
        assert_eq!(caps.last().unwrap().text, "Mari kita mulai.");
        assert!(!caps.iter().any(|c| c.text.contains("BLANK")));
    }

    #[test]
    fn audio_is_extracted_from_a_real_file() {
        let ff = std::path::PathBuf::from("ffmpeg");
        if crate::command(&ff).arg("-version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("reka-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.mp4");
        let ok = crate::command(&ff)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-f", "lavfi", "-i", "color=c=black:s=64x64:d=2", "-shortest", "-c:v", "libx264", "-c:a", "aac"])
            .arg(&wav)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        let a = extract_audio(&ff, &wav).unwrap();
        assert!((a.len() as i64 - 32000).abs() < 2000, "got {} samples", a.len());
        assert!(a.iter().any(|v| v.abs() > 0.1));
        let _ = std::fs::remove_dir_all(dir);
    }
}
