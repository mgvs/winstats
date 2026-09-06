//! Taskbar widgets: BarChart, LineChart, Mini, Speed, DualChart, Battery.
//! Every widget draws into a horizontal strip of height `Theme::inner_h`.

use crate::canvas::{Canvas, Color, FontSpec, FW_NORMAL, FW_SEMIBOLD, usage_color, GREEN, RED, YELLOW};
use crate::metrics::Metrics;
use crate::util::{format_bytes_rate, format_size};

/// How a module is coloured: accent, by utilisation, monochrome, or a fixed colour.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ColorMode {
    /// charts in the accent colour, text values in the plain text colour
    Accent,
    /// green -> yellow -> orange -> red by value
    Utilization,
    /// everything in the text colour
    Mono,
    Fixed(Color),
}

impl ColorMode {
    pub fn parse(s: &str) -> ColorMode {
        match s {
            "accent" | "" => ColorMode::Accent,
            "utilization" => ColorMode::Utilization,
            "mono" => ColorMode::Mono,
            other => named_color(other).map(ColorMode::Fixed).unwrap_or(ColorMode::Accent),
        }
    }
}

pub struct Theme {
    pub text: Color,
    pub dim: Color,
    pub accent: Color,
    pub light: bool,
    pub scale: f32,
    pub inner_h: i32,
    pub labels: bool,
    /// multiplier for widget text (Font size menu)
    pub font_scale: f32,
    /// draw a gap + divider between P and E cores
    pub split_pe: bool,
    /// width of chart widgets in DIP
    pub chart_w: f32,
    /// colour mode per module, indexed like MODULES
    pub modes: [ColorMode; 6],
}

impl Theme {
    /// DIP -> physical pixels
    pub fn s(&self, dip: f32) -> i32 {
        (dip * self.scale).round().max(1.0) as i32
    }
    /// Widget text font, scaled by the Font size setting.
    fn font(&self, dip: f32, weight: i32) -> FontSpec {
        FontSpec { px: self.s(dip * self.font_scale), weight }
    }
    /// Fixed-size font (vertical labels).
    fn font_fixed(&self, dip: f32, weight: i32) -> FontSpec {
        FontSpec { px: self.s(dip), weight }
    }

    pub fn mode(&self, k: Kind) -> ColorMode {
        self.modes[MODULES.iter().position(|m| *m == k).unwrap_or(0)]
    }

    /// Utilisation colour, darker shades on a light taskbar so yellow/orange stay readable.
    pub fn usage(&self, v: f32, base: Color) -> Color {
        if v < 0.6 {
            base
        } else if self.light {
            if v < 0.75 {
                Color::rgb(0xB8, 0x86, 0x00)
            } else if v < 0.9 {
                Color::rgb(0xE0, 0x5A, 0x00)
            } else {
                Color::rgb(0xD3, 0x2F, 0x2F)
            }
        } else {
            usage_color(v, base)
        }
    }

    /// Colour for chart geometry (bars, lines) of a module at value `v` (0..1).
    pub fn chart_color(&self, k: Kind, v: f32) -> Color {
        match self.mode(k) {
            ColorMode::Accent => self.accent,
            ColorMode::Utilization => self.usage(v, self.accent),
            ColorMode::Mono => self.text,
            ColorMode::Fixed(c) => c,
        }
    }

    /// Colour for a text value of a module.
    pub fn value_color(&self, k: Kind, v: f32) -> Color {
        match self.mode(k) {
            ColorMode::Accent | ColorMode::Mono => self.text,
            ColorMode::Utilization => self.usage(v, self.text),
            ColorMode::Fixed(c) => c,
        }
    }
}

// ---------------------------------------------------------------- palette

pub const BLUE: Color = Color::rgb(0x2E, 0x7C, 0xF6);

