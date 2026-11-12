use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Move,
    Down,
    Up,
}

/// One mouse event on the *video* timeline. `x`/`y` are 0..1 within the recorded screen.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct InputEvent {
    /// Milliseconds since the start of the video.
    pub t: u32,
    pub k: Kind,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub bg_from: String,
    pub bg_to: String,
    /// Gradient direction in degrees (0 = left→right, 90 = top→bottom).
    pub bg_angle: f32,
    /// Frame inset, percent of the shorter canvas side.
    pub padding: f32,
    /// Corner radius in pixels at 1080p.
    pub radius: f32,
    /// 0..1
    pub shadow: f32,
    pub zoom: bool,
    pub zoom_level: f32,
    pub cursor: bool,
    pub cursor_scale: f32,
    pub click_fx: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bg_from: "#6a5af9".into(),
            bg_to: "#d66efd".into(),
            bg_angle: 135.0,
            padding: 6.0,
            radius: 16.0,
            shadow: 0.6,
            zoom: true,
            zoom_level: 1.9,
            cursor: true,
            cursor_scale: 1.6,
            click_fx: true,
        }
    }
}

/// "#rrggbb" (or "rrggbb") → RGB; falls back to mid-grey on bad input.
pub fn parse_hex(s: &str) -> [u8; 3] {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        }
    }
    [128, 128, 128]
}
