use crate::{parse_hex, Frame, InputEvent, Kind, Settings};
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, LineJoin, Mask, Paint, Path,
    PathBuilder, Pattern, Pixmap, PixmapPaint, Point, Rect, SpreadMode, Stroke, Transform,
};

/// Source pixels covered by the click ripple at its widest, before cursor scaling.
const RIPPLE_MS: f32 = 450.0;

pub struct Compositor {
    w: u32,
    h: u32,
    s: Settings,
    bg: Pixmap,
    canvas: Pixmap,
    mask: Mask,
    clip: Path,
    cursor: Path,
    /// (x, y, w, h) of the framed video on the canvas.
    inner: (f32, f32, f32, f32),
    /// (time ms, x, y) of every press, for ripples.
    clicks: Vec<(f32, f32, f32)>,
}

fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let mut pb = PathBuilder::new();
    if r < 0.5 {
        pb.push_rect(Rect::from_xywh(x, y, w, h)?);
        return pb.finish();
    }
    let k = 0.552_284_75 * r;
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Classic arrow pointer, hot-spot at the origin, ~20 px tall at scale 1.
fn arrow() -> Path {
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, 0.0);
    pb.line_to(0.0, 16.5);
    pb.line_to(4.2, 12.8);
    pb.line_to(6.9, 19.2);
    pb.line_to(9.4, 18.1);
    pb.line_to(6.8, 11.9);
    pb.line_to(12.2, 11.9);
    pb.close();
    pb.finish().expect("arrow path")
}

/// Three box-blur passes ≈ gaussian, applied to the alpha plane only.
fn blur_alpha(pm: &mut Pixmap, radius: usize) {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let mut a: Vec<u8> = pm.data().chunks_exact(4).map(|p| p[3]).collect();
    let mut tmp = vec![0u8; a.len()];
    let r = radius.max(1);
    let div = (2 * r + 1) as u32;
    for _ in 0..3 {
        for y in 0..h {
            let row = &a[y * w..(y + 1) * w];
            let mut sum: u32 = row[..=r.min(w - 1)].iter().map(|&v| v as u32).sum();
            for x in 0..w {
                tmp[y * w + x] = (sum / div) as u8;
                if x + r + 1 < w {
                    sum += row[x + r + 1] as u32;
                }
                if x >= r {
                    sum -= row[x - r] as u32;
                }
            }
        }
        for x in 0..w {
            let mut sum: u32 = (0..=r.min(h - 1)).map(|y| tmp[y * w + x] as u32).sum();
            for y in 0..h {
                a[y * w + x] = (sum / div) as u8;
                if y + r + 1 < h {
                    sum += tmp[(y + r + 1) * w + x] as u32;
                }
                if y >= r {
                    sum -= tmp[(y - r) * w + x] as u32;
                }
            }
        }
    }
    for (p, &al) in pm.data_mut().chunks_exact_mut(4).zip(a.iter()) {
        p.copy_from_slice(&[0, 0, 0, al]); // premultiplied black
    }
}