/// (config name, i18n key of the menu title, colour). "system" is resolved from the Windows accent colour.
pub const PALETTE: &[(&str, &str, Color)] = &[
    ("system", "colour.system", BLUE),
    ("blue", "colour.blue", BLUE),
    ("green", "colour.green", Color::rgb(0x34, 0xC7, 0x59)),
    ("red", "colour.red", Color::rgb(0xFF, 0x3B, 0x30)),
    ("orange", "colour.orange", Color::rgb(0xFF, 0x95, 0x00)),
    ("yellow", "colour.yellow", Color::rgb(0xFF, 0xCC, 0x00)),
    ("purple", "colour.purple", Color::rgb(0xAF, 0x52, 0xDE)),
    ("pink", "colour.pink", Color::rgb(0xFF, 0x2D, 0x55)),
    ("teal", "colour.teal", Color::rgb(0x30, 0xB0, 0xC7)),
    ("cyan", "colour.cyan", Color::rgb(0x32, 0xAD, 0xE6)),
    ("indigo", "colour.indigo", Color::rgb(0x58, 0x56, 0xD6)),
    ("white", "colour.white", Color::rgb(0xFF, 0xFF, 0xFF)),
    ("black", "colour.black", Color::rgb(0x00, 0x00, 0x00)),
];

/// Palette name or "#RRGGBB" -> colour.
pub fn named_color(name: &str) -> Option<Color> {
    if name == "system" {
        return Some(crate::util::system_accent_color().unwrap_or(BLUE));
    }
    if let Some((_, _, c)) = PALETTE.iter().find(|(n, _, _)| *n == name) {
        return Some(*c);
    }
    Color::from_hex(name)
}

/// Module a widget belongs to (one popup per kind).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Cpu,
    Gpu,
    Mem,
    Disk,
    Net,
    Battery,
}

impl Kind {
    /// Translated module name for the menu.
    pub fn title(self) -> String {
        crate::i18n::t(match self {
            Kind::Cpu => "module.cpu",
            Kind::Gpu => "module.gpu",
            Kind::Mem => "module.ram",
            Kind::Disk => "module.disk",
            Kind::Net => "module.network",
            Kind::Battery => "module.battery",
        })
    }
}

/// Every available widget, in module order. (kind, config name, i18n key of the menu title)
pub const CATALOG: &[(Kind, &str, &str)] = &[
    (Kind::Cpu, "cpu_bars", "widget.bar_chart"),
    (Kind::Cpu, "cpu_line", "widget.line_chart"),
    (Kind::Cpu, "cpu_mini", "widget.mini"),
    (Kind::Gpu, "gpu", "widget.mini"),
    (Kind::Gpu, "gpu_line", "widget.line_chart"),
    (Kind::Mem, "mem", "widget.mini"),
    (Kind::Mem, "mem_line", "widget.line_chart"),
    (Kind::Mem, "mem_text", "widget.used_free"),
    (Kind::Disk, "disk", "widget.speed"),
    (Kind::Disk, "disk_chart", "widget.chart"),
    (Kind::Net, "net", "widget.speed"),
    (Kind::Net, "net_chart", "widget.chart"),
    (Kind::Battery, "battery", "widget.battery"),
];

pub const MODULES: &[Kind] = &[Kind::Cpu, Kind::Gpu, Kind::Mem, Kind::Disk, Kind::Net, Kind::Battery];

fn catalog_index(name: &str) -> Option<usize> {
    CATALOG.iter().position(|(_, n, _)| *n == name)
}

/// Enable or disable a widget in the config list, keeping widgets of one module together.
pub fn toggle_widget(list: &mut Vec<String>, name: &str) {
    if let Some(i) = list.iter().position(|n| n == name) {
        list.remove(i);
        return;
    }
    let Some(idx) = catalog_index(name) else { return };
    let kind = CATALOG[idx].0;
    if let Some(pos) = list.iter().rposition(|n| catalog_index(n).map(|j| CATALOG[j].0 == kind).unwrap_or(false)) {
        list.insert(pos + 1, name.to_string());
        return;
    }
    let pos = list
        .iter()
        .position(|n| catalog_index(n).map(|j| j > idx).unwrap_or(false))
        .unwrap_or(list.len());
    list.insert(pos, name.to_string());
}

/// True for a name the builder understands.
pub fn is_known(name: &str) -> bool {
    catalog_index(name).is_some()
}

/// Drop unknown names so config indices line up with the built widget list.
pub fn normalize(list: &mut Vec<String>) {
    list.retain(|n| {
        let ok = is_known(n);
        if !ok {
            crate::log!("unknown widget '{n}' in config, dropped");
        }
        ok
    });
}

