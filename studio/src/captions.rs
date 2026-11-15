//! Subtitles: SRT parsing, line splitting and text rendering with any TrueType/OpenType font.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tiny_skia::{Color, FillRule, Paint, Pixmap, Transform};

use crate::parse_hex;

/// Font bundled with the app (Liberation Sans Bold, SIL OFL) so captions work out of the box.
const DEFAULT_FONT: &[u8] = include_bytes!("../assets/LiberationSans-Bold.ttf");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caption {
    /// Milliseconds on the video timeline.
    pub start: u32,
    pub end: u32,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Bottom,
    Center,
    Top,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptionStyle {
    pub enabled: bool,
    /// Path to a .ttf/.otf file; `None` uses the bundled font.
    pub font_path: Option<String>,
    /// Text height as percent of the output height.
    pub size: f32,
    pub color: String,
    pub outline_color: String,
    /// Outline thickness in pixels at 1080p.
    pub outline: f32,
    pub box_on: bool,
    pub box_color: String,
    pub box_opacity: f32,
    pub position: Position,
    /// Distance from the top/bottom edge, percent of output height.
    pub margin: f32,
    /// Widest a line may get, percent of output width.
    pub max_width: f32,
    pub uppercase: bool,
}

impl Default for CaptionStyle {
    fn default() -> Self {
        CaptionStyle {
            enabled: false,
            font_path: None,
            size: 4.4,
            color: "#ffffff".into(),
            outline_color: "#000000".into(),
            outline: 3.0,
            box_on: false,
            box_color: "#000000".into(),
            box_opacity: 0.55,
            position: Position::Bottom,
            margin: 6.0,
            max_width: 80.0,
            uppercase: false,
        }
    }
}

// ------------------------------------------------------------------ SRT

fn parse_time(s: &str) -> Option<u32> {
    let s = s.trim().replace(',', ".");
    let mut it = s.split(':');
    let (h, m, sec): (u32, u32, f64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    Some(h * 3_600_000 + m * 60_000 + (sec * 1000.0).round() as u32)
}

fn fmt_time(ms: u32) -> String {
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

pub fn parse_srt(src: &str) -> Vec<Caption> {
    let src = src.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut out = Vec::new();
    for block in src.split("\n\n") {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let Some(first) = lines.next() else { continue };
        // The numeric index line is optional in the wild.
        let time_line = if first.contains("-->") { Some(first) } else { lines.next() };
        let Some((a, b)) = time_line.and_then(|l| l.split_once("-->")) else { continue };
        let (Some(start), Some(end)) = (parse_time(a), parse_time(b.split_whitespace().next().unwrap_or(b))) else { continue };
        let text = lines.collect::<Vec<_>>().join("\n");
        if !text.trim().is_empty() && end > start {
            out.push(Caption { start, end, text });
        }
    }
    out
}

pub fn to_srt(caps: &[Caption]) -> String {
    let mut s = String::new();
    for (i, c) in caps.iter().enumerate() {
        s += &format!("{}\n{} --> {}\n{}\n\n", i + 1, fmt_time(c.start), fmt_time(c.end), c.text);
    }
    s
}

/// Break one long speech segment into readable chunks of at most `max_chars`, sharing the time
/// span in proportion to each chunk's length.
pub fn split_caption(text: &str, start: u32, end: u32, max_chars: usize) -> Vec<Caption> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || end <= start {
        return vec![];
    }
    let mut chunks: Vec<String> = vec![];
    let mut cur = String::new();
    for w in words {
        if !cur.is_empty() && cur.chars().count() + 1 + w.chars().count() > max_chars {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur += w;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    let total: usize = chunks.iter().map(|c| c.chars().count()).sum::<usize>().max(1);
    let span = (end - start) as f64;
    let mut t = start as f64;
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            let s = t.round() as u32;
            t += span * text.chars().count() as f64 / total as f64;
            let e = if i + 1 == n { end } else { t.round() as u32 };
            Caption { start: s, end: e, text }
        })
        .collect()
}

/// Whisper marks silence and sound effects as `[BLANK_AUDIO]`, `(music)`, `*applause*` …
pub fn is_noise(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('(') && t.ends_with(')'))
        || (t.starts_with('*') && t.ends_with('*'))
        || t.chars().all(|c| !c.is_alphanumeric())
}

// ------------------------------------------------------------------ rendering

type Glyph = (fontdue::Metrics, Vec<u8>);

pub struct Captioner {
    font: fontdue::Font,
    style: CaptionStyle,
    w: u32,
    h: u32,
    cache: HashMap<char, Glyph>,
    px: f32,
}

/// Load a font file chosen by the user, or the bundled one.
pub fn load_font(path: Option<&str>) -> Result<fontdue::Font, String> {
    let bytes: Vec<u8> = match path {
        Some(p) if !p.is_empty() => std::fs::read(p).map_err(|e| format!("font tidak bisa dibaca ({p}): {e}"))?,
        _ => DEFAULT_FONT.to_vec(),
    };
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).map_err(|e| format!("file font tidak valid: {e}"))
}

