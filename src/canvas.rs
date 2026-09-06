//! Premultiplied BGRA framebuffer with simple primitives, GDI-rendered text,
//! and presentation through UpdateLayeredWindow (per-pixel alpha).

use crate::util::{pcwstr, wide};
use std::collections::HashMap;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};

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

    /// Composite another (premultiplied) canvas onto this one at (x, y).
    pub fn blit(&mut self, src: &Canvas, x: i32, y: i32) {
        for sy in 0..src.h {
            let dy = y + sy;
            if dy < 0 || dy >= self.h {
                continue;
            }
            for sx in 0..src.w {
                let dx = x + sx;
                if dx < 0 || dx >= self.w {
                    continue;
                }
                let s = src.px[(sy * src.w + sx) as usize];
                let sa = (s >> 24) & 0xFF;
                if sa == 0 {
                    continue;
                }
                let idx = (dy * self.w + dx) as usize;
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
        *self.fonts.entry(spec).or_insert_with(|| unsafe {
            let face = wide("Segoe UI");
            CreateFontW(
                -spec.px,
                0,
                0,
                0,
                spec.weight,
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

    fn measure(&mut self, text: &str, spec: FontSpec) -> (i32, i32) {
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

    fn render(&mut self, text: &str, spec: FontSpec) -> Option<Glyphs> {
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