/// Move every widget of `kind` one module block to the left (-1) or right (+1).
pub fn move_module(list: &mut Vec<String>, kind: Kind, dir: i32) {
    // blocks of consecutive entries by module, in list order
    let mut blocks: Vec<(Kind, Vec<String>)> = Vec::new();
    for n in list.iter() {
        let Some(i) = catalog_index(n) else { continue };
        let k = CATALOG[i].0;
        match blocks.last_mut() {
            Some((bk, names)) if *bk == k => names.push(n.clone()),
            _ => blocks.push((k, vec![n.clone()])),
        }
    }
    // merge scattered blocks of the same module into the first one so the move is predictable
    let mut merged: Vec<(Kind, Vec<String>)> = Vec::new();
    for (k, names) in blocks {
        if let Some((_, existing)) = merged.iter_mut().find(|(bk, _)| *bk == k) {
            existing.extend(names);
        } else {
            merged.push((k, names));
        }
    }
    let Some(pos) = merged.iter().position(|(k, _)| *k == kind) else { return };
    let target = pos as i32 + dir;
    if target < 0 || target >= merged.len() as i32 {
        return;
    }
    merged.swap(pos, target as usize);
    *list = merged.into_iter().flat_map(|(_, names)| names).collect();
}

pub trait Widget {
    fn kind(&self) -> Kind;
    fn width(&self, c: &mut Canvas, t: &Theme, m: &Metrics) -> i32;
    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics);
}

pub fn build(names: &[String]) -> Vec<Box<dyn Widget>> {
    let mut out: Vec<Box<dyn Widget>> = Vec::new();
    for n in names {
        match n.as_str() {
            "cpu_bars" => out.push(Box::new(CpuBars)),
            "cpu_line" => out.push(Box::new(Line(Kind::Cpu))),
            "cpu_mini" => out.push(Box::new(Mini(Kind::Cpu))),
            "gpu" => out.push(Box::new(Mini(Kind::Gpu))),
            "gpu_line" => out.push(Box::new(Line(Kind::Gpu))),
            "mem" => out.push(Box::new(Mini(Kind::Mem))),
            "mem_line" => out.push(Box::new(Line(Kind::Mem))),
            "mem_text" => out.push(Box::new(MemText)),
            "disk" => out.push(Box::new(Speed(Kind::Disk))),
            "disk_chart" => out.push(Box::new(DualChart(Kind::Disk))),
            "net" => out.push(Box::new(Speed(Kind::Net))),
            "net_chart" => out.push(Box::new(DualChart(Kind::Net))),
            "battery" => out.push(Box::new(BatteryWidget)),
            other => crate::log!("unknown widget '{other}' in config"),
        }
    }
    out
}

// ---------------------------------------------------------------- label column

const LABEL_DIP: f32 = 6.0;

fn label_width(t: &Theme) -> i32 {
    if t.labels { t.s(LABEL_DIP) + t.s(2.0) } else { 0 }
}

/// Three letters stacked vertically in a narrow column.
fn draw_label(c: &mut Canvas, x: i32, y: i32, t: &Theme, label: &str) {
    if !t.labels {
        return;
    }
    let f = t.font_fixed(6.5, FW_SEMIBOLD);
    let chars: Vec<char> = label.chars().take(3).collect();
    let slot = t.inner_h as f32 / chars.len().max(1) as f32;
    for (i, ch) in chars.iter().enumerate() {
        let s = ch.to_string();
        let (w, h) = c.measure_text(&s, f);
        let cx = x + (t.s(LABEL_DIP) - w) / 2;
        let cy = y + (slot * i as f32 + (slot - h as f32) / 2.0).round() as i32;
        c.draw_text(&s, cx, cy, f, t.dim);
    }
}

fn short_label(k: Kind) -> &'static str {
    match k {
        Kind::Cpu => "CPU",
        Kind::Gpu => "GPU",
        Kind::Mem => "RAM",
        Kind::Disk => "DSK",
        Kind::Net => "NET",
        Kind::Battery => "BAT",
    }
}

/// 0..1 value of a module, if it has one.
fn value(k: Kind, m: &Metrics) -> Option<f32> {
    match k {
        Kind::Cpu => Some(m.cpu_total),
        Kind::Gpu => m.gpu,
        Kind::Mem => Some(m.mem.pct),
        _ => None,
    }
}

