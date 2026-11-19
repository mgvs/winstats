//! Premultiplied BGRA framebuffer with simple primitives, GDI-rendered text,
//! and presentation through UpdateLayeredWindow (per-pixel alpha).

use crate::util::{pcwstr, wide};
use std::collections::HashMap;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow, UpdateLayeredWindowIndirect, UPDATELAYEREDWINDOWINFO};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    pub fn alpha(self, a: f32) -> Self {
        Self { a: (self.a as f32 * a.clamp(0.0, 1.0)).round() as u8, ..self }
    }
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 && s.len() != 8 {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        if s.len() == 6 {
            Some(Self::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
        } else {
            Some(Self::rgba((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8))
        }
    }
}

pub const WHITE: Color = Color::rgb(255, 255, 255);
pub const BLACK: Color = Color::rgb(0, 0, 0);
pub const GREEN: Color = Color::rgb(0x4C, 0xAF, 0x50);
pub const YELLOW: Color = Color::rgb(0xFF, 0xC1, 0x07);
pub const ORANGE: Color = Color::rgb(0xFF, 0x98, 0x00);
pub const RED: Color = Color::rgb(0xF4, 0x43, 0x36);

/// `c` moved towards white by `f` (0..1).
pub fn lighten_color(c: Color, f: f32) -> Color {
    let l = |v: u8| (v as f32 + (255.0 - v as f32) * f.clamp(0.0, 1.0)).round() as u8;
    Color::rgba(l(c.r), l(c.g), l(c.b), c.a)
}

/// `a` moved towards `b` by `f` (0..1), alpha included.
pub fn mix_color(a: Color, b: Color, f: f32) -> Color {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    Color::rgba(m(a.r, b.r), m(a.g, b.g), m(a.b, b.b), m(a.a, b.a))
}

/// Colour for a 0..1 utilisation value: base below 0.6, then yellow, orange, red.
pub fn usage_color(v: f32, base: Color) -> Color {
    if v < 0.6 {
        base
    } else if v < 0.75 {
        YELLOW
    } else if v < 0.9 {
        ORANGE
    } else {
        RED
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FontSpec {
    pub px: i32,
    pub weight: i32,
}

pub const FW_NORMAL: i32 = 400;
pub const FW_SEMIBOLD: i32 = 600;
/// Not a weight: Windows' symbol font (Segoe Fluent Icons) instead of the UI font.
pub const FW_ICON: i32 = 1;

pub struct Canvas {
    pub w: i32,
    pub h: i32,
    /// premultiplied BGRA
    px: Vec<u32>,
    text: TextRenderer,
    dib: Option<Dib>,
}

impl Canvas {
    pub fn new() -> Self {
        Self { w: 0, h: 0, px: Vec::new(), text: TextRenderer::new(), dib: None }
    }

    pub fn resize(&mut self, w: i32, h: i32) {
        let w = w.max(1);
        let h = h.max(1);
        if w != self.w || h != self.h {
            self.w = w;
            self.h = h;
            self.px = vec![0; (w * h) as usize];
            self.dib = None;
        }
    }

    pub fn clear(&mut self) {
        self.px.iter_mut().for_each(|p| *p = 0);
    }

    #[inline]
    fn blend_px(&mut self, x: i32, y: i32, c: Color, coverage: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let a = (c.a as f32 / 255.0) * coverage;
        if a <= 0.0 {
            return;
        }
        let idx = (y * self.w + x) as usize;
        let d = self.px[idx];
        let db = (d & 0xFF) as f32;
        let dg = ((d >> 8) & 0xFF) as f32;
        let dr = ((d >> 16) & 0xFF) as f32;
        let da = ((d >> 24) & 0xFF) as f32;
        let inv = 1.0 - a;
        let nb = c.b as f32 * a + db * inv;
        let ng = c.g as f32 * a + dg * inv;
        let nr = c.r as f32 * a + dr * inv;
        let na = 255.0 * a + da * inv;
        self.px[idx] = (na.round() as u32) << 24
            | (nr.round() as u32) << 16
            | (ng.round() as u32) << 8
            | (nb.round() as u32);
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                self.blend_px(xx, yy, c, 1.0);
            }
        }
    }

    /// Fill a bar growing up from `bottom` (exclusive) with fractional height:
    /// the top-most partial pixel row gets partial coverage.
    pub fn fill_bar(&mut self, x: i32, bottom: i32, w: i32, h: f32, c: Color) {
        if h <= 0.0 {
            return;
        }
        let full = h.floor() as i32;
        let frac = h - full as f32;
        self.fill_rect(x, bottom - full, w, full, c);
        if frac > 0.02 {
            let yy = bottom - full - 1;
            for xx in x.max(0)..(x + w).min(self.w) {
                self.blend_px(xx, yy, c, frac);
            }
        }
    }

    /// 1px outline with softened corners.
    pub fn stroke_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        if w < 3 || h < 3 {
            return;
        }
        let x2 = x + w - 1;
        let y2 = y + h - 1;
        for xx in (x + 1)..x2 {
            self.blend_px(xx, y, c, 1.0);
            self.blend_px(xx, y2, c, 1.0);
        }
        for yy in (y + 1)..y2 {
            self.blend_px(x, yy, c, 1.0);
            self.blend_px(x2, yy, c, 1.0);
        }
        for (cx, cy) in [(x, y), (x2, y), (x, y2), (x2, y2)] {
            self.blend_px(cx, cy, c, 0.35);
        }
    }

    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        let x0 = (cx - r - 1.0).floor() as i32;
        let x1 = (cx + r + 1.0).ceil() as i32;
        let y0 = (cy - r - 1.0).floor() as i32;
        let y1 = (cy + r + 1.0).ceil() as i32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let cov = (r - d + 0.5).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_px(x, y, c, cov);
                }
            }
        }
    }

    /// Filled line chart: `values` in 0..1, right-aligned in the box. With fewer samples than
    /// columns the series is stretched across the full width (linear interpolation); with more,
    /// the last `w` samples are drawn one per column.
    pub fn line_chart(&mut self, x: i32, y: i32, w: i32, h: i32, values: &[f32], line: Color, fill: Color) {
        if w <= 0 || h <= 0 || values.is_empty() {
            return;
        }
        let stretched: Vec<f32>;
        let series: &[f32] = if values.len() >= 2 && (values.len() as i32) < w {
            let n = values.len();
            stretched = (0..w)
                .map(|i| {
                    let pos = i as f32 * (n - 1) as f32 / (w - 1).max(1) as f32;
                    let a = pos.floor() as usize;
                    let b = (a + 1).min(n - 1);
                    let t = pos - a as f32;
                    values[a] * (1.0 - t) + values[b] * t
                })
                .collect();
            &stretched
        } else {
            values
        };
        let n = series.len().min(w as usize);
        let start = series.len() - n;
        let x0 = x + w - n as i32;
        let mut prev_top: Option<i32> = None;
        for (i, v) in series[start..].iter().enumerate() {
            let col = x0 + i as i32;
            let vh = v.clamp(0.0, 1.0) * h as f32;
            self.fill_bar(col, y + h, 1, vh, fill);
            let top = (y + h - vh.round() as i32).clamp(y, y + h - 1);
            self.blend_px(col, top, line, 1.0);
            if let Some(pt) = prev_top {
                let (a, b) = if pt < top { (pt, top) } else { (top, pt) };
                for yy in a..b {
                    self.blend_px(col, yy, line, 0.6);
                }
            }
            prev_top = Some(top);
        }
    }

    /// Popup area chart: `values` in 0..1, right-aligned and stretched like `line_chart`, with an
    /// antialiased line of `width` px over a fill fading from `fill_alpha` at the line to nothing
    /// at the bottom.
    pub fn area_chart(&mut self, x: i32, y: i32, w: i32, h: i32, values: &[f32], line: Color, fill_alpha: f32, width: f32) {
        if w <= 1 || h <= 1 || values.is_empty() {
            return;
        }
        let n = values.len();
        // one value per column: stretched when short, the newest `w` when long
        let cols: Vec<f32> = if n >= 2 && (n as i32) < w {
            (0..w)
                .map(|i| {
                    let pos = i as f32 * (n - 1) as f32 / (w - 1) as f32;
                    let a = pos.floor() as usize;
                    let b = (a + 1).min(n - 1);
                    let t = pos - a as f32;
                    values[a] * (1.0 - t) + values[b] * t
                })
                .collect()
        } else {
            values[n.saturating_sub(w as usize)..].to_vec()
        };
        let x0 = x + w - cols.len() as i32;
        let bottom = (y + h) as f32;
        let top_of = |v: f32| bottom - v.clamp(0.0, 1.0) * (h as f32 - width);
        for (i, v) in cols.iter().enumerate() {
            let cx = x0 + i as i32;
            let ty = top_of(*v);
            let span = bottom - ty;
            if span <= 0.0 {
                continue;
            }
            for yy in (ty.floor() as i32).max(y)..(y + h) {
                let cov = ((yy as f32 + 1.0).min(bottom) - (yy as f32).max(ty)).clamp(0.0, 1.0);
                // fade towards the bottom
                let k = 1.0 - ((yy as f32 - ty) / h as f32).clamp(0.0, 1.0);
                let a = fill_alpha * (0.25 + 0.75 * k * k);
                if cov > 0.0 {
                    self.blend_px(cx, yy, line, cov * a);
                }
            }
        }
        for i in 1..cols.len() {
            let (ax, bx) = ((x0 + i as i32 - 1) as f32 + 0.5, (x0 + i as i32) as f32 + 0.5);
            self.draw_line(ax, top_of(cols[i - 1]), bx, top_of(cols[i]), width, line);
        }
    }

    /// Pie slice: angles in radians, 0 at 12 o'clock, clockwise; `start <= end`, end - start <= 2π.
    pub fn fill_pie(&mut self, cx: f32, cy: f32, r: f32, start: f32, end: f32, c: Color) {
        let x0 = (cx - r - 1.0).floor() as i32;
        let x1 = (cx + r + 1.0).ceil() as i32;
        let y0 = (cy - r - 1.0).floor() as i32;
        let y1 = (cy + r + 1.0).ceil() as i32;
        let tau = std::f32::consts::PI * 2.0;
        for y in y0..=y1 {
            for x in x0..=x1 {
                // 2x2 supersampling keeps the radial edges smooth
                let mut cov = 0.0;
                for (sx, sy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                    let dx = x as f32 + sx - cx;
                    let dy = y as f32 + sy - cy;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d > r {
                        continue;
                    }
                    let mut a = dx.atan2(-dy);
                    if a < 0.0 {
                        a += tau;
                    }
                    if a >= start && a < end {
                        cov += 0.25;
                    }
                }
                if cov > 0.0 {
                    self.blend_px(x, y, c, cov);
                }
            }
        }
    }

    /// Antialiased filled rounded rectangle.
    pub fn fill_round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, r: f32, c: Color) {
        let r = r.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let cov = round_rect_coverage(xx, yy, x, y, w, h, r);
                if cov > 0.0 {
                    self.blend_px(xx, yy, c, cov);
                }
            }
        }
    }

    /// Soft drop shadow of a rounded rectangle fading out over `blur` px; only painted outside
    /// the rectangle itself, so a translucent card drawn on top is not darkened.
    pub fn fill_shadow(&mut self, x: i32, y: i32, w: i32, h: i32, r: f32, blur: f32, dy: i32, c: Color) {
        let r = r.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
        let (cx, cy) = (x as f32 + w as f32 / 2.0, (y + dy) as f32 + h as f32 / 2.0);
        let (hx, hy) = (w as f32 / 2.0 - r, h as f32 / 2.0 - r);
        let b = blur.ceil() as i32;
        for yy in (y + dy - b).max(0)..(y + dy + h + b).min(self.h) {
            for xx in (x - b).max(0)..(x + w + b).min(self.w) {
                let qx = (xx as f32 + 0.5 - cx).abs() - hx;
                let qy = (yy as f32 + 0.5 - cy).abs() - hy;
                let d = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r).max(0.0);
                let f = (1.0 - d / blur).clamp(0.0, 1.0);
                let cov = f * f * (1.0 - round_rect_coverage(xx, yy, x, y, w, h, r));
                if cov > 0.0 {
                    self.blend_px(xx, yy, c, cov);
                }
            }
        }
    }

    /// Antialiased 1px rounded outline.
    pub fn stroke_round_rect_r(&mut self, x: i32, y: i32, w: i32, h: i32, r: f32, c: Color) {
        let r = r.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let outer = round_rect_coverage(xx, yy, x, y, w, h, r);
                let inner = round_rect_coverage(xx, yy, x + 1, y + 1, w - 2, h - 2, (r - 1.0).max(0.0));
                let cov = (outer - inner).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_px(xx, yy, c, cov);
                }
            }
        }
    }

    /// Draw premultiplied BGRA pixels (`sw` x `sh`) scaled to `w` x `h` at (x, y): bilinear when
    /// shrinking or growing a little, which is what 32 px icons drawn at 24 px need.
    pub fn draw_image(&mut self, src: &[u32], sw: i32, sh: i32, x: i32, y: i32, w: i32, h: i32) {
        if sw <= 0 || sh <= 0 || w <= 0 || h <= 0 || src.len() < (sw * sh) as usize {
            return;
        }
        for dy in 0..h {
            let ty = y + dy;
            if ty < 0 || ty >= self.h {
                continue;
            }
            let fy = ((dy as f32 + 0.5) * sh as f32 / h as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
            let y0 = fy.floor() as i32;
            let y1 = (y0 + 1).min(sh - 1);
            let wy = fy - y0 as f32;
            for dx in 0..w {
                let tx = x + dx;
                if tx < 0 || tx >= self.w {
                    continue;
                }
                let fx = ((dx as f32 + 0.5) * sw as f32 / w as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
                let x0 = fx.floor() as i32;
                let x1 = (x0 + 1).min(sw - 1);
                let wx = fx - x0 as f32;
                let sample = |shift: u32| -> f32 {
                    let p = |xx: i32, yy: i32| ((src[(yy * sw + xx) as usize] >> shift) & 0xFF) as f32;
                    let top = p(x0, y0) * (1.0 - wx) + p(x1, y0) * wx;
                    let bot = p(x0, y1) * (1.0 - wx) + p(x1, y1) * wx;
                    top * (1.0 - wy) + bot * wy
                };
                let sa = sample(24);
                if sa <= 0.5 {
                    continue;
                }
                let (sr, sg, sb) = (sample(16), sample(8), sample(0));
                let idx = (ty * self.w + tx) as usize;
                let d = self.px[idx];
                let inv = 1.0 - sa / 255.0;
                let ch = |s: f32, shift: u32| (s + ((d >> shift) & 0xFF) as f32 * inv).round().min(255.0) as u32;
                self.px[idx] = ch(sa, 24) << 24 | ch(sr, 16) << 16 | ch(sg, 8) << 8 | ch(sb, 0);
            }
        }
    }

    /// Antialiased straight line of `width` px between two points.
    pub fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        let hw = width / 2.0;
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = dx * dx + dy * dy;
        let minx = (x0.min(x1) - hw - 1.0).floor() as i32;
        let maxx = (x0.max(x1) + hw + 1.0).ceil() as i32;
        let miny = (y0.min(y1) - hw - 1.0).floor() as i32;
        let maxy = (y0.max(y1) + hw + 1.0).ceil() as i32;
        for y in miny..=maxy {
            for x in minx..=maxx {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = if len2 > 0.0 { (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let (cx, cy) = (x0 + dx * t, y0 + dy * t);
                let d = ((px - cx) * (px - cx) + (py - cy) * (py - cy)).sqrt();
                let cov = (hw + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_px(x, y, c, cov);
                }
            }
        }
    }

    /// Sankey-style band from a vertical span at `x0` (`y0a`..`y0b`) to one at `x1`
    /// (`y1a`..`y1b`), eased along a smoothstep curve; edges antialiased. With `pulse` =
    /// Some((phase, period, strength)) bright bands travel left to right along it.
    pub fn fill_ribbon(&mut self, x0: f32, x1: f32, (y0a, y0b): (f32, f32), (y1a, y1b): (f32, f32), c: Color, pulse: Option<(f32, f32, f32)>) {
        if x1 <= x0 {
            return;
        }
        let glow = Color { a: 255, ..lighten_color(c, 0.45) };
        for x in (x0.floor() as i32).max(0)..(x1.ceil() as i32).min(self.w) {
            let px = x as f32 + 0.5;
            let t = ((px - x0) / (x1 - x0)).clamp(0.0, 1.0);
            let s = t * t * (3.0 - 2.0 * t);
            let top = y0a + (y1a - y0a) * s;
            let bot = y0b + (y1b - y0b) * s;
            // partial columns at the ends
            let xcov = ((px + 0.5).min(x1) - (px - 0.5).max(x0)).clamp(0.0, 1.0);
            let bright = match pulse {
                Some((phase, period, strength)) if period > 0.0 => {
                    let p = ((px - x0) / period - phase).rem_euclid(1.0);
                    // soft bump centred in the period
                    let d = (p - 0.5) / 0.18;
                    strength * (-d * d).exp()
                }
                _ => 0.0,
            };
            // the column's colour with its share of the pulse mixed in: one blend per pixel
            let col = if bright > 0.01 { mix_color(c, glow, bright.min(1.0)) } else { c };
            // integer blend for the fully covered pixels (most of a band): alpha in 0..=256
            let a8 = col.a as u32 + (col.a as u32 >> 7);
            let inv = 256 - a8;
            let (tr, tg, tb, ta) = (col.r as u32 * a8, col.g as u32 * a8, col.b as u32 * a8, 255 * a8);
            let full_x = xcov >= 0.999;
            for y in (top.floor() as i32).max(0)..(bot.ceil() as i32).min(self.h) {
                let py = y as f32;
                let cov = ((py + 1.0).min(bot) - py.max(top)).clamp(0.0, 1.0) * xcov;
                if cov >= 0.999 && full_x {
                    let i = (y * self.w + x) as usize;
                    let d = self.px[i];
                    let ch = |shift: u32, t: u32| ((t + ((d >> shift) & 0xFF) * inv) >> 8).min(255);
                    self.px[i] = ch(24, ta) << 24 | ch(16, tr) << 16 | ch(8, tg) << 8 | ch(0, tb);
                } else if cov > 0.0 {
                    self.blend_px(x, y, col, cov);
                }
            }
        }
    }

    /// Dashed antialiased curve along the same easing as `fill_ribbon`: a flow whose size is not
    /// known.
    pub fn dashed_curve(&mut self, x0: f32, x1: f32, y0: f32, y1: f32, width: f32, dash: f32, c: Color) {
        let n = ((x1 - x0) / 2.0).ceil().max(1.0) as i32;
        let at = |i: i32| {
            let t = i as f32 / n as f32;
            let s = t * t * (3.0 - 2.0 * t);
            (x0 + (x1 - x0) * t, y0 + (y1 - y0) * s)
        };
        let mut run = 0.0;
        for i in 0..n {
            let ((ax, ay), (bx, by)) = (at(i), at(i + 1));
            let len = (bx - ax).hypot(by - ay);
            if (run / dash) as i32 % 2 == 0 {
                self.draw_line(ax, ay, bx, by, width, c);
            }
            run += len;
        }
    }

    /// Write the canvas over an opaque background as a 32-bit BMP (preview tests).
    #[cfg(test)]
    pub fn save_bmp(&self, path: &std::path::Path, bg: Color) -> std::io::Result<()> {
        let (w, h) = (self.w as u32, self.h as u32);
        let mut out = Vec::with_capacity(54 + (w * h * 4) as usize);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(54 + w * h * 4).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(-(h as i32)).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&[0; 24]);
        for p in &self.px {
            let a = ((p >> 24) & 0xFF) as f32 / 255.0;
            let ch = |s: u32, b: u8| (((p >> s) & 0xFF) as f32 + b as f32 * (1.0 - a)).round().min(255.0) as u8;
            out.extend_from_slice(&[ch(0, bg.b), ch(8, bg.g), ch(16, bg.r), 255]);
        }
        std::fs::write(path, out)
    }

    /// Copy of the pixels in a rectangle (clipped to the canvas), for putting them back later.
    pub fn copy_region(&self, x: i32, y: i32, w: i32, h: i32) -> Vec<u32> {
        let mut out = vec![0u32; (w.max(0) * h.max(0)) as usize];
        for yy in 0..h {
            for xx in 0..w {
                let (sx, sy) = (x + xx, y + yy);
                if sx >= 0 && sy >= 0 && sx < self.w && sy < self.h {
                    out[(yy * w + xx) as usize] = self.px[(sy * self.w + sx) as usize];
                }
            }
        }
        out
    }

    /// Put pixels saved by `copy_region` back.
    pub fn put_region(&mut self, x: i32, y: i32, w: i32, h: i32, px: &[u32]) {
        if px.len() < (w.max(0) * h.max(0)) as usize {
            return;
        }
        // whole rows at once, clipped to the canvas
        let x0 = x.max(0);
        let x1 = (x + w).min(self.w);
        if x1 <= x0 {
            return;
        }
        for yy in 0..h {
            let dy = y + yy;
            if dy < 0 || dy >= self.h {
                continue;
            }
            let src = (yy * w + (x0 - x)) as usize;
            let dst = (dy * self.w + x0) as usize;
            let n = (x1 - x0) as usize;
            self.px[dst..dst + n].copy_from_slice(&px[src..src + n]);
        }
    }

    /// Composite a rectangle of another canvas onto this one at (dx, dy).
    pub fn blit_rect(&mut self, src: &Canvas, sx: i32, sy: i32, w: i32, h: i32, dx: i32, dy: i32) {
        for yy in 0..h {
            let (ssy, ddy) = (sy + yy, dy + yy);
            if ssy < 0 || ssy >= src.h || ddy < 0 || ddy >= self.h {
                continue;
            }
            for xx in 0..w {
                let (ssx, ddx) = (sx + xx, dx + xx);
                if ssx < 0 || ssx >= src.w || ddx < 0 || ddx >= self.w {
                    continue;
                }
                let s = src.px[(ssy * src.w + ssx) as usize];
                let sa = (s >> 24) & 0xFF;
                if sa == 0 {
                    continue;
                }
                let idx = (ddy * self.w + ddx) as usize;
                let d = self.px[idx];
                let inv = 255 - sa;
                let ch = |shift: u32| ((s >> shift) & 0xFF) + ((d >> shift) & 0xFF) * inv / 255;
                self.px[idx] = ch(24).min(255) << 24 | ch(16).min(255) << 16 | ch(8).min(255) << 8 | ch(0).min(255);
            }
        }
    }

    /// Height of the last non-transparent row + 1 (0 when empty).
    pub fn used_height(&self) -> i32 {
        for y in (0..self.h).rev() {
            let row = &self.px[(y * self.w) as usize..((y + 1) * self.w) as usize];
            if row.iter().any(|p| p >> 24 != 0) {
                return y + 1;
            }
        }
        0
    }

    /// Draw text truncated with an ellipsis to fit `max_w`.
    pub fn draw_text_clipped(&mut self, text: &str, x: i32, y: i32, max_w: i32, font: FontSpec, c: Color) -> (i32, i32) {
        if self.measure_text(text, font).0 <= max_w {
            return self.draw_text(text, x, y, font, c);
        }
        let chars: Vec<char> = text.chars().collect();
        let mut n = chars.len();
        while n > 1 {
            n -= 1;
            let s: String = chars[..n].iter().collect::<String>().trim_end().to_string() + "…";
            if self.measure_text(&s, font).0 <= max_w {
                return self.draw_text(&s, x, y, font, c);
            }
        }
        (0, 0)
    }

    pub fn measure_text(&mut self, text: &str, font: FontSpec) -> (i32, i32) {
        self.text.measure(text, font)
    }

    /// Draw text with its top-left corner at (x, y). Returns the rendered size.
    pub fn draw_text(&mut self, text: &str, x: i32, y: i32, font: FontSpec, c: Color) -> (i32, i32) {
        let Some(g) = self.text.render(text, font) else { return (0, 0) };
        for gy in 0..g.h {
            for gx in 0..g.w {
                let cov = g.cov[(gy * g.w + gx) as usize] as f32 / 255.0;
                if cov > 0.0 {
                    self.blend_px(x + gx, y + gy, c, cov);
                }
            }
        }
        (g.w, g.h)
    }

    /// Push the framebuffer to a layered window (keeps the window position).
    pub fn present(&mut self, hwnd: HWND) -> windows::core::Result<()> {
        self.present_impl(hwnd, None)
    }

    /// Push the framebuffer to a top-level layered window and move it to screen (x, y).
    pub fn present_at(&mut self, hwnd: HWND, x: i32, y: i32) -> windows::core::Result<()> {
        self.present_impl(hwnd, Some(POINT { x, y }))
    }

    /// Present only the rect (rx, ry, rw, rh) of the framebuffer (an animation frame): just that
    /// part is copied and handed to the compositor. Falls back to a full present before the
    /// first one or after a resize.
    pub fn present_region(&mut self, hwnd: HWND, x: i32, y: i32, rx: i32, ry: i32, rw: i32, rh: i32) -> windows::core::Result<()> {
        let Some((rx, ry, rw, rh)) = self.clip_region(rx, ry, rw, rh) else { return Ok(()) };
        if self.dib.as_ref().map(|d| d.w != self.w || d.h != self.h).unwrap_or(true) {
            return self.present_at(hwnd, x, y);
        }
        let dib = self.dib.as_mut().unwrap();
        dib.copy_region(&self.px, rx, ry, rw, rh);
        let size = SIZE { cx: self.w, cy: self.h };
        let src = POINT { x: 0, y: 0 };
        let dst = POINT { x, y };
        let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 };
        let dirty = RECT { left: rx, top: ry, right: rx + rw, bottom: ry + rh };
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: &dst,
            psize: &size,
            hdcSrc: dib.hdc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: &dirty,
        };
        unsafe { UpdateLayeredWindowIndirect(hwnd, &info).ok() }
    }

    /// Paint only a rect of the framebuffer onto a window DC (non-layered windows).
    pub fn paint_dc_region(&mut self, hdc: HDC, rx: i32, ry: i32, rw: i32, rh: i32) {
        let Some((rx, ry, rw, rh)) = self.clip_region(rx, ry, rw, rh) else { return };
        if self.dib.as_ref().map(|d| d.w != self.w || d.h != self.h).unwrap_or(true) {
            self.paint_dc(hdc);
            return;
        }
        let dib = self.dib.as_mut().unwrap();
        dib.copy_region(&self.px, rx, ry, rw, rh);
        unsafe {
            let _ = BitBlt(hdc, rx, ry, rw, rh, dib.hdc, rx, ry, SRCCOPY);
        }
    }

    fn clip_region(&self, rx: i32, ry: i32, rw: i32, rh: i32) -> Option<(i32, i32, i32, i32)> {
        let x0 = rx.max(0);
        let y0 = ry.max(0);
        let x1 = (rx + rw).min(self.w);
        let y1 = (ry + rh).min(self.h);
        (x1 > x0 && y1 > y0).then(|| (x0, y0, x1 - x0, y1 - y0))
    }

    /// Copy the framebuffer, alpha included, onto a window DC (a non-layered window whose
    /// client area DWM composites with that alpha over a backdrop).
    pub fn paint_dc(&mut self, hdc: HDC) {
        if self.dib.as_ref().map(|d| d.w != self.w || d.h != self.h).unwrap_or(true) {
            match Dib::new(self.w, self.h) {
                Ok(d) => self.dib = Some(d),
                Err(_) => return,
            }
        }
        let dib = self.dib.as_mut().unwrap();
        dib.copy_from(&self.px);
        unsafe {
            let _ = BitBlt(hdc, 0, 0, self.w, self.h, dib.hdc, 0, 0, SRCCOPY);
        }
    }

    fn present_impl(&mut self, hwnd: HWND, dst: Option<POINT>) -> windows::core::Result<()> {
        if self.dib.as_ref().map(|d| d.w != self.w || d.h != self.h).unwrap_or(true) {
            self.dib = Some(Dib::new(self.w, self.h)?);
        }
        let dib = self.dib.as_mut().unwrap();
        dib.copy_from(&self.px);
        let size = SIZE { cx: self.w, cy: self.h };
        let src = POINT { x: 0, y: 0 };
        // AC_SRC_OVER = 0, AC_SRC_ALPHA = 1
        let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 };
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                dst.as_ref().map(|p| p as *const POINT),
                Some(&size),
                dib.hdc,
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
    }
}