impl Captioner {
    /// `w`×`h` is the output canvas the captions are drawn onto.
    pub fn new(style: &CaptionStyle, w: u32, h: u32) -> Result<Self, String> {
        let font = match load_font(style.font_path.as_deref()) {
            Ok(f) => f,
            // A deleted font file must not break the export; fall back to the bundled one.
            Err(_) => load_font(None)?,
        };
        let px = (style.size / 100.0 * h as f32).clamp(8.0, 400.0);
        Ok(Captioner { font, style: style.clone(), w, h, cache: HashMap::new(), px })
    }

    fn glyph(&mut self, c: char) -> &Glyph {
        let px = self.px;
        let font = &self.font;
        self.cache.entry(c).or_insert_with(|| font.rasterize(c, px))
    }

    fn advance(&mut self, c: char) -> f32 {
        self.glyph(c).0.advance_width
    }

    fn text_width(&mut self, s: &str) -> f32 {
        s.chars().map(|c| self.advance(c)).sum()
    }

    /// Greedy word wrap to `max` pixels.
    fn wrap(&mut self, text: &str, max: f32) -> Vec<String> {
        let mut lines = vec![];
        for para in text.lines() {
            let mut cur = String::new();
            for word in para.split_whitespace() {
                let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
                if !cur.is_empty() && self.text_width(&cand) > max {
                    lines.push(std::mem::take(&mut cur));
                    cur = word.to_string();
                } else {
                    cur = cand;
                }
            }
            if !cur.is_empty() {
                lines.push(cur);
            }
        }
        lines
    }