impl Compositor {
    /// `w`×`h` is the source (and output) size.
    pub fn new(w: u32, h: u32, s: &Settings, events: &[InputEvent]) -> Result<Self, String> {
        let err = || "ukuran video tidak valid".to_string();
        let (wf, hf) = (w as f32, h as f32);

        // Framed video rectangle: same aspect as the source, inset by `padding`.
        let pad = (s.padding.clamp(0.0, 30.0) / 100.0) * wf.min(hf);
        let k = ((wf - 2.0 * pad) / wf).min((hf - 2.0 * pad) / hf).max(0.1);
        let (iw, ih) = (wf * k, hf * k);
        let (ix, iy) = ((wf - iw) / 2.0, (hf - ih) / 2.0);
        let radius = if pad > 0.5 { s.radius.max(0.0) * hf / 1080.0 } else { 0.0 };
        let clip = rrect(ix, iy, iw, ih, radius).ok_or_else(err)?;

        // Background gradient.
        let mut bg = Pixmap::new(w, h).ok_or_else(err)?;
        let (a, b) = (parse_hex(&s.bg_from), parse_hex(&s.bg_to));
        let ang = s.bg_angle.to_radians();
        let (dx, dy) = (ang.cos(), ang.sin());
        let half = (wf * dx.abs() + hf * dy.abs()) / 2.0;
        let shader = LinearGradient::new(
            Point::from_xy(wf / 2.0 - dx * half, hf / 2.0 - dy * half),
            Point::from_xy(wf / 2.0 + dx * half, hf / 2.0 + dy * half),
            vec![
                GradientStop::new(0.0, Color::from_rgba8(a[0], a[1], a[2], 255)),
                GradientStop::new(1.0, Color::from_rgba8(b[0], b[1], b[2], 255)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .ok_or_else(err)?;
        let mut paint = Paint::default();
        paint.shader = shader;
        bg.fill_rect(Rect::from_xywh(0.0, 0.0, wf, hf).ok_or_else(err)?, &paint, Transform::identity(), None);

        // Drop shadow, rendered once.
        if pad > 0.5 && s.shadow > 0.01 {
            if let Some(mut sh) = Pixmap::new(w, h) {
                let off = hf * 0.012;
                if let Some(p) = rrect(ix, iy + off, iw, ih, radius) {
                    let mut sp = Paint::default();
                    sp.set_color_rgba8(0, 0, 0, (s.shadow.clamp(0.0, 1.0) * 150.0) as u8);
                    sp.anti_alias = true;
                    sh.fill_path(&p, &sp, FillRule::Winding, Transform::identity(), None);
                    blur_alpha(&mut sh, ((hf * 0.014) as usize).max(4));
                    bg.draw_pixmap(0, 0, sh.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
                }
            }
        }

        let mut mask = Mask::new(w, h).ok_or_else(err)?;
        mask.fill_path(&clip, FillRule::Winding, true, Transform::identity());

        let clicks = events
            .iter()
            .filter(|e| e.k == Kind::Down)
            .map(|e| (e.t as f32, e.x, e.y))
            .collect();

        Ok(Compositor {
            w,
            h,
            s: s.clone(),
            canvas: bg.clone(),
            bg,
            mask,
            clip,
            cursor: arrow(),
            inner: (ix, iy, iw, ih),
            clicks,
        })
    }

    /// Compose one output frame. `src` must be `w`×`h`; `ms` is the frame time.
    /// The last composed frame, for drawing overlays (captions) on top of it.
    pub fn canvas_mut(&mut self) -> &mut Pixmap {
        &mut self.canvas
    }

    pub fn canvas(&self) -> &Pixmap {
        &self.canvas
    }

    pub fn render(&mut self, src: &Pixmap, f: &Frame, ms: f32) -> &Pixmap {
        self.canvas.data_mut().copy_from_slice(self.bg.data());
        let (ix, iy, iw, _) = self.inner;
        let (sw, sh) = (self.w as f32, self.h as f32);

        // Camera: which part of the source is visible, and how it maps onto the frame.
        let z = f.z.max(1.0);
        let (vw, vh) = (sw / z, sh / z);
        let cropx = (f.cx * sw - vw / 2.0).clamp(0.0, sw - vw);
        let cropy = (f.cy * sh - vh / 2.0).clamp(0.0, sh - vh);
        let k = iw / vw; // source px → canvas px
        let tf = Transform::from_row(k, 0.0, 0.0, k, ix - cropx * k, iy - cropy * k);

        let mut paint = Paint::default();
        paint.shader = Pattern::new(src.as_ref(), SpreadMode::Pad, FilterQuality::Bilinear, 1.0, tf);
        paint.anti_alias = true;
        self.canvas.fill_path(&self.clip, &paint, FillRule::Winding, Transform::identity(), None);

        if !f.has_cursor {
            return &self.canvas;
        }
        let to_canvas = |x: f32, y: f32| ((x * sw - cropx) * k + ix, (y * sh - cropy) * k + iy);
        let cs = self.s.cursor_scale.clamp(0.5, 5.0);

        if self.s.click_fx {
            let lo = self.clicks.partition_point(|c| c.0 < ms - RIPPLE_MS);
            for &(t0, x, y) in &self.clicks[lo..] {
                if t0 > ms {
                    break;
                }
                let u = (ms - t0) / RIPPLE_MS;
                let ease = 1.0 - (1.0 - u) * (1.0 - u);
                let (cx, cy) = to_canvas(x, y);
                let r = (7.0 + 22.0 * ease) * k * cs;
                if let Some(circle) = PathBuilder::from_circle(cx, cy, r) {
                    let mut p = Paint::default();
                    p.anti_alias = true;
                    p.set_color_rgba8(255, 255, 255, ((1.0 - u) * 80.0) as u8);
                    self.canvas.fill_path(&circle, &p, FillRule::Winding, Transform::identity(), Some(&self.mask));
                    p.set_color_rgba8(255, 255, 255, ((1.0 - u) * 220.0) as u8);
                    let st = Stroke { width: (2.0 * k * cs).max(1.5), ..Default::default() };
                    self.canvas.stroke_path(&circle, &p, &st, Transform::identity(), Some(&self.mask));
                }
            }
        }

        // Pointer: soft shadow, white outline, black fill. Shrinks slightly while pressed.
        let (px, py) = to_canvas(f.mx, f.my);
        let scale = k * cs * (1.0 - 0.14 * f.press);
        let tf = |dx: f32, dy: f32| Transform::from_row(scale, 0.0, 0.0, scale, px + dx * scale, py + dy * scale);
        let mut p = Paint::default();
        p.anti_alias = true;
        p.set_color_rgba8(0, 0, 0, 70);
        self.canvas.fill_path(&self.cursor, &p, FillRule::Winding, tf(1.0, 1.6), Some(&self.mask));
        p.set_color_rgba8(255, 255, 255, 255);
        let st = Stroke { width: 2.6, line_join: LineJoin::Round, ..Default::default() };
        self.canvas.stroke_path(&self.cursor, &p, &st, tf(0.0, 0.0), Some(&self.mask));
        p.set_color_rgba8(18, 18, 22, 255);
        self.canvas.fill_path(&self.cursor, &p, FillRule::Winding, tf(0.0, 0.0), Some(&self.mask));

        &self.canvas
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(p: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * p.width() + x) * 4) as usize;
        [p.data()[i], p.data()[i + 1], p.data()[i + 2], p.data()[i + 3]]
    }

    #[test]
    fn frame_is_inset_with_background_around_it() {
        let s = Settings { bg_from: "#0000ff".into(), bg_to: "#0000ff".into(), shadow: 0.0, ..Default::default() };
        let mut src = Pixmap::new(320, 200).unwrap();
        src.fill(Color::from_rgba8(255, 0, 0, 255));
        let mut c = Compositor::new(320, 200, &s, &[]).unwrap();
        let out = c.render(&src, &Frame { z: 1.0, cx: 0.5, cy: 0.5, ..Default::default() }, 0.0);
        assert_eq!(px(out, 2, 2), [0, 0, 255, 255], "corner is background");
        assert_eq!(px(out, 160, 100), [255, 0, 0, 255], "centre is the video");
    }

    #[test]
    fn zoom_crops_the_right_region() {
        // Left half green, right half red; zoom 2× at the right-hand side shows only red.
        let s = Settings { padding: 0.0, ..Default::default() };
        let mut src = Pixmap::new(200, 100).unwrap();
        for y in 0..100u32 {
            for x in 0..200u32 {
                let i = ((y * 200 + x) * 4) as usize;
                let c = if x < 100 { [0, 255, 0, 255] } else { [255, 0, 0, 255] };
                src.data_mut()[i..i + 4].copy_from_slice(&c);
            }
        }
        let mut c = Compositor::new(200, 100, &s, &[]).unwrap();
        let f = Frame { z: 2.0, cx: 0.75, cy: 0.5, ..Default::default() };
        let out = c.render(&src, &f, 0.0);
        assert_eq!(px(out, 5, 50), [255, 0, 0, 255]);
        assert_eq!(px(out, 195, 50), [255, 0, 0, 255]);
    }

    #[test]
    fn cursor_is_drawn_only_when_asked() {
        let s = Settings { padding: 0.0, ..Default::default() };
        let mut src = Pixmap::new(200, 100).unwrap();
        src.fill(Color::from_rgba8(200, 200, 200, 255));
        let mut c = Compositor::new(200, 100, &s, &[]).unwrap();
        let base = Frame { z: 1.0, cx: 0.5, cy: 0.5, mx: 0.5, my: 0.5, ..Default::default() };
        let without = c.render(&src, &base, 0.0).data().to_vec();
        let with = c.render(&src, &Frame { has_cursor: true, ..base }, 0.0).data().to_vec();
        assert_ne!(without, with);
        assert_eq!(px(c.render(&src, &Frame { has_cursor: true, ..base }, 0.0), 101, 52)[0], 18, "black arrow body");
    }
}