/// Pixel coverage (0..1) of a rounded rectangle, evaluated at the pixel centre with a 1px-wide AA ramp.
fn round_rect_coverage(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32, r: f32) -> f32 {
    if w <= 0 || h <= 0 {
        return 0.0;
    }
    let cx = px as f32 + 0.5;
    let cy = py as f32 + 0.5;
    let left = x as f32;
    let top = y as f32;
    let right = (x + w) as f32;
    let bottom = (y + h) as f32;
    if cx < left || cx > right || cy < top || cy > bottom {
        return 0.0;
    }
    // distance to the nearest corner circle centre, only matters inside the corner squares
    let qx = if cx < left + r { left + r } else if cx > right - r { right - r } else { cx };
    let qy = if cy < top + r { top + r } else if cy > bottom - r { bottom - r } else { cy };
    let dx = cx - qx;
    let dy = cy - qy;
    if dx == 0.0 || dy == 0.0 {
        return 1.0;
    }
    let d = (dx * dx + dy * dy).sqrt();
    (r - d + 0.5).clamp(0.0, 1.0)
}

/// A 32bpp top-down DIB section selected into a memory DC.
struct Dib {
    w: i32,
    h: i32,
    hdc: HDC,
    hbm: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
}

impl Dib {
    fn new(w: i32, h: i32) -> windows::core::Result<Self> {
        unsafe {
            let hdc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbm = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old = SelectObject(hdc, hbm);
            Ok(Self { w, h, hdc, hbm, old, bits: bits as *mut u32 })
        }
    }
    fn copy_from(&mut self, px: &[u32]) {
        unsafe {
            std::ptr::copy_nonoverlapping(px.as_ptr(), self.bits, (self.w * self.h) as usize);
        }
    }