    /// Draw the caption active at `t_ms` (if any) onto `pm`.
    pub fn draw(&mut self, pm: &mut Pixmap, t_ms: u32, caps: &[Caption]) {
        let Some(cap) = caps.iter().rev().find(|c| t_ms >= c.start && t_ms < c.end) else { return };
        let text = if self.style.uppercase { cap.text.to_uppercase() } else { cap.text.clone() };
        let max = self.w as f32 * self.style.max_width.clamp(20.0, 100.0) / 100.0;
        let lines = self.wrap(&text, max);
        if lines.is_empty() {
            return;
        }
        let Some(lm) = self.font.horizontal_line_metrics(self.px) else { return };
        let line_h = (lm.ascent - lm.descent + lm.line_gap).max(self.px);
        let block_h = line_h * lines.len() as f32;
        let margin = self.style.margin / 100.0 * self.h as f32;
        let top = match self.style.position {
            Position::Bottom => self.h as f32 - margin - block_h,
            Position::Top => margin,
            Position::Center => (self.h as f32 - block_h) / 2.0,
        };
        let scale = self.h as f32 / 1080.0;
        let outline = (self.style.outline * scale).round().clamp(0.0, 12.0) as i32;
        let [r, g, b] = parse_hex(&self.style.color);
        let [or, og, ob] = parse_hex(&self.style.outline_color);
        let [br, bg, bb] = parse_hex(&self.style.box_color);

        for (i, line) in lines.iter().enumerate() {
            let lw = self.text_width(line);
            let x0 = (self.w as f32 - lw) / 2.0;
            let y_line = top + i as f32 * line_h;
            let baseline = y_line + lm.ascent + lm.line_gap / 2.0;

            if self.style.box_on {
                let pad = self.px * 0.28;
                if let Some(rect) = tiny_skia::Rect::from_xywh(x0 - pad, y_line - pad * 0.25, lw + pad * 2.0, line_h + pad * 0.5) {
                    let mut paint = Paint::default();
                    paint.set_color(Color::from_rgba8(br, bg, bb, (self.style.box_opacity.clamp(0.0, 1.0) * 255.0) as u8));
                    paint.anti_alias = true;
                    let radius = self.px * 0.22;
                    if let Some(p) = round_rect(rect, radius) {
                        pm.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
                    }
                }
            }

            let chars: Vec<char> = line.chars().collect();
            // Outline pass first, then the fill, so the outline never covers a neighbouring letter.
            for pass in 0..2 {
                if pass == 0 && outline == 0 {
                    continue;
                }
                let mut x = x0;
                for (k, &c) in chars.iter().enumerate() {
                    let (m, bmp) = {
                        let g = self.glyph(c);
                        (g.0, g.1.clone())
                    };
                    let gx = (x + m.xmin as f32).round() as i32;
                    let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
                    if pass == 0 {
                        for (dx, dy) in ring(outline) {
                            blit(pm, &bmp, m.width, m.height, gx + dx, gy + dy, [or, og, ob]);
                        }
                    } else {
                        blit(pm, &bmp, m.width, m.height, gx, gy, [r, g, b]);
                    }
                    x += m.advance_width;
                    if let Some(&n) = chars.get(k + 1) {
                        x += self.font.horizontal_kern(c, n, self.px).unwrap_or(0.0);
                    }
                }
            }
        }
    }
}

fn round_rect(r: tiny_skia::Rect, radius: f32) -> Option<tiny_skia::Path> {
    let (x, y, w, h) = (r.x(), r.y(), r.width(), r.height());
    let rad = radius.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_75 * rad;
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(x + rad, y);
    pb.line_to(x + w - rad, y);
    pb.cubic_to(x + w - rad + k, y, x + w, y + rad - k, x + w, y + rad);
    pb.line_to(x + w, y + h - rad);
    pb.cubic_to(x + w, y + h - rad + k, x + w - rad + k, y + h, x + w - rad, y + h);
    pb.line_to(x + rad, y + h);
    pb.cubic_to(x + rad - k, y + h, x, y + h - rad + k, x, y + h - rad);
    pb.line_to(x, y + rad);
    pb.cubic_to(x, y + rad - k, x + rad - k, y, x + rad, y);
    pb.close();
    pb.finish()
}

/// Offsets around a point at distance `r`, dense enough that a thick outline has no gaps.
fn ring(r: i32) -> Vec<(i32, i32)> {
    let mut v = vec![];
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                v.push((dx, dy));
            }
        }
    }
    v
}

