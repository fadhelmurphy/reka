use crate::{build_track, Compositor, Frame, InputEvent, Settings};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

pub const FPS: f32 = 30.0;

/// An ffmpeg `Command` that does not flash a console window on Windows.
pub fn command(ffmpeg: &Path) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    c
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub w: u32,
    pub h: u32,
    pub duration: f64,
    pub has_audio: bool,
}

/// Reads `ffmpeg -i file` stderr (bundled static builds ship without ffprobe).
pub fn parse_info(stderr: &str) -> Option<VideoInfo> {
    let dur = stderr.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("Duration: ")?;
        let mut it = rest.get(..11)?.split(':');
        let (h, m, s): (f64, f64, f64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
        Some(h * 3600.0 + m * 60.0 + s)
    })?;
    let video = stderr.lines().find(|l| l.contains("Video:"))?;
    // The codec tag looks like "0x31637661"; a real size has both sides ≥ 16.
    let (w, h) = video.split(|c: char| c == ' ' || c == ',' || c == '[').find_map(|t| {
        let (a, b) = t.split_once('x')?;
        let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
        (a >= 16 && b >= 16 && a <= 16384 && b <= 16384).then_some((a, b))
    })?;
    Some(VideoInfo { w, h, duration: dur, has_audio: stderr.lines().any(|l| l.contains("Audio:")) })
}