    /// Copy one rect (already inside the bitmap) of a same-sized framebuffer.
    fn copy_region(&mut self, px: &[u32], x: i32, y: i32, w: i32, h: i32) {
        for row in y..y + h {
            let at = (row * self.w + x) as usize;
            unsafe {
                std::ptr::copy_nonoverlapping(px.as_ptr().add(at), self.bits.add(at), w as usize);
            }
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old);
            let _ = DeleteObject(self.hbm);
            let _ = DeleteDC(self.hdc);
        }
    }
}

/// The UI font: Segoe UI Variable (Windows 11) where installed, else Segoe UI. Checked once by
/// asking GDI which face it actually picked for the Variable name.
fn ui_face(hdc: HDC) -> &'static str {
    static FACE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FACE.get_or_init(|| unsafe {
        const VARIABLE: &str = "Segoe UI Variable Text";
        let face = wide(VARIABLE);
        let f = CreateFontW(-12, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_TT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32, (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32, pcwstr(&face));
        let old = SelectObject(hdc, f);
        let mut got = [0u16; 64];
        let n = GetTextFaceW(hdc, Some(&mut got)) as usize;
        SelectObject(hdc, old);
        let _ = DeleteObject(f);
        let picked = String::from_utf16_lossy(&got[..n.saturating_sub(1).min(got.len())]);
        if picked.eq_ignore_ascii_case(VARIABLE) {
            VARIABLE
        } else {
            "Segoe UI"
        }
    })
}