/// Source-over a coverage bitmap tinted with `rgb` onto a premultiplied RGBA pixmap.
fn blit(pm: &mut Pixmap, cov: &[u8], gw: usize, gh: usize, x: i32, y: i32, rgb: [u8; 3]) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for row in 0..gh as i32 {
        let py = y + row;
        if py < 0 || py >= ph {
            continue;
        }
        for col in 0..gw as i32 {
            let px = x + col;
            if px < 0 || px >= pw {
                continue;
            }
            let a = cov[row as usize * gw + col as usize] as u32;
            if a == 0 {
                continue;
            }
            let i = (py * pw + px) as usize * 4;
            let inv = 255 - a;
            for (k, &c) in rgb.iter().enumerate() {
                data[i + k] = ((c as u32 * a + data[i + k] as u32 * inv) / 255) as u8;
            }
            data[i + 3] = (a + data[i + 3] as u32 * inv / 255).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_roundtrip() {
        let caps = vec![
            Caption { start: 1500, end: 3200, text: "Halo semuanya".into() },
            Caption { start: 3_700_000, end: 3_701_250, text: "baris satu\nbaris dua".into() },
        ];
        let srt = to_srt(&caps);
        assert!(srt.contains("00:00:01,500 --> 00:00:03,200"));
        assert!(srt.contains("01:01:40,000 --> 01:01:41,250"));
        assert_eq!(parse_srt(&srt), caps);
    }

    #[test]
    fn srt_tolerates_crlf_bom_and_missing_index() {
        let s = "\u{feff}1\r\n00:00:00,000 --> 00:00:02,000\r\nsatu\r\n\r\n00:00:02,000 --> 00:00:04,500 X1:0\r\ndua\r\n";
        let c = parse_srt(s);
        assert_eq!(c.len(), 2);
        assert_eq!((c[1].start, c[1].end, c[1].text.as_str()), (2000, 4500, "dua"));
    }

    #[test]
    fn split_keeps_text_and_covers_the_span() {
        let t = "ini kalimat yang cukup panjang supaya harus dipecah menjadi beberapa potongan pendek";
        let v = split_caption(t, 1000, 9000, 30);
        assert!(v.len() >= 3);
        assert!(v.iter().all(|c| c.text.chars().count() <= 30));
        assert_eq!(v.first().unwrap().start, 1000);
        assert_eq!(v.last().unwrap().end, 9000);
        assert!(v.windows(2).all(|w| w[0].end == w[1].start));
        assert_eq!(v.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" "), t);
    }

    #[test]
    fn noise_tags_are_dropped() {
        assert!(is_noise("[BLANK_AUDIO]"));
        assert!(is_noise(" (musik) "));
        assert!(is_noise("..."));
        assert!(!is_noise("Halo"));
    }

    #[test]
    fn draws_text_only_while_active_and_inside_the_requested_area() {
        let (w, h) = (640, 360);
        let style = CaptionStyle { enabled: true, ..Default::default() };
        let mut cap = Captioner::new(&style, w, h).unwrap();
        let caps = vec![Caption { start: 1000, end: 2000, text: "Halo dunia, ini subtitle".into() }];
        let blank = Pixmap::new(w, h).unwrap();

        let mut off = blank.clone();
        cap.draw(&mut off, 500, &caps);
        assert!(off.data().iter().all(|&b| b == 0), "nothing before the caption starts");

        let mut on = blank.clone();
        cap.draw(&mut on, 1500, &caps);
        let rows: Vec<u32> = (0..h).filter(|&y| (0..w).any(|x| on.pixel(x, y).unwrap().alpha() > 0)).collect();
        assert!(!rows.is_empty(), "caption should paint something");
        // Default position is the bottom; nothing should appear in the upper half.
        assert!(rows.iter().all(|&y| y > h / 2), "rows: {:?}..{:?}", rows.first(), rows.last());
        // White fill is present, not just the black outline.
        assert!((0..h).any(|y| (0..w).any(|x| {
            let p = on.pixel(x, y).unwrap();
            p.red() > 200 && p.green() > 200 && p.alpha() == 255
        })));
    }

    #[test]
    fn missing_font_file_falls_back_to_bundled() {
        let style = CaptionStyle { font_path: Some("/tidak/ada/font.ttf".into()), ..Default::default() };
        assert!(Captioner::new(&style, 320, 180).is_ok());
        assert!(load_font(Some("/tidak/ada/font.ttf")).is_err());
    }
}
