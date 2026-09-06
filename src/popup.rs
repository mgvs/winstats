//! Click popup: a top-level layered window anchored to the clicked widget
//! showing details for that module. Content is drawn with the same Canvas as the strip.

use crate::canvas::{Canvas, Color, FW_NORMAL, FW_SEMIBOLD, FontSpec, GREEN, RED, YELLOW};
use crate::metrics::Metrics;
use crate::util::{format_bytes, format_bytes_rate, format_duration, pcwstr, wide};
use crate::i18n::{t, tf};
use crate::widgets::{Kind, Theme};
use std::time::Instant;
use windows::Win32::Foundation::{HINSTANCE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

const WIDTH_DIP: f32 = 300.0;
const PAD_DIP: f32 = 14.0;
const RADIUS_DIP: f32 = 10.0;

pub const DOWN: Color = Color::rgb(0x2E, 0x9C, 0xF6);
pub const UP: Color = Color::rgb(0xF4, 0x43, 0x36);
pub const READ: Color = Color::rgb(0x9C, 0x6A, 0xF6);
pub const WRITE: Color = Color::rgb(0xFF, 0x98, 0x00);

/// Geometry of the last drawn pie chart, in window coordinates, for hover hit-testing.
#[derive(Clone, Default)]
pub struct PieGeom {
    cx: f32,
    cy: f32,
    r: f32,
    /// (start, end) angles per slice, radians from 12 o'clock clockwise
    slices: Vec<(f32, f32)>,
}

/// What a click on a popup row does.
#[derive(Clone, Debug)]
pub enum Action {
    /// include / exclude a network interface (by alias) in the widget and the chart
    ToggleNet(String),
    /// include / exclude a physical disk (by model)
    ToggleDisk(String),
}

/// A clickable row rectangle, in content coords.
#[derive(Clone)]
struct Hit {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    action: Action,
}

/// A label that did not fit and was drawn with an ellipsis; hovering it shows the full text.
#[derive(Clone)]
struct Tip {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: FontSpec,
    text: String,
}

pub struct Popup {
    pub hwnd: HWND,
    /// hovered pie slice, highlighted on the next render
    pub hover: Option<usize>,
    pie: Option<PieGeom>,
    /// truncated labels of the last render, in content coords
    tips: Vec<Tip>,
    /// clickable rows of the last render, in content coords
    hits: Vec<Hit>,
    /// hovered truncated label, drawn as a tooltip on the next render
    tip: Option<usize>,
    /// content offset inside the window (set by render)
    pad: i32,
    canvas: Canvas,
    content: Canvas,
    pub kind: Option<Kind>,
    pub visible: bool,
    pub hidden_at: Option<Instant>,
    anchor: RECT,
    tray: RECT,
    light: bool,
    /// height fixed while the popup stays open, so content changes do not make it jump
    locked_h: i32,
}

impl Popup {
    pub fn new(hinstance: HINSTANCE, class: &str) -> Option<Self> {
        let cls = wide(class);
        let title = wide("winstats popup");
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                pcwstr(&cls),
                pcwstr(&title),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                hinstance,
                None,
            )
            .ok()?
        };
        Some(Self {
            hwnd,
            canvas: Canvas::new(),
            content: Canvas::new(),
            kind: None,
            hover: None,
            pie: None,
            tips: Vec::new(),
            hits: Vec::new(),
            tip: None,
            pad: 0,
            visible: false,
            hidden_at: None,
            anchor: RECT::default(),
            tray: RECT::default(),
            light: false,
            locked_h: 0,
        })
    }

    /// Called from the strip's click handler. `anchor` and `tray` are screen rects.
    pub fn toggle(&mut self, kind: Kind, anchor: RECT, tray: RECT) -> bool {
        let just_hidden = self.hidden_at.map(|t| t.elapsed().as_millis() < 300).unwrap_or(false);
        if self.visible && self.kind == Some(kind) {
            self.hide("toggle");
            return false;
        }
        if !self.visible && just_hidden && self.kind == Some(kind) {
            // the click that opened us also deactivated the popup: treat as toggle-off
            return false;
        }
        self.kind = Some(kind);
        self.anchor = anchor;
        self.tray = tray;
        self.visible = true;
        self.locked_h = 0;
        self.hover = None;
        self.pie = None;
        self.tips.clear();
        self.hits.clear();
        self.tip = None;
        true
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<&Hit> {
        let (cx, cy) = (x - self.pad, y - self.pad);
        self.hits.iter().find(|h| cx >= h.x && cx < h.x + h.w && cy >= h.y && cy < h.y + h.h)
    }

    /// Something clickable under (x, y) in window coords.
    pub fn click_target(&self, x: i32, y: i32) -> bool {
        self.hit_at(x, y).is_some()
    }

    /// The action of the row under (x, y), if any.
    pub fn click(&self, x: i32, y: i32) -> Option<Action> {
        self.hit_at(x, y).map(|h| h.action.clone())
    }

    pub fn hide(&mut self, reason: &str) {
        if self.visible {
            let _ = reason;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
            self.hidden_at = Some(Instant::now());
        }
    }

    /// Mouse moved over the popup (window coords): returns true when the hovered slice or
    /// truncated label changed.
    pub fn mouse_move(&mut self, x: i32, y: i32) -> bool {
        let (cx, cy) = (x - self.pad, y - self.pad);
        let tip = self.tips.iter().position(|t| cx >= t.x && cx < t.x + t.w && cy >= t.y && cy < t.y + t.h);
        let mut changed = false;
        if tip != self.tip {
            self.tip = tip;
            changed = true;
        }
        let hit = self.pie.as_ref().and_then(|p| {
            let dx = x as f32 + 0.5 - p.cx;
            let dy = y as f32 + 0.5 - p.cy;
            if (dx * dx + dy * dy).sqrt() > p.r + 2.0 {
                return None;
            }
            let mut a = dx.atan2(-dy);
            if a < 0.0 {
                a += std::f32::consts::PI * 2.0;
            }
            p.slices.iter().position(|(s, e)| a >= *s && a < *e)
        });
        if hit != self.hover {
            self.hover = hit;
            changed = true;
        }
        changed
    }

    /// Full text of the hovered truncated label in a small box above it (below when there is no
    /// room), drawn onto the content canvas.
    fn draw_tooltip(&mut self, t: &Theme, light: bool, content_w: i32) {
        let Some(tip) = self.tip.and_then(|i| self.tips.get(i)).cloned() else { return };
        let px = t.s(7.0);
        let py = t.s(4.0);
        // word-wrap to the popup width: the window cannot grow for the tooltip
        let max_w = content_w - px * 2;
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        for word in tip.text.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if line.is_empty() || self.content.measure_text(&candidate, tip.font).0 <= max_w {
                line = candidate;
            } else {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            }
        }
        lines.push(line);
        let th = self.content.measure_text("X", tip.font).1;
        let tw = lines.iter().map(|l| self.content.measure_text(l, tip.font).0).max().unwrap_or(0);
        let w = (tw + px * 2).min(content_w);
        let h = th * lines.len() as i32 + py * 2;
        let gap = t.s(3.0);
        let x = tip.x.min(content_w - w).max(0);
        let y = if tip.y - h - gap >= 0 { tip.y - h - gap } else { tip.y + tip.h + gap };
        let bg = if light { Color::rgb(0xFF, 0xFF, 0xFF) } else { Color::rgb(0x2C, 0x2C, 0x2C) };
        let r = t.s(4.0) as f32;
        self.content.fill_round_rect(x, y, w, h, r, bg);
        self.content.stroke_round_rect_r(x, y, w, h, r, t.dim.alpha(0.5));
        for (i, l) in lines.iter().enumerate() {
            self.content.draw_text_clipped(l, x + px, y + py + th * i as i32, w - px * 2, tip.font, t.text);
        }
    }

    pub fn render(&mut self, t: &Theme, light: bool, m: &Metrics, first: bool) {
        let Some(kind) = self.kind else { return };
        if !self.visible {
            return;
        }
        self.light = light;
        let w = t.s(WIDTH_DIP);
        let pad = t.s(PAD_DIP);

        // pass 1: content on a tall transparent canvas
        self.content.resize(w - pad * 2, t.s(900.0));
        self.content.clear();
        let mut ui = Ui { c: &mut self.content, t, w: w - pad * 2, y: 0, hover: self.hover, pie: None, tips: Vec::new(), hits: Vec::new() };
        match kind {
            Kind::Cpu => draw_cpu(&mut ui, m),
            Kind::Mem => draw_mem(&mut ui, m),
            Kind::Gpu => draw_gpu(&mut ui, m),
            Kind::Net => draw_net(&mut ui, m),
            Kind::Disk => draw_disk(&mut ui, m),
            Kind::Battery => draw_battery(&mut ui, m),
        }
        // layout cursor, not painted pixels: reserved (empty) rows count too
        let end_y = ui.y;
        // pie geometry from content coords to window coords
        self.pie = ui.pie.take().map(|mut p| {
            p.cx += pad as f32;
            p.cy += pad as f32;
            p
        });
        self.tips = std::mem::take(&mut ui.tips);
        self.hits = std::mem::take(&mut ui.hits);
        self.pad = pad;
        if self.tip.map_or(false, |i| i >= self.tips.len()) {
            self.tip = None;
        }
        self.draw_tooltip(t, light, w - pad * 2);
        let content_h = end_y.max(self.content.used_height()).max(t.s(40.0));
        // never shrink while open; grow only if content really needs more room
        let h = (content_h + pad * 2).max(self.locked_h);
        self.locked_h = h;

        // pass 2: background + content
        self.canvas.resize(w, h);
        self.canvas.clear();
        let bg = if light { Color::rgba(0xF9, 0xF9, 0xF9, 244) } else { Color::rgba(0x20, 0x20, 0x20, 244) };
        let r = t.s(RADIUS_DIP) as f32;
        self.canvas.fill_round_rect(0, 0, w, h, r, bg);
        self.canvas.stroke_round_rect_r(0, 0, w, h, r, t.dim.alpha(0.35));
        self.canvas.blit(&self.content, pad, pad);

        // position: above (or below) the taskbar, centred on the clicked widget
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = unsafe {
            let hm = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let _ = GetMonitorInfoW(hm, &mut mi);
            mi.rcMonitor
        };
        let margin = t.s(8.0);
        let cx = (self.anchor.left + self.anchor.right) / 2;
        let x = (cx - w / 2).clamp(mon.left + margin, (mon.right - margin - w).max(mon.left + margin));
        let y = if self.tray.top > mon.top + 10 { self.tray.top - h - margin } else { self.tray.bottom + margin };

        unsafe {
            let flags = if first { SWP_SHOWWINDOW } else { SWP_SHOWWINDOW | SWP_NOACTIVATE };
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w, h, flags);
            if let Err(e) = self.canvas.present_at(self.hwnd, x, y) {
                crate::log!("popup UpdateLayeredWindow failed: {e}");
            }
            if first {
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
    }
}