fn history(k: Kind, m: &Metrics) -> Vec<f32> {
    match k {
        Kind::Cpu => m.cpu_history.iter().cloned().collect(),
        Kind::Gpu => m.gpu_history.iter().cloned().collect(),
        Kind::Mem => m.mem_history.iter().cloned().collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------- CPU bar chart

pub struct CpuBars;

fn bar_geometry(t: &Theme, n: usize) -> (i32, i32, i32) {
    // (bar width, gap, padding) in px
    let bw_dip = match n {
        0..=4 => 5.0,
        5..=8 => 4.0,
        9..=16 => 3.0,
        17..=32 => 2.0,
        _ => 1.0,
    };
    (t.s(bw_dip), t.s(1.0), t.s(2.0))
}


impl Widget for CpuBars {
    fn kind(&self) -> Kind {
        Kind::Cpu
    }
    fn width(&self, _c: &mut Canvas, t: &Theme, m: &Metrics) -> i32 {
        let n = m.cpu_cores.len().max(1);
        let (bw, gap, pad) = bar_geometry(t, n);
        // each P|E boundary adds: extra space, 1px divider, extra space, plus a normal gap after it
        let extra = t.s(2.0);
        let breaks = if t.split_pe { m.cpu_breaks().iter().filter(|b| **b).count() as i32 } else { 0 };
        label_width(t) + pad * 2 + n as i32 * bw + (n as i32 - 1) * gap + breaks * (2 * extra + 1 + gap) + 2
    }

    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics) {
        draw_label(c, x, y, t, "CPU");
        let x = x + label_width(t);
        let n = m.cpu_cores.len().max(1);
        let (bw, gap, pad) = bar_geometry(t, n);
        let box_w = self.width(c, t, m) - label_width(t);
        c.stroke_round_rect(x, y, box_w, t.inner_h, t.dim.alpha(0.7));
        let bottom = y + t.inner_h - 1 - pad;
        let max_h = (t.inner_h - 2 - pad * 2) as f32;
        let top_class = m.cpu_classes.iter().cloned().max().unwrap_or(0);
        let breaks_at = m.cpu_breaks();
        let mut bx = x + 1 + pad;
        for (i, v) in m.cpu_cores.iter().enumerate() {
            let class = m.cpu_classes.get(i).cloned().unwrap_or(top_class);
            if t.split_pe && breaks_at.get(i).cloned().unwrap_or(false) {
                // P|E boundary: gap, extra, divider, extra, gap (symmetric around the line)
                let extra = t.s(2.0);
                c.fill_rect(bx + extra, y + 1 + pad, 1, max_h as i32, t.dim.alpha(0.35));
                bx += extra + 1 + extra + gap;
            }
            let mut col = t.chart_color(Kind::Cpu, *v);
            if class != top_class {
                col = col.alpha(0.6); // E-cores lighter
            }
            // faint track so idle cores stay visible
            c.fill_rect(bx, y + 1 + pad, bw, max_h as i32, t.dim.alpha(0.12));
            c.fill_bar(bx, bottom, bw, (v * max_h).max(1.0), col);
            bx += bw + gap;
        }
    }
}

// ---------------------------------------------------------------- line chart (CPU / GPU / RAM)

pub struct Line(pub Kind);

impl Widget for Line {
    fn kind(&self) -> Kind {
        self.0
    }
    fn width(&self, _c: &mut Canvas, t: &Theme, _m: &Metrics) -> i32 {
        label_width(t) + t.s(t.chart_w)
    }

    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics) {
        draw_label(c, x, y, t, short_label(self.0));
        let x = x + label_width(t);
        let w = t.s(t.chart_w);
        c.stroke_round_rect(x, y, w, t.inner_h, t.dim.alpha(0.7));
        let vals = history(self.0, m);
        let col = t.chart_color(self.0, vals.last().cloned().unwrap_or(0.0));
        c.line_chart(x + 1, y + 1, w - 2, t.inner_h - 2, &vals, col, col.alpha(0.35));
    }
}

// ---------------------------------------------------------------- Mini (label above value)

pub struct Mini(pub Kind);

fn pct_text(v: Option<f32>) -> String {
    match v {
        Some(v) => format!("{}%", (v * 100.0).round() as i32),
        None => "--".into(),
    }
}