/// Windows' symbol font: Segoe Fluent Icons (Windows 11), else Segoe MDL2 Assets.
fn icon_face(hdc: HDC) -> &'static str {
    static FACE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FACE.get_or_init(|| unsafe {
        const FLUENT: &str = "Segoe Fluent Icons";
        let face = wide(FLUENT);
        let f = CreateFontW(-12, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_TT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32, (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32, pcwstr(&face));
        let old = SelectObject(hdc, f);
        let mut got = [0u16; 64];
        let n = GetTextFaceW(hdc, Some(&mut got)) as usize;
        SelectObject(hdc, old);
        let _ = DeleteObject(f);
        let picked = String::from_utf16_lossy(&got[..n.saturating_sub(1).min(got.len())]);
        if picked.eq_ignore_ascii_case(FLUENT) {
            FLUENT
        } else {
            "Segoe MDL2 Assets"
        }
    })
}

/// Rendered texts kept per (text, font), shared by every canvas of the UI thread.
const TEXT_CACHE_MAX: usize = 4000;
thread_local! {
    static GLYPH_CACHE: std::cell::RefCell<HashMap<(String, FontSpec), Option<std::rc::Rc<Glyphs>>>> = std::cell::RefCell::new(HashMap::new());
    static TEXT_SIZES: std::cell::RefCell<HashMap<(String, FontSpec), (i32, i32)>> = std::cell::RefCell::new(HashMap::new());
}

