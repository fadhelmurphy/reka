//! Virtual camera + cursor motion, computed from the logged mouse events.

use crate::{InputEvent, Kind, Settings};

#[derive(Debug, Clone, Copy, Default)]
pub struct Frame {
    /// Camera centre (0..1) and zoom (≥1).
    pub cx: f32,
    pub cy: f32,
    pub z: f32,
    /// Smoothed cursor position (0..1).
    pub mx: f32,
    pub my: f32,
    /// 0..1 — how "pressed" the cursor looks.
    pub press: f32,
    pub has_cursor: bool,
}

/// Start zooming this long before a click, so the camera is already there when it lands.
const LEAD: f32 = 0.5;
/// Stay zoomed this long after the last click.
const HOLD: f32 = 1.6;
/// Clicks closer than this (after HOLD) share one zoom instead of zooming out and in again.
const MERGE_GAP: f32 = 0.9;
const ZOOM_SMOOTH: f32 = 0.35;
const CENTER_SMOOTH: f32 = 0.45;
const CURSOR_SMOOTH: f32 = 0.05;
/// The camera looks slightly ahead of the cursor.
const LOOKAHEAD: f32 = 0.25;
/// While zoomed, the cursor may roam this fraction of the half-viewport before the camera follows.
const DEAD_ZONE: f32 = 0.45;

/// Critically damped spring ("SmoothDamp"): no overshoot, continuous velocity.
struct Damp {
    v: f32,
    vel: f32,
}

impl Damp {
    fn new(v: f32) -> Self {
        Damp { v, vel: 0.0 }
    }
    fn step(&mut self, target: f32, smooth: f32, dt: f32) -> f32 {
        let omega = 2.0 / smooth.max(1e-4);
        let x = omega * dt;
        let e = 1.0 / (1.0 + x + 0.48 * x * x + 0.235 * x * x * x);
        let change = self.v - target;
        let temp = (self.vel + omega * change) * dt;
        self.vel = (self.vel - omega * temp) * e;
        self.v = target + (change + temp) * e;
        self.v
    }
}

fn pos_at(ev: &[InputEvent], ms: f32) -> Option<(f32, f32)> {
    let i = ev.partition_point(|e| (e.t as f32) <= ms);
    if i == 0 {
        ev.first().map(|e| (e.x, e.y))
    } else {
        Some((ev[i - 1].x, ev[i - 1].y))
    }
}

/// Zoom windows in seconds.
pub fn zoom_segments(ev: &[InputEvent]) -> Vec<(f32, f32)> {
    let mut segs: Vec<(f32, f32)> = vec![];
    for e in ev.iter().filter(|e| e.k == Kind::Down) {
        let t = e.t as f32 / 1000.0;
        let (start, end) = (t - LEAD, t + HOLD);
        if let Some(last) = segs.last_mut() {
            if start <= last.1 + MERGE_GAP {
                last.1 = last.1.max(end);
                continue;
            }
        }
        segs.push((start, end));
    }
    segs
}