impl Widget for Mini {
    fn kind(&self) -> Kind {
        self.0
    }
    fn width(&self, c: &mut Canvas, t: &Theme, m: &Metrics) -> i32 {
        let label = short_label(self.0);
        let v = value(self.0, m);
        let lw = c.measure_text(label, t.font(7.0, FW_NORMAL)).0;
        let vw = c.measure_text(&pct_text(v), t.font(10.0, FW_SEMIBOLD)).0;
        let reserve = c.measure_text("100%", t.font(10.0, FW_SEMIBOLD)).0;
        lw.max(vw).max(reserve)
    }

    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics) {
        let label = short_label(self.0);
        let v = value(self.0, m);
        let w = self.width(c, t, m);
        let lf = t.font(7.0, FW_NORMAL);
        let vf = t.font(10.0, FW_SEMIBOLD);
        let (lw, lh) = c.measure_text(label, lf);
        let text = pct_text(v);
        let (vw, vh) = c.measure_text(&text, vf);
        let total = lh + vh - t.s(2.0);
        let top = y + (t.inner_h - total) / 2;
        c.draw_text(label, x + (w - lw) / 2, top, lf, t.dim);
        let col = match v {
            Some(v) => t.value_color(self.0, v),
            None => t.dim,
        };
        c.draw_text(&text, x + (w - vw) / 2, top + lh - t.s(2.0), vf, col);
    }
}

// ---------------------------------------------------------------- Speed (two rows with dots)

pub struct Speed(pub Kind);

pub const DOWN: Color = Color::rgb(0x2E, 0x9C, 0xF6);
pub const UP: Color = Color::rgb(0xF4, 0x43, 0x36);
pub const READ: Color = Color::rgb(0x9C, 0x6A, 0xF6);
pub const WRITE: Color = Color::rgb(0xFF, 0x98, 0x00);

/// Colours of the two series (download/upload, read/write) under the module colour mode.
fn pair_colors(k: Kind, t: &Theme) -> (Color, Color) {
    let default = if k == Kind::Disk { (READ, WRITE) } else { (DOWN, UP) };
    match t.mode(k) {
        ColorMode::Accent | ColorMode::Utilization => default,
        ColorMode::Mono => (t.text, t.dim),
        ColorMode::Fixed(c) => (c, default.1),
    }
}

fn pair_now(k: Kind, m: &Metrics, t: &Theme) -> [(f64, Color); 2] {
    let (a, b) = pair_colors(k, t);
    match k {
        Kind::Disk => [(m.disk_read, a), (m.disk_write, b)],
        _ => [(m.net_rx, a), (m.net_tx, b)],
    }
}

fn pair_history(k: Kind, m: &Metrics, t: &Theme) -> (Vec<f32>, Vec<f32>, Color, Color, f32) {
    let (a, b) = pair_colors(k, t);
    match k {
        Kind::Disk => (
            m.disk_history.iter().map(|v| v.0).collect(),
            m.disk_history.iter().map(|v| v.1).collect(),
            a,
            b,
            1024.0 * 1024.0,
        ),
        _ => (
            m.net_history.iter().map(|v| v.0).collect(),
            m.net_history.iter().map(|v| v.1).collect(),
            a,
            b,
            10.0 * 1024.0,
        ),
    }
}

impl Widget for Speed {
    fn kind(&self) -> Kind {
        self.0
    }
    fn width(&self, c: &mut Canvas, t: &Theme, m: &Metrics) -> i32 {
        let f = t.font(8.5, FW_NORMAL);
        let reserve = c.measure_text("88.8 MB/s", f).0;
        let cur = pair_now(self.0, m, t).iter().map(|(v, _)| c.measure_text(&format_bytes_rate(*v), f).0).max().unwrap_or(0);
        label_width(t) + t.s(4.0) + t.s(3.0) + reserve.max(cur)
    }

    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics) {
        draw_label(c, x, y, t, short_label(self.0));
        let x = x + label_width(t);
        let f = t.font(8.5, FW_NORMAL);
        let rows = pair_now(self.0, m, t);
        let row_h = t.inner_h as f32 / 2.0;
        let dot_r = t.s(4.0) as f32 / 2.0;
        let text_x = x + t.s(4.0) + t.s(3.0);
        for (i, (v, col)) in rows.iter().enumerate() {
            let cy = y as f32 + row_h * i as f32 + row_h / 2.0;
            let active = *v > 1024.0;
            c.fill_circle(x as f32 + dot_r, cy, dot_r, if active { *col } else { t.dim.alpha(0.35) });
            let s = format_bytes_rate(*v);
            let (_, th) = c.measure_text(&s, f);
            c.draw_text(&s, text_x, (cy - th as f32 / 2.0).round() as i32, f, t.text);
        }
    }
}

