//! Global mouse log for Studio mode. Only pointer position and button presses are kept:
//! keystrokes are deliberately never recorded.

use crate::recorder::{Screen, Span};
use reka_studio::{InputEvent, Kind};
use rdev::{Button, EventType};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Raw {
    pub at: Instant,
    pub kind: Kind,
    pub x: f64,
    pub y: f64,
}

static SINK: Mutex<Option<Vec<Raw>>> = Mutex::new(None);
static LAST: Mutex<Option<(f64, f64)>> = Mutex::new(None);
static FAILED: Mutex<Option<String>> = Mutex::new(None);
static START: Once = Once::new();

fn is_real_button(b: &Button) -> bool {
    matches!(b, Button::Left | Button::Right | Button::Middle)
}

fn on_event(ev: rdev::Event) {
    let (kind, pos) = match ev.event_type {
        EventType::MouseMove { x, y } => {
            *LAST.lock().unwrap() = Some((x, y));
            (Kind::Move, Some((x, y)))
        }
        EventType::ButtonPress(b) if is_real_button(&b) => (Kind::Down, *LAST.lock().unwrap()),
        EventType::ButtonRelease(b) if is_real_button(&b) => (Kind::Up, *LAST.lock().unwrap()),
        _ => return,
    };
    let Some((x, y)) = pos else { return };
    if let Some(sink) = SINK.lock().unwrap().as_mut() {
        sink.push(Raw { at: Instant::now(), kind, x, y });
    }
}

fn hint(e: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("{e}. Izinkan Reka di System Settings → Privacy & Security → Accessibility dan Input Monitoring, lalu buka ulang aplikasi.")
    } else if cfg!(target_os = "linux") {
        format!("{e}. Mode Studio butuh sesi X11 (bukan Wayland murni).")
    } else {
        e.to_string()
    }
}

/// Start logging. The listener thread is created once and lives as long as the process
/// (rdev cannot stop a listener); events are only stored between `begin` and `end`.
pub fn begin(initial: Option<(f64, f64)>) -> Result<(), String> {
    START.call_once(|| {
        std::thread::spawn(|| {
            if let Err(e) = rdev::listen(on_event) {
                *FAILED.lock().unwrap() = Some(format!("{e:?}"));
            }
        });
        // listen() blocks forever when it works; if it cannot start it returns almost at once.
        std::thread::sleep(Duration::from_millis(400));
    });
    if let Some(e) = FAILED.lock().unwrap().clone() {
        return Err(hint(&e));
    }
    let start = initial.or(*LAST.lock().unwrap());
    let mut sink = Vec::new();
    if let Some((x, y)) = start {
        *LAST.lock().unwrap() = Some((x, y));
        sink.push(Raw { at: Instant::now(), kind: Kind::Move, x, y });
    }
    *SINK.lock().unwrap() = Some(sink);
    Ok(())
}

pub fn end() -> Vec<Raw> {
    SINK.lock().unwrap().take().unwrap_or_default()
}