/// One [`Frame`] per output frame. `events` must be sorted by time.
pub fn build(ev: &[InputEvent], n: usize, fps: f32, s: &Settings) -> Vec<Frame> {
    let dt = 1.0 / fps;
    let has = !ev.is_empty();
    let segs = if s.zoom { zoom_segments(ev) } else { vec![] };
    let zl = s.zoom_level.clamp(1.1, 4.0);

    let (mut z, mut cx, mut cy) = (Damp::new(1.0), Damp::new(0.5), Damp::new(0.5));
    let first = pos_at(ev, 0.0).unwrap_or((0.5, 0.5));
    let (mut mx, mut my) = (Damp::new(first.0), Damp::new(first.1));
    let (mut tcx, mut tcy) = (0.5f32, 0.5f32);
    let mut was_active = false;
    let (mut ei, mut pressed, mut press) = (0usize, false, 0.0f32);
    let mut out = Vec::with_capacity(n);

    for i in 0..n {
        let t = i as f32 * dt;
        let ms = t * 1000.0;
        let cur = pos_at(ev, ms).unwrap_or((0.5, 0.5));
        let ahead = pos_at(ev, ms + LOOKAHEAD * 1000.0).unwrap_or(cur);
        let active = segs.iter().any(|&(a, b)| t >= a && t <= b);

        if active {
            if !was_active {
                tcx = ahead.0;
                tcy = ahead.1;
            } else {
                let m = 0.5 / zl * DEAD_ZONE;
                tcx = tcx.clamp(ahead.0 - m, ahead.0 + m);
                tcy = tcy.clamp(ahead.1 - m, ahead.1 + m);
            }
        } else {
            tcx = 0.5;
            tcy = 0.5;
        }
        was_active = active;

        let zv = z.step(if active { zl } else { 1.0 }, ZOOM_SMOOTH, dt).max(1.0);
        let half = 0.5 / zv;
        let cxv = cx.step(tcx, CENTER_SMOOTH, dt).clamp(half, 1.0 - half);
        let cyv = cy.step(tcy, CENTER_SMOOTH, dt).clamp(half, 1.0 - half);

        while ei < ev.len() && ev[ei].t as f32 <= ms {
            match ev[ei].k {
                Kind::Down => pressed = true,
                Kind::Up => pressed = false,
                Kind::Move => {}
            }
            ei += 1;
        }
        press += ((pressed as u8 as f32) - press) * (1.0 - (-dt / 0.05).exp());

        out.push(Frame {
            cx: cxv,
            cy: cyv,
            z: zv,
            mx: mx.step(cur.0, CURSOR_SMOOTH, dt),
            my: my.step(cur.1, CURSOR_SMOOTH, dt),
            press,
            has_cursor: s.cursor && has,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: u32, k: Kind, x: f32, y: f32) -> InputEvent {
        InputEvent { t, k, x, y }
    }

    #[test]
    fn zooms_to_click_then_back() {
        let events = vec![
            ev(0, Kind::Move, 0.2, 0.2),
            ev(1500, Kind::Move, 0.7, 0.3),
            ev(2000, Kind::Down, 0.7, 0.3),
            ev(2100, Kind::Up, 0.7, 0.3),
        ];
        let f = build(&events, 6 * 30, 30.0, &Settings::default());
        assert!((f[6].z - 1.0).abs() < 0.05, "idle before the lead window: {}", f[6].z);
        let mid = &f[(2.6 * 30.0) as usize];
        assert!(mid.z > 1.7, "zoomed after click: {}", mid.z);
        assert!(mid.cx > 0.6, "camera moved toward the click: {}", mid.cx);
        assert!(f.last().unwrap().z < 1.05, "zoomed back out: {}", f.last().unwrap().z);
        for fr in &f {
            let h = 0.5 / fr.z;
            assert!(fr.cx >= h - 1e-4 && fr.cx <= 1.0 - h + 1e-4, "viewport stays inside the frame");
        }
    }

    #[test]
    fn close_clicks_share_one_zoom() {
        let events = vec![
            ev(0, Kind::Move, 0.5, 0.5),
            ev(2000, Kind::Down, 0.5, 0.5),
            ev(3500, Kind::Down, 0.55, 0.5),
        ];
        assert_eq!(zoom_segments(&events).len(), 1);
        let far = vec![ev(1000, Kind::Down, 0.5, 0.5), ev(9000, Kind::Down, 0.5, 0.5)];
        assert_eq!(zoom_segments(&far).len(), 2);
    }

    #[test]
    fn zoom_off_and_no_events_stay_static() {
        let events = vec![ev(0, Kind::Move, 0.5, 0.5), ev(1000, Kind::Down, 0.5, 0.5)];
        let s = Settings { zoom: false, ..Default::default() };
        assert!(build(&events, 90, 30.0, &s).iter().all(|f| (f.z - 1.0).abs() < 1e-6));
        let none = build(&[], 30, 30.0, &Settings::default());
        assert!(none.iter().all(|f| !f.has_cursor && f.z == 1.0));
    }

    #[test]
    fn dead_zone_keeps_camera_still_for_small_moves() {
        let mut events = vec![ev(0, Kind::Move, 0.5, 0.5), ev(1000, Kind::Down, 0.5, 0.5)];
        for i in 0..40 {
            events.push(ev(1200 + i * 30, Kind::Move, 0.5 + (i as f32 % 2.0) * 0.02, 0.5));
        }
        let f = build(&events, 90, 30.0, &Settings::default());
        let (a, b) = (f[50].cx, f[80].cx);
        assert!((a - b).abs() < 0.01, "jitter shouldn't move the camera: {a} vs {b}");
    }
}