// ---------------------------------------------------------------- Memory text (used over free)

pub struct MemText;

impl MemText {
    fn rows(m: &Metrics) -> [String; 2] {
        [format_size(m.mem.used), format_size(m.mem.total.saturating_sub(m.mem.used))]
    }
}

impl Widget for MemText {
    fn kind(&self) -> Kind {
        Kind::Mem
    }
    fn width(&self, c: &mut Canvas, t: &Theme, m: &Metrics) -> i32 {
        let f = t.font(9.5, FW_SEMIBOLD);
        let reserve = c.measure_text("88.88 GB", f).0;
        let cur = Self::rows(m).iter().map(|v| c.measure_text(v, f).0).max().unwrap_or(0);
        label_width(t) + reserve.max(cur)
    }

    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics) {
        draw_label(c, x, y, t, "RAM");
        let x = x + label_width(t);
        let f = t.font(9.5, FW_SEMIBOLD);
        let w = self.width(c, t, m) - label_width(t);
        let row_h = t.inner_h as f32 / 2.0;
        for (i, v) in Self::rows(m).iter().enumerate() {
            let cy = y as f32 + row_h * i as f32 + row_h / 2.0;
            let (vw, vh) = c.measure_text(v, f);
            let col = if i == 0 { t.value_color(Kind::Mem, m.mem.pct) } else { t.text };
            c.draw_text(v, x + (w - vw), (cy - vh as f32 / 2.0).round() as i32, f, col);
        }
    }
}

// ---------------------------------------------------------------- dual chart (network / disk)

pub struct DualChart(pub Kind);

impl Widget for DualChart {
    fn kind(&self) -> Kind {
        self.0
    }
    fn width(&self, _c: &mut Canvas, t: &Theme, _m: &Metrics) -> i32 {
        label_width(t) + t.s(t.chart_w)
    }

    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics) {
        draw_label(c, x, y, t, short_label(self.0));
        let x = x + label_width(t);
        let w = t.s(t.chart_w);
        c.stroke_round_rect(x, y, w, t.inner_h, t.dim.alpha(0.7));
        let (a, b, ca, cb, floor) = pair_history(self.0, m, t);
        let max = a.iter().chain(b.iter()).cloned().fold(floor, f32::max);
        let na: Vec<f32> = a.iter().map(|v| v / max).collect();
        let nb: Vec<f32> = b.iter().map(|v| v / max).collect();
        c.line_chart(x + 1, y + 1, w - 2, t.inner_h - 2, &na, ca, ca.alpha(0.35));
        c.line_chart(x + 1, y + 1, w - 2, t.inner_h - 2, &nb, cb, cb.alpha(0.35));
    }
}

// ---------------------------------------------------------------- Battery

pub struct BatteryWidget;

impl Widget for BatteryWidget {
    fn kind(&self) -> Kind {
        Kind::Battery
    }
    fn width(&self, c: &mut Canvas, t: &Theme, m: &Metrics) -> i32 {
        if m.battery.is_none() {
            return 0;
        }
        let f = t.font(9.0, FW_NORMAL);
        c.measure_text("100%", f).0 + t.s(3.0) + t.s(22.0)
    }

    fn draw(&self, c: &mut Canvas, x: i32, y: i32, t: &Theme, m: &Metrics) {
        let Some(b) = m.battery else { return };
        let f = t.font(9.0, FW_NORMAL);
        let text = format!("{}%", (b.level * 100.0).round() as i32);
        let (tw, th) = c.measure_text(&text, f);
        c.draw_text(&text, x, y + (t.inner_h - th) / 2, f, t.text);
        let bx = x + tw + t.s(3.0);
        let bw = t.s(20.0);
        let bh = t.s(10.0);
        let by = y + (t.inner_h - bh) / 2;
        c.stroke_round_rect(bx, by, bw, bh, t.dim);
        c.fill_rect(bx + bw, by + bh / 2 - t.s(1.5), t.s(1.5), t.s(3.0), t.dim); // nub
        let fill_col = if b.charging || b.on_ac {
            GREEN
        } else if b.level < 0.15 {
            RED
        } else if b.level < 0.3 {
            YELLOW
        } else {
            t.text
        };
        let inner = bw - 4;
        let fw = ((inner as f32) * b.level).round().max(1.0) as i32;
        c.fill_rect(bx + 2, by + 2, fw, bh - 4, fill_col);
    }
}