// ---------------------------------------------------------------- layout helper

struct Ui<'a> {
    c: &'a mut Canvas,
    t: &'a Theme,
    w: i32,
    y: i32,
    hover: Option<usize>,
    pie: Option<PieGeom>,
    tips: Vec<Tip>,
    hits: Vec<Hit>,
}

impl Ui<'_> {
    fn font(&self, dip: f32, weight: i32) -> FontSpec {
        FontSpec { px: self.t.s(dip), weight }
    }

    /// Text clipped to `max_w` with an ellipsis; a clipped label is remembered so hovering it
    /// shows the full text.
    fn clipped(&mut self, text: &str, x: i32, y: i32, max_w: i32, font: FontSpec, c: Color) -> (i32, i32) {
        let (tw, th) = self.c.measure_text(text, font);
        if tw > max_w {
            self.tips.push(Tip { x, y, w: max_w.max(1), h: th, font, text: text.to_string() });
        }
        self.c.draw_text_clipped(text, x, y, max_w, font, c)
    }

    fn space(&mut self, dip: f32) {
        self.y += self.t.s(dip);
    }

    fn title(&mut self, title: &str, subtitle: &str) {
        let f = self.font(15.0, FW_SEMIBOLD);
        let (_, h) = self.c.draw_text(title, 0, self.y, f, self.t.text);
        self.y += h;
        if !subtitle.is_empty() {
            let f = self.font(9.5, FW_NORMAL);
            let (_, h) = self.clipped(subtitle, 0, self.y, self.w, f, self.t.dim);
            self.y += h;
        }
        self.space(10.0);
    }

    fn section(&mut self, name: &str) {
        self.space(8.0);
        let f = self.font(9.5, FW_SEMIBOLD);
        let (_, h) = self.c.draw_text(&name.to_uppercase(), 0, self.y, f, self.t.dim);
        self.y += h + self.t.s(4.0);
    }

    fn row(&mut self, label: &str, value: &str) {
        self.row_colored(label, value, self.t.text);
    }

    fn row_colored(&mut self, label: &str, value: &str, color: Color) {
        self.row_lv(label, value, self.t.dim, color);
    }

    /// Row with explicit label and value colours.
    fn row_lv(&mut self, label: &str, value: &str, label_color: Color, color: Color) {
        let f = self.font(11.0, FW_NORMAL);
        let (vw, mut h) = self.c.measure_text(value, f);
        if value.is_empty() {
            // label-only row: take the height from the label, an empty string measures 0
            h = self.c.measure_text(label, f).1;
        }
        self.clipped(label, 0, self.y, self.w - vw - self.t.s(8.0), f, label_color);
        self.c.draw_text(value, self.w - vw, self.y, f, color);
        self.y += h + self.t.s(3.0);
    }

    /// Make the rows drawn since `from_y` react to a click.
    fn clickable(&mut self, from_y: i32, action: Action) {
        self.hits.push(Hit { x: 0, y: from_y, w: self.w, h: self.y - from_y, action });
    }

    /// Text colour for a row that can be switched off: full when on, faded when off.
    fn on_off(&self, base: Color, on: bool) -> Color {
        if on {
            base
        } else {
            base.alpha(0.45)
        }
    }

    /// label | bar | value
    fn bar_row(&mut self, label: &str, frac: f32, value: &str, color: Color) {
        let f = self.font(10.5, FW_NORMAL);
        let (vw, h) = self.c.measure_text(value, f);
        let label_w = self.t.s(110.0);
        let value_w = self.t.s(64.0).max(vw);
        let bar_x = label_w + self.t.s(6.0);
        let bar_w = self.w - bar_x - value_w - self.t.s(8.0);
        let bar_h = self.t.s(6.0);
        let by = self.y + (h - bar_h) / 2;
        self.clipped(label, 0, self.y, label_w, f, self.t.text);
        self.c.fill_round_rect(bar_x, by, bar_w, bar_h, bar_h as f32 / 2.0, self.t.dim.alpha(0.18));
        let fw = (bar_w as f32 * frac.clamp(0.0, 1.0)).round() as i32;
        if fw > 0 {
            self.c.fill_round_rect(bar_x, by, fw.max(bar_h), bar_h, bar_h as f32 / 2.0, color);
        }
        self.c.draw_text(value, self.w - vw, self.y, f, self.t.dim);
        self.y += h + self.t.s(4.0);
    }

    fn chart_box(&mut self, h_dip: f32) -> (i32, i32, i32, i32) {
        let h = self.t.s(h_dip);
        let r = self.t.s(4.0) as f32;
        self.c.fill_round_rect(0, self.y, self.w, h, r, self.t.dim.alpha(0.08));
        self.c.stroke_round_rect_r(0, self.y, self.w, h, r, self.t.dim.alpha(0.25));
        for q in [0.25f32, 0.5, 0.75] {
            let gy = self.y + (h as f32 * q).round() as i32;
            self.c.fill_rect(1, gy, self.w - 2, 1, self.t.dim.alpha(0.12));
        }
        let rect = (1, self.y + 1, self.w - 2, h - 2);
        self.y += h + self.t.s(6.0);
        rect
    }

    /// Single 0..1 series.
    fn chart(&mut self, values: &[f32], h_dip: f32, color: Color) {
        let (x, y, w, h) = self.chart_box(h_dip);
        self.c.line_chart(x, y, w, h, values, color, color.alpha(0.3));
    }

    /// Two series sharing an automatic scale (bytes/s etc).
    fn chart2(&mut self, a: &[f32], b: &[f32], h_dip: f32, ca: Color, cb: Color, floor: f32) -> f32 {
        let max = a.iter().chain(b.iter()).cloned().fold(floor, f32::max);
        let na: Vec<f32> = a.iter().map(|v| v / max).collect();
        let nb: Vec<f32> = b.iter().map(|v| v / max).collect();
        let (x, y, w, h) = self.chart_box(h_dip);
        self.c.line_chart(x, y, w, h, &na, ca, ca.alpha(0.3));
        self.c.line_chart(x, y, w, h, &nb, cb, cb.alpha(0.3));
        max
    }

    fn legend(&mut self, items: &[(&str, Color)]) {
        let f = self.font(9.5, FW_NORMAL);
        let mut x = 0;
        let mut h = 0;
        for (label, col) in items {
            let r = self.t.s(3.0) as f32;
            let (_, th) = self.c.measure_text(label, f);
            h = th;
            self.c.fill_circle(x as f32 + r, self.y as f32 + th as f32 / 2.0, r, *col);
            x += self.t.s(9.0);
            let (tw, _) = self.c.draw_text(label, x, self.y, f, self.t.dim);
            x += tw + self.t.s(12.0);
        }
        self.y += h + self.t.s(4.0);
    }

    /// Per-core bars in a box; E-cores drawn lighter, separated from P-cores by a gap.
    fn cores(&mut self, cores: &[f32], classes: &[u8], breaks_at: &[bool]) {
        let n = cores.len().max(1) as i32;
        let (x, y, w, h) = self.chart_box(56.0);
        let gap = self.t.s(2.0);
        let pad = self.t.s(4.0);
        let breaks = if self.t.split_pe { breaks_at.iter().filter(|b| **b).count() as i32 } else { 0 };
        let extra = self.t.s(4.0);
        let per_break = 2 * extra + 1 + gap;
        let bw = ((w - pad * 2 - gap * (n - 1) - breaks * per_break) / n).max(1);
        let total = bw * n + gap * (n - 1) + breaks * per_break;
        let mut bx = x + (w - total) / 2;
        let max_h = (h - pad * 2) as f32;
        let top = classes.iter().cloned().max().unwrap_or(0);
        for (i, v) in cores.iter().enumerate() {
            let class = classes.get(i).cloned().unwrap_or(top);
            if self.t.split_pe && breaks_at.get(i).cloned().unwrap_or(false) {
                self.c.fill_rect(bx + extra, y + pad, 1, max_h as i32, self.t.dim.alpha(0.35));
                bx += extra + 1 + extra + gap;
            }
            let mut col = self.t.chart_color(Kind::Cpu, *v);
            if class != top {
                col = col.alpha(0.6);
            }
            self.c.fill_rect(bx, y + pad, bw, max_h as i32, self.t.dim.alpha(0.1));
            self.c.fill_bar(bx, y + h - pad, bw, (v * max_h).max(1.0), col);
            bx += bw + gap;
        }
    }

    /// Pie chart on the left, colour legend with values on the right. The hovered slice is
    /// drawn lighter and slightly larger, its legend row in the text colour.
    fn pie(&mut self, slices: &[(String, u64, Color)]) {
        let total: u64 = slices.iter().map(|s| s.1).sum();
        if total == 0 {
            return;
        }
        let d = self.t.s(84.0);
        let r = d as f32 / 2.0;
        let cx = r + self.t.s(2.0) as f32;
        let cy = self.y as f32 + r + self.t.s(2.0) as f32;
        let tau = std::f32::consts::PI * 2.0;
        let mut a = 0.0f32;
        let mut geom = PieGeom { cx, cy, r, slices: Vec::new() };
        for (i, (_, v, col)) in slices.iter().enumerate() {
            let span = tau * (*v as f32 / total as f32);
            let end = if i + 1 == slices.len() { tau } else { a + span };
            let hovered = self.hover == Some(i);
            let (rr, c) = if hovered { (r + self.t.s(3.0) as f32, lighten(*col)) } else { (r, *col) };
            if end > a {
                self.c.fill_pie(cx, cy, rr, a, end, c);
            }
            geom.slices.push((a, end));
            a = end;
        }
        // thin separators between slices
        for (s, _) in geom.slices.iter().skip(1) {
            let ex = cx + s.sin() * r;
            let ey = cy - s.cos() * r;
            let steps = (r as i32).max(1);
            for k in 0..=steps {
                let f = k as f32 / steps as f32;
                self.c.fill_rect((cx + (ex - cx) * f) as i32, (cy + (ey - cy) * f) as i32, 1, 1, self.t.dim.alpha(0.25));
            }
        }
        // legend
        let lf = self.font(10.0, FW_NORMAL);
        let lx = d + self.t.s(16.0);
        let mut ly = self.y + self.t.s(2.0);
        let sw = self.t.s(8.0);
        for (i, (label, v, col)) in slices.iter().enumerate() {
            let (_, th) = self.c.measure_text(label, lf);
            self.c.fill_round_rect(lx, ly + (th - sw) / 2, sw, sw, 2.0, *col);
            let pct = (*v as f64 / total as f64 * 100.0).round() as i32;
            let value = format!("{} ({pct}%)", format_bytes(*v));
            let (vw, _) = self.c.measure_text(&value, lf);
            let colour = if self.hover == Some(i) { self.t.text } else { self.t.dim };
            self.clipped(label, lx + sw + self.t.s(6.0), ly, self.w - lx - sw - self.t.s(6.0) - vw - self.t.s(6.0), lf, colour);
            self.c.draw_text(&value, self.w - vw, ly, lf, self.t.text);
            ly += th + self.t.s(4.0);
        }
        self.y = (self.y + d + self.t.s(6.0)).max(ly + self.t.s(2.0));
        self.pie = Some(geom);
    }

    /// Process list; always takes the room of `n` rows so the popup height stays stable.
    fn processes(&mut self, list: &[crate::metrics::processes::ProcInfo], mode: ProcMode, n: usize) {
        let row_h = match mode {
            ProcMode::Cpu => self.c.measure_text("X", self.font(10.5, FW_NORMAL)).1 + self.t.s(4.0),
            _ => self.c.measure_text("X", self.font(11.0, FW_NORMAL)).1 + self.t.s(3.0),
        };
        if list.is_empty() {
            let f = self.font(10.5, FW_NORMAL);
            let text = if mode == ProcMode::Io { t("popup.nothing_io") } else { t("popup.collecting") };
            self.c.draw_text(&text, 0, self.y, f, self.t.dim);
            self.y += row_h * n as i32;
            return;
        }
        for p in list.iter().take(n) {
            match mode {
                ProcMode::Cpu => self.bar_row(&p.name, p.cpu, &format!("{:.1}%", p.cpu * 100.0), self.t.accent),
                ProcMode::Mem => self.row_colored(&p.name, &format_bytes(p.mem), self.t.text),
                ProcMode::Io => self.row_colored(&p.name, &format_bytes_rate(p.io), self.t.text),
            }
        }
        if list.len() < n {
            self.y += row_h * (n - list.len()) as i32;
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ProcMode {
    Cpu,
    Mem,
    Io,
}

fn lighten(c: Color) -> Color {
    Color::rgba(
        c.r as u16 as u8 + ((255 - c.r as u16) * 3 / 10) as u8,
        c.g as u16 as u8 + ((255 - c.g as u16) * 3 / 10) as u8,
        c.b as u16 as u8 + ((255 - c.b as u16) * 3 / 10) as u8,
        c.a,
    )
}

fn pct(v: f32) -> String {
    format!("{}%", (v * 100.0).round() as i32)
}

// ---------------------------------------------------------------- modules

fn draw_cpu(ui: &mut Ui, m: &Metrics) {
    ui.title(&t("popup.cpu"), &m.cpu_name);
    let hist: Vec<f32> = m.cpu_history.iter().cloned().collect();
    ui.chart(&hist, 70.0, ui.t.accent);
    ui.row_colored(&t("popup.load"), &pct(m.cpu_total), ui.t.usage(m.cpu_total, ui.t.text));
    ui.row(&t("popup.system"), &pct(m.cpu_system));
    ui.row(&t("popup.user"), &pct(m.cpu_user));
    ui.row(&t("popup.idle"), &pct((1.0 - m.cpu_total).max(0.0)));
    if m.cpu_sockets > 1 {
        ui.row(&t("popup.sockets"), &m.cpu_sockets.to_string());
    }
    let sockets = if m.cpu_sockets > 1 { tf("popup.sockets_suffix", &[("n", &m.cpu_sockets.to_string())]) } else { String::new() };
    if m.cpu_hybrid() {
        let (pn, en, pl, el) = m.cpu_pe();
        ui.row(&tf("popup.p_cores", &[("n", &pn.to_string())]), &pct(pl));
        ui.row(&tf("popup.e_cores", &[("n", &en.to_string())]), &pct(el));
        ui.section(&tf("popup.cores_pe", &[("p", &pn.to_string()), ("e", &en.to_string()), ("sockets", &sockets)]));
    } else {
        ui.section(&tf("popup.cores", &[("n", &m.cpu_cores.len().to_string()), ("sockets", &sockets)]));
    }
    let breaks = m.cpu_breaks();
    ui.cores(&m.cpu_cores, &m.cpu_classes, &breaks);
    ui.section(&t("popup.top_processes"));
    ui.processes(&m.top_cpu, ProcMode::Cpu, m.top_n);
}

fn draw_mem(ui: &mut Ui, m: &Metrics) {
    let total = tf("popup.total", &[("size", &format_bytes(m.mem.total))]);
    let mut sub = match &m.mem_summary {
        Some(s) => format!("{total} \u{00B7} {s}"),
        None => total,
    };
    if let Some(n) = m.mem_channels {
        sub.push_str(&format!(" \u{00B7} {}", tf("popup.channels", &[("n", &n.to_string())])));
    }
    ui.title(&t("popup.memory"), &sub);
    let hist: Vec<f32> = m.mem_history.iter().cloned().collect();
    ui.chart(&hist, 70.0, ui.t.accent);
    ui.row_colored(&t("popup.usage"), &pct(m.mem.pct), ui.t.usage(m.mem.pct, ui.t.text));
    ui.row(&t("popup.used"), &format_bytes(m.mem.used));
    ui.row(&t("popup.free"), &format_bytes(m.mem.total.saturating_sub(m.mem.used)));
    ui.row(&t("popup.committed"), &format!("{} / {}", format_bytes(m.mem.commit_used), format_bytes(m.mem.commit_total)));
    if m.mem_processes > 0 {
        // where the physical memory goes: process private sets, kernel pools, the rest of "used"
        // (driver-locked pages such as VM memory, shared/mapped pages, hardware reserved) and
        // what is still available (the standby file cache lives in there)
        let kernel = m.mem_kernel_paged + m.mem_kernel_nonpaged;
        let other = m.mem.used.saturating_sub(m.mem_processes).saturating_sub(kernel);
        let available = m.mem.total.saturating_sub(m.mem.used);
        ui.section(&t("popup.where_it_goes"));
        ui.pie(&[
            (t("popup.processes_private"), m.mem_processes, ui.t.accent),
            (t("popup.kernel_pools"), kernel, Color::rgb(0xAF, 0x52, 0xDE)),
            (t("popup.other_memory"), other, Color::rgb(0xFF, 0x95, 0x00)),
            (t("popup.available_memory"), available, ui.t.dim.alpha(0.45)),
        ]);
        ui.row(&t("popup.file_cache"), &format_bytes(m.mem_cache));
    }
    ui.section(&t("popup.top_processes"));
    ui.processes(&m.top_mem, ProcMode::Mem, m.top_n);
}

fn draw_gpu(ui: &mut Ui, m: &Metrics) {
    ui.title(&t("popup.gpu"), &m.gpu_name);
    let hist: Vec<f32> = m.gpu_history.iter().cloned().collect();
    ui.chart(&hist, 70.0, ui.t.accent);
    match m.gpu {
        Some(v) => ui.row_colored(&t("popup.load"), &pct(v), ui.t.usage(v, ui.t.text)),
        None => ui.row(&t("popup.load"), &t("popup.na")),
    }
    if m.vram_budget > 0 {
        let frac = m.vram_used as f32 / m.vram_budget as f32;
        ui.section(&t("popup.video_memory"));
        ui.bar_row(&t("popup.used"), frac, &format_bytes(m.vram_used), ui.t.accent);
        ui.row(&t("popup.budget"), &format_bytes(m.vram_budget));
    }
    if m.vram_total > 0 {
        ui.row(&t("popup.dedicated"), &format_bytes(m.vram_total));
    }
    if let Some(a) = &m.gpu_primary {
        ui.section(&t("popup.adapter"));
        let vendor = a.vendor();
        let ids = format!("{:04X}:{:04X}", a.vendor_id, a.device_id);
        ui.row(&t("popup.vendor_device"), &if vendor.is_empty() { ids } else { format!("{vendor} {ids}") });
        if !a.driver.is_empty() {
            ui.row(&t("popup.driver"), &a.driver);
        }
    }
    if m.gpu_adapters.len() > 1 {
        ui.section(&t("popup.all_adapters"));
        for a in &m.gpu_adapters {
            ui.row(&a.name, &if a.dedicated > 0 { format_bytes(a.dedicated) } else { t("popup.shared") });
        }
    }
}

fn draw_net(ui: &mut Ui, m: &Metrics) {
    let sub = if m.net_ip.is_empty() { m.net_iface.clone() } else { format!("{} · {}", m.net_iface, m.net_ip) };
    ui.title(&t("popup.network"), &sub);
    let rx: Vec<f32> = m.net_history.iter().map(|v| v.0).collect();
    let tx: Vec<f32> = m.net_history.iter().map(|v| v.1).collect();
    let max = ui.chart2(&rx, &tx, 70.0, DOWN, UP, 10.0 * 1024.0);
    let (down, up) = (t("popup.download"), t("popup.upload"));
    ui.legend(&[(&down, DOWN), (&up, UP), (&tf("popup.scale", &[("rate", &format_bytes_rate(max as f64))]), ui.t.dim.alpha(0.0))]);
    ui.row_colored(&down, &format_bytes_rate(m.net_rx), DOWN);
    ui.row_colored(&up, &format_bytes_rate(m.net_tx), UP);
    ui.section(&t("popup.since_start"));
    ui.row(&t("popup.received"), &format_bytes(m.net_total_rx));
    ui.row(&t("popup.sent"), &format_bytes(m.net_total_tx));
    if !m.net_ifaces.is_empty() {
        ui.section(&t("popup.interfaces"));
        for i in &m.net_ifaces {
            let start = ui.y;
            // "Wi-Fi · HomeNet"  |  12.3 KB/s / 1.2 KB/s
            let mut name = i.alias.clone();
            if !i.ssid.is_empty() {
                name.push_str(&format!(" \u{00B7} {}", i.ssid));
            }
            let value = if i.up { format!("{} / {}", format_bytes_rate(i.rx_bps), format_bytes_rate(i.tx_bps)) } else { t("popup.disconnected") };
            let (lc, vc) = (ui.on_off(ui.t.text, i.enabled), ui.on_off(ui.t.text, i.enabled));
            ui.row_lv(&name, &value, lc, if i.up { vc } else { ui.on_off(ui.t.dim, i.enabled) });
            // hardware name · kind · IPv4
            let mut desc = i.description.clone();
            if !i.kind.is_empty() && !i.alias.to_lowercase().contains(&i.kind.to_lowercase()) {
                desc.push_str(&format!(" \u{00B7} {}", i.kind));
            }
            if !i.ip.is_empty() {
                desc.push_str(&format!(" \u{00B7} {}", i.ip));
            }
            let dc = ui.on_off(ui.t.dim, i.enabled);
            ui.row_lv(&format!("    {desc}"), "", dc, dc);
            ui.clickable(start, Action::ToggleNet(i.alias.clone()));
        }
    }
}

fn draw_disk(ui: &mut Ui, m: &Metrics) {
    let sub = match m.disks.len() {
        0 => String::new(),
        1 => m.disks[0].model.clone(),
        n => tf("popup.physical_disks_n", &[("n", &n.to_string())]),
    };
    ui.title(&t("popup.disk"), &sub);
    let r: Vec<f32> = m.disk_history.iter().map(|v| v.0).collect();
    let w: Vec<f32> = m.disk_history.iter().map(|v| v.1).collect();
    let max = ui.chart2(&r, &w, 70.0, READ, WRITE, 1024.0 * 1024.0);
    let (read, write) = (t("popup.read"), t("popup.write"));
    ui.legend(&[(&read, READ), (&write, WRITE), (&tf("popup.scale", &[("rate", &format_bytes_rate(max as f64))]), ui.t.dim.alpha(0.0))]);
    ui.row_colored(&read, &format_bytes_rate(m.disk_read), READ);
    ui.row_colored(&write, &format_bytes_rate(m.disk_write), WRITE);
    if !m.disks.is_empty() {
        ui.section(&t("popup.physical_disks"));
        for d in &m.disks {
            let mut desc = String::new();
            if !d.bus.is_empty() {
                desc.push_str(d.bus);
            }
            if d.size > 0 {
                if !desc.is_empty() {
                    desc.push_str(" \u{00B7} ");
                }
                desc.push_str(&format_bytes(d.size));
            }
            let start = ui.y;
            let (lc, vc) = (ui.on_off(ui.t.text, d.enabled), ui.on_off(ui.t.dim, d.enabled));
            ui.row_lv(&d.model, &desc, lc, vc);
            let rw = format!("{} / {}", format_bytes_rate(d.read_bps), format_bytes_rate(d.write_bps));
            ui.row_lv(&format!("    {}", t("popup.read_write")), &rw, vc, ui.on_off(ui.t.text, d.enabled));
            ui.clickable(start, Action::ToggleDisk(d.model.clone()));
        }
    }
    ui.section(&t("popup.top_processes"));
    ui.processes(&m.top_io, ProcMode::Io, m.top_n);
    ui.section(&t("popup.drives"));
    for d in &m.drives {
        let used = d.total.saturating_sub(d.free);
        let frac = used as f32 / d.total.max(1) as f32;
        let mut label = if d.label.is_empty() { format!("{}:", d.letter) } else { format!("{}: {}", d.letter, d.label) };
        // "(USB, exFAT)", "(NTFS)", "(DVD, UDF)"
        let tags: Vec<&str> = [d.kind, d.fs.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
        if !tags.is_empty() {
            label.push_str(&format!(" ({})", tags.join(", ")));
        }
        if d.readonly {
            // nothing can be freed on read-only media: only the capacity
            ui.row(&label, &tf("popup.total", &[("size", &format_bytes(d.total))]));
            continue;
        }
        let col = if frac > 0.9 { RED } else if frac > 0.8 { YELLOW } else { ui.t.accent };
        ui.bar_row(&label, frac, &format_bytes(d.free), col);
    }
}

fn draw_battery(ui: &mut Ui, m: &Metrics) {
    let Some(b) = m.battery else {
        ui.title(&t("popup.battery"), &t("popup.no_battery"));
        return;
    };
    let state = if b.charging {
        t("popup.charging")
    } else if b.on_ac {
        t("popup.plugged_in")
    } else {
        t("popup.discharging")
    };
    ui.title(&t("popup.battery"), &state);
    let col = if b.charging || b.on_ac {
        GREEN
    } else if b.level < 0.15 {
        RED
    } else if b.level < 0.3 {
        YELLOW
    } else {
        ui.t.accent
    };
    ui.bar_row(&t("popup.level"), b.level, &pct(b.level), col);
    if let Some(s) = b.remaining_secs {
        ui.row(&t("popup.time_remaining"), &format_duration(s));
    }
    if let Some(d) = m.battery_detail {
        ui.section(&t("popup.details"));
        if d.rate_mw != 0 {
            ui.row(&t("popup.power"), &format!("{:.1} W", d.rate_mw.abs() as f32 / 1000.0));
        }
        if d.max_mwh > 0 {
            ui.row(&t("popup.capacity"), &format!("{:.1} / {:.1} Wh", d.remaining_mwh as f32 / 1000.0, d.max_mwh as f32 / 1000.0));
        }
    }
}