pub fn probe(ffmpeg: &Path, input: &Path) -> Result<VideoInfo, String> {
    let out = command(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(input)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg tidak bisa dijalankan ({}): {e}", ffmpeg.display()))?;
    parse_info(&String::from_utf8_lossy(&out.stderr)).ok_or_else(|| "tidak bisa membaca info video".into())
}

fn track_for(events: &[InputEvent], n: usize, s: &Settings) -> Vec<Frame> {
    let mut ev = events.to_vec();
    ev.sort_by_key(|e| e.t);
    build_track(&ev, n, FPS, s)
}

fn finish_child(mut c: Child, what: &str, log: &Path) -> Result<(), String> {
    let st = c.wait().map_err(|e| e.to_string())?;
    if st.success() {
        let _ = std::fs::remove_file(log);
        return Ok(());
    }
    let tail = std::fs::read_to_string(log).unwrap_or_default();
    Err(format!("{what} gagal ({st}): {}", tail.lines().rev().take(4).collect::<Vec<_>>().join(" | ")))
}

/// Render `input` + mouse `events` into a styled MP4. `progress` receives 0..=100.
pub fn export(
    ffmpeg: &Path,
    input: &Path,
    events: &[InputEvent],
    s: &Settings,
    output: &Path,
    mut progress: impl FnMut(u32),
) -> Result<(), String> {
    let info = probe(ffmpeg, input)?;
    let (w, h) = (info.w, info.h);
    let n = (info.duration * FPS as f64).ceil().max(1.0) as usize;
    let mut ev = events.to_vec();
    ev.sort_by_key(|e| e.t);
    let track = build_track(&ev, n, FPS, s);
    let mut comp = Compositor::new(w, h, s, &ev)?;

    let mut dec = command(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-vf", "fps=30", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let mut dec_out = dec.stdout.take().ok_or("no decoder stdout")?;

    let log = output.with_extension("enc.log");
    let log_file = std::fs::File::create(&log).map_err(|e| e.to_string())?;
    let mut enc_cmd = command(ffmpeg);
    enc_cmd
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba", "-s"])
        .arg(format!("{w}x{h}"))
        .args(["-r", "30", "-i", "-", "-i"])
        .arg(input)
        .args(["-map", "0:v"]);
    if info.has_audio {
        enc_cmd.args(["-map", "1:a:0", "-c:a", "copy"]);
    }
    enc_cmd
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-shortest"])
        .args(["-movflags", "+faststart"])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    let mut enc = enc_cmd.spawn().map_err(|e| format!("ffmpeg: {e}"))?;
    let mut enc_in = enc.stdin.take().ok_or("no encoder stdin")?;

    let mut src = Pixmap::new(w, h).ok_or("ukuran video tidak valid")?;
    let (mut i, mut last_pct) = (0usize, 0u32);
    let result: Result<(), String> = loop {
        match dec_out.read_exact(src.data_mut()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(e) => break Err(e.to_string()),
        }
        let f = track.get(i).or(track.last()).copied().unwrap_or_default();
        let ms = i as f32 * 1000.0 / FPS;
        if let Err(e) = enc_in.write_all(comp.render(&src, &f, ms).data()) {
            break Err(format!("encoder berhenti: {e}"));
        }
        i += 1;
        let pct = ((i * 100) / n).min(99) as u32;
        if pct != last_pct {
            last_pct = pct;
            progress(pct);
        }
    };
    drop(enc_in);
    if result.is_err() {
        let _ = dec.kill();
    }
    let _ = dec.wait();
    let enc_res = finish_child(enc, "encode", &log);
    result?;
    enc_res?;
    if i == 0 {
        return Err("tidak ada frame yang terbaca dari video".into());
    }
    progress(100);
    Ok(())
}

/// One composed frame at `t_ms` as PNG, scaled to at most `max_w` pixels wide.
pub fn preview(
    ffmpeg: &Path,
    input: &Path,
    events: &[InputEvent],
    s: &Settings,
    t_ms: u64,
    max_w: u32,
) -> Result<Vec<u8>, String> {
    let info = probe(ffmpeg, input)?;
    let (w, h) = (info.w, info.h);
    let t = (t_ms as f64 / 1000.0).min((info.duration - 0.1).max(0.0));
    let idx = (t * FPS as f64).round() as usize;

    let grab = |ss: f64| -> Result<Vec<u8>, String> {
        let out = command(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-ss", &format!("{ss:.3}"), "-i"])
            .arg(input)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        Ok(out.stdout)
    };
    let mut raw = grab(t)?;
    if raw.len() < (w * h * 4) as usize {
        raw = grab((t - 0.3).max(0.0))?;
    }
    if raw.len() < (w * h * 4) as usize {
        return Err("tidak bisa membaca frame".into());
    }
    let mut src = Pixmap::new(w, h).ok_or("ukuran video tidak valid")?;
    src.data_mut().copy_from_slice(&raw[..(w * h * 4) as usize]);

    let track = track_for(events, idx + 1, s);
    let mut ev = events.to_vec();
    ev.sort_by_key(|e| e.t);
    let mut comp = Compositor::new(w, h, s, &ev)?;
    let f = track.last().copied().unwrap_or_default();
    let full = comp.render(&src, &f, idx as f32 * 1000.0 / FPS);

    if w <= max_w {
        return full.encode_png().map_err(|e| e.to_string());
    }
    let mh = (h as f32 * max_w as f32 / w as f32).round().max(1.0) as u32;
    let mut small = Pixmap::new(max_w, mh).ok_or("ukuran preview tidak valid")?;
    let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() };
    small.draw_pixmap(0, 0, full.as_ref(), &paint, Transform::from_scale(max_w as f32 / w as f32, mh as f32 / h as f32), None);
    small.encode_png().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_banner() {
        let out = "Input #0, mov,mp4, from 'a.mp4':\n  Duration: 00:01:04.47, start: 0.000000, bitrate: 1200 kb/s\n  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 1440x900 [SAR 1:1 DAR 8:5], 1100 kb/s, 30 fps\n  Stream #0:1[0x2](und): Audio: aac (LC), 48000 Hz, stereo";
        assert_eq!(parse_info(out), Some(VideoInfo { w: 1440, h: 900, duration: 64.47, has_audio: true }));
        let no_audio = out.lines().take(3).collect::<Vec<_>>().join("\n");
        assert!(!parse_info(&no_audio).unwrap().has_audio);
    }
}