/// Wall-clock events → video-timeline events normalised to 0..1 of `screen`.
/// Anything that happened during a pause is dropped; mouse moves are thinned to ~60 Hz.
pub fn map(raw: &[Raw], spans: &[Span], screen: &Screen) -> Vec<InputEvent> {
    if screen.w == 0 || screen.h == 0 {
        return vec![];
    }
    let norm = |r: &Raw| {
        (
            ((r.x - screen.x as f64) / screen.w as f64).clamp(0.0, 1.0) as f32,
            ((r.y - screen.y as f64) / screen.h as f64).clamp(0.0, 1.0) as f32,
        )
    };
    let mut out: Vec<InputEvent> = vec![];
    let mut offset = 0.0f64; // ms of video before this span
    let mut last_move = f64::NEG_INFINITY;
    for sp in spans {
        // Where the pointer already was when this stretch began (start, or after a pause).
        if let Some(r) = raw.iter().rev().find(|r| r.at < sp.start) {
            let (x, y) = norm(r);
            out.push(InputEvent { t: offset as u32, k: Kind::Move, x, y });
            last_move = offset;
        }
        for r in raw.iter().filter(|r| r.at >= sp.start && r.at < sp.end) {
            let t = offset + r.at.duration_since(sp.start).as_secs_f64() * 1000.0;
            if r.kind == Kind::Move && t - last_move < 16.0 {
                continue;
            }
            if r.kind == Kind::Move {
                last_move = t;
            }
            let (x, y) = norm(r);
            out.push(InputEvent { t: t as u32, k: r.kind, x, y });
        }
        offset += sp.end.saturating_duration_since(sp.start).as_secs_f64() * 1000.0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_wall_clock_to_video_time_and_skips_pauses() {
        let b = Instant::now();
        let ms = |n: u64| b + Duration::from_millis(n);
        let raw = |at, kind, x| Raw { at, kind, x, y: 100.0 };
        let raws = [
            raw(ms(0), Kind::Move, 200.0),     // before recording: becomes the starting position
            raw(ms(600), Kind::Move, 500.0),   // 500 ms into span 1
            raw(ms(700), Kind::Down, 500.0),
            raw(ms(3000), Kind::Move, 800.0),  // during the pause: dropped, but seeds span 2
            raw(ms(5500), Kind::Move, 900.0),  // 500 ms into span 2
        ];
        let spans = [Span { start: ms(100), end: ms(1100) }, Span { start: ms(5000), end: ms(6000) }];
        let screen = Screen { label: "t".into(), mac_index: None, x: 0, y: 0, w: 1000, h: 500 };
        let ev = map(&raws, &spans, &screen);
        let got: Vec<(u32, Kind, f32)> = ev.iter().map(|e| (e.t, e.k, e.x)).collect();
        assert_eq!(
            got,
            vec![
                (0, Kind::Move, 0.2),
                (500, Kind::Move, 0.5),
                (600, Kind::Down, 0.5),
                (1000, Kind::Move, 0.8), // span 2 starts at video time 1000 ms
                (1500, Kind::Move, 0.9),
            ]
        );
        assert!((ev[0].y - 0.2).abs() < 1e-6);
    }

    #[test]
    fn thins_dense_moves_but_keeps_every_click() {
        let b = Instant::now();
        let mut raws: Vec<Raw> = (0..200)
            .map(|i| Raw { at: b + Duration::from_millis(i), kind: Kind::Move, x: i as f64, y: 0.0 })
            .collect();
        raws.push(Raw { at: b + Duration::from_millis(100), kind: Kind::Down, x: 100.0, y: 0.0 });
        raws.sort_by_key(|r| r.at);
        let spans = [Span { start: b, end: b + Duration::from_millis(300) }];
        let screen = Screen { label: "t".into(), mac_index: None, x: 0, y: 0, w: 1000, h: 500 };
        let ev = map(&raws, &spans, &screen);
        assert!(ev.len() < 40, "thinned: {}", ev.len());
        assert_eq!(ev.iter().filter(|e| e.k == Kind::Down).count(), 1);
    }

    #[test]
    fn unknown_screen_size_yields_nothing() {
        let screen = Screen { label: "t".into(), mac_index: Some(1), x: 0, y: 0, w: 0, h: 0 };
        assert!(map(&[], &[], &screen).is_empty());
    }
}

/// Real capture on an X server: `xvfb-run -s "-screen 0 1280x800x24" cargo test -p reka-desktop -- --ignored --nocapture`
#[cfg(all(test, target_os = "linux"))]
mod e2e {
    use super::*;
    use crate::recorder::{RecOpts, Session};
    use std::process::Command;

    fn xd(args: &[&str]) {
        assert!(Command::new("xdotool").args(args).status().unwrap().success(), "xdotool {args:?}");
    }
    fn sleep(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
    /// Time (ms) of the first frame whose average colour is red.
    fn first_red_ms(p: &std::path::Path) -> Option<f64> {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(p)
            .args(["-vf", "scale=1:1,fps=30", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
            .unwrap()
            .stdout;
        out.chunks_exact(3).position(|px| px[0] > 150 && px[1] < 90 && px[2] < 90).map(|i| i as f64 * 1000.0 / 30.0)
    }

    #[test]
    #[ignore]
    fn studio_record_roundtrip() {
        begin(None).expect("mouse log (needs X11 + XRecord)");
        xd(&["mousemove", "100", "100"]);
        assert!(Command::new("xsetroot").args(["-solid", "#101010"]).status().unwrap().success());

        let dir = std::env::temp_dir().join("reka-studio-e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let screen = Screen { label: "t".into(), mac_index: None, x: 0, y: 0, w: 1280, h: 800 };
        let s = Session::start(RecOpts { screen: screen.clone(), mic: None, hide_cursor: true }, dir.join("s")).unwrap();

        sleep(300);
        xd(&["mousemove", "200", "200"]);
        sleep(400);
        xd(&["mousemove", "640", "400"]);
        sleep(300);
        xd(&["click", "1"]);
        // The screen turns red right after the click: the click time in the log must line up
        // with this change in the video.
        assert!(Command::new("xsetroot").args(["-solid", "red"]).status().unwrap().success());
        sleep(1500);
        xd(&["mousemove", "1000", "600"]);
        sleep(400);
        xd(&["click", "1"]);
        sleep(1200);

        let raw = end();
        let raw_mp4 = dir.join("raw.mp4");
        let (_, dur, spans) = s.finish(&raw_mp4).unwrap();
        let ev = map(&raw, &spans, &screen);
        println!("recorded {dur:?}, {} raw / {} mapped events", raw.len(), ev.len());

        let downs: Vec<_> = ev.iter().filter(|e| e.k == Kind::Down).collect();
        assert_eq!(downs.len(), 2, "two clicks logged: {downs:?}");
        assert!((downs[0].x - 0.5).abs() < 0.01 && (downs[0].y - 0.5).abs() < 0.01, "{:?}", downs[0]);
        assert!((downs[1].x - 1000.0 / 1280.0).abs() < 0.01 && (downs[1].y - 0.75).abs() < 0.01, "{:?}", downs[1]);

        let red = first_red_ms(&raw_mp4).expect("screen turned red in the video");
        let skew = red - downs[0].t as f64;
        println!("click logged at {} ms, screen turned red at {red:.0} ms  → video is {skew:+.0} ms after the log", downs[0].t);
        assert!(skew.abs() < 100.0, "log and video are out of sync by {skew} ms");

        // And the whole pipeline: export with the real log.
        let events_json = dir.join("raw.events.json");
        std::fs::write(&events_json, serde_json::to_vec(&ev).unwrap()).unwrap();
        let out = dir.join("studio.mp4");
        reka_studio::export(&crate::recorder::ffmpeg_bin(), &raw_mp4, &ev, &[], &reka_studio::Settings::default(), None, &out, |_| {}).unwrap();
        assert!(std::fs::metadata(&out).unwrap().len() > 1000);
    }
}