struct Glyphs {
    w: i32,
    h: i32,
    cov: Vec<u8>,
}

/// Renders text through GDI (grayscale antialiasing) into a coverage mask.
struct TextRenderer {
    hdc: HDC,
    fonts: HashMap<FontSpec, HFONT>,
}

impl TextRenderer {
    fn new() -> Self {
        Self { hdc: unsafe { CreateCompatibleDC(None) }, fonts: HashMap::new() }
    }

    fn font(&mut self, spec: FontSpec) -> HFONT {
        let hdc = self.hdc;
        *self.fonts.entry(spec).or_insert_with(|| unsafe {
            let icons = spec.weight == FW_ICON;
            let face = wide(if icons { icon_face(hdc) } else { ui_face(hdc) });
            CreateFontW(
                -spec.px,
                0,
                0,
                0,
                if icons { FW_NORMAL } else { spec.weight },
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_TT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                // Segoe UI's gasp table forbids grayscale AA at small sizes (text came out jagged);
                // ClearType is allowed at every size, and we average its RGB coverage below.
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                pcwstr(&face),
            )
        })
    }

    /// The size of a text, from the shared cache when it was measured before.
    fn measure(&mut self, text: &str, spec: FontSpec) -> (i32, i32) {
        let key = (text.to_string(), spec);
        if let Some(s) = TEXT_SIZES.with(|m| m.borrow().get(&key).copied()) {
            return s;
        }
        let s = self.measure_uncached(text, spec);
        TEXT_SIZES.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() > TEXT_CACHE_MAX {
                m.clear();
            }
            m.insert(key, s);
        });
        s
    }

    fn measure_uncached(&mut self, text: &str, spec: FontSpec) -> (i32, i32) {
        let font = self.font(spec);
        let t: Vec<u16> = text.encode_utf16().collect();
        let mut sz = SIZE::default();
        unsafe {
            let old = SelectObject(self.hdc, font);
            let _ = GetTextExtentPoint32W(self.hdc, &t, &mut sz);
            SelectObject(self.hdc, old);
        }
        (sz.cx, sz.cy)
    }

    /// The coverage mask of a text, from the shared cache when it was drawn before: a popup
    /// redraws mostly the same labels every second, and GDI rendering is the slow part.
    fn render(&mut self, text: &str, spec: FontSpec) -> Option<std::rc::Rc<Glyphs>> {
        let key = (text.to_string(), spec);
        if let Some(g) = GLYPH_CACHE.with(|m| m.borrow().get(&key).cloned()) {
            return g;
        }
        let g = self.render_uncached(text, spec).map(std::rc::Rc::new);
        GLYPH_CACHE.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() > TEXT_CACHE_MAX {
                m.clear();
            }
            m.insert(key, g.clone());
        });
        g
    }

    fn render_uncached(&mut self, text: &str, spec: FontSpec) -> Option<Glyphs> {
        let (w, h) = self.measure(text, spec);
        if w <= 0 || h <= 0 {
            return None;
        }
        let font = self.font(spec);
        let dib = Dib::new(w, h).ok()?;
        let mut t: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            std::ptr::write_bytes(dib.bits, 0, (w * h) as usize);
            let old = SelectObject(dib.hdc, font);
            SetTextColor(dib.hdc, COLORREF(0x00FF_FFFF));
            SetBkMode(dib.hdc, TRANSPARENT);
            let mut rc = RECT { left: 0, top: 0, right: w, bottom: h };
            DrawTextW(dib.hdc, &mut t, &mut rc, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(dib.hdc, old);
            let src = std::slice::from_raw_parts(dib.bits, (w * h) as usize);
            let cov = src
                .iter()
                .map(|p| {
                    let b = *p & 0xFF;
                    let g = (*p >> 8) & 0xFF;
                    let r = (*p >> 16) & 0xFF;
                    ((r + g + b) / 3) as u8
                })
                .collect();
            Some(Glyphs { w, h, cov })
        }
    }
}

impl Drop for TextRenderer {
    fn drop(&mut self) {
        unsafe {
            for (_, f) in self.fonts.drain() {
                let _ = DeleteObject(f);
            }
            let _ = DeleteDC(self.hdc);
        }
    }
}

#[cfg(test)]
mod anim_bench {
    use super::*;

    /// `cargo test --release --bin winstats anim_frame_cost -- --ignored --nocapture`: the work
    /// of one battery-flow frame (restore, five bands, compose) on a canvas of the real size.
    #[test]
    #[ignore]
    fn anim_frame_cost() {
        let (w, h) = (1170, 300);
        let mut content = Canvas::new();
        content.resize(w, h);
        content.clear();
        content.fill_rect(0, 0, w, h, Color::rgba(0x20, 0x20, 0x20, 255));
        let _ = content.copy_region(0, 0, w, h);
        let mut window = Canvas::new();
        window.resize(w + 40, h + 40);
        window.clear();
        let wbg = window.copy_region(20, 20, w, h);
        let c = Color::rgba(0xFC, 0xB0, 0x40, 200);
        // like the diagram: one wide band in, four out
        let bands: [(f32, f32, (f32, f32), (f32, f32)); 5] = [
            (150.0, 495.0, (100.0, 200.0), (100.0, 200.0)),
            (630.0, 975.0, (110.0, 150.0), (20.0, 60.0)),
            (630.0, 975.0, (150.0, 155.0), (95.0, 100.0)),
            (630.0, 975.0, (155.0, 200.0), (160.0, 205.0)),
            (630.0, 975.0, (200.0, 260.0), (230.0, 290.0)),
        ];
        let n = 300;
        let (mut paint, mut compose) = (0.0f64, 0.0f64);
        for i in 0..n {
            let t0 = std::time::Instant::now();
            // the window under the bands copied back, the bands drawn straight onto it
            window.put_region(20, 20, w, h, &wbg);
            for (x0, x1, l, r) in bands {
                window.fill_ribbon(x0 + 20.0, x1 + 20.0, (l.0 + 20.0, l.1 + 20.0), (r.0 + 20.0, r.1 + 20.0), c, Some(((i as f32 * 0.01) % 1.0, 60.0, 0.55)));
            }
            let t1 = std::time::Instant::now();
            compose += t1.elapsed().as_secs_f64();
            paint += (t1 - t0).as_secs_f64();
        }
        let (p, c) = (paint * 1000.0 / n as f64, compose * 1000.0 / n as f64);
        println!("per frame: paint {p:.2} ms, compose {c:.2} ms; at 64 fps that is {:.1} % of one core", (p + c) * 64.0 / 10.0);
    }
}
