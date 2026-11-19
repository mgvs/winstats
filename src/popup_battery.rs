//! Battery popup: the power-flow diagram (adapter -> battery + system, or battery -> system),
//! the battery's own readings, and the charge / power history.
//!
//! Every number says what it is: plain = measured by a sensor, "calc." = derived from measured
//! values, "—" with "not reported" = the hardware does not tell. See `metrics::powerflow`.

use super::{head, pct, sub, Action, Hit, Ui};
use crate::canvas::{Canvas, Color, FontSpec, FW_NORMAL, FW_SEMIBOLD};
use crate::i18n::{t, tf};
use crate::metrics::power::{Cell, HistPoint, MeterRole, SourceKind, Telemetry, HIST_STEP};
use crate::metrics::powerflow::{self, Flow, Mode, Reading};
use crate::metrics::MetricsData;
use crate::util::format_duration;

/// One animated band of the diagram, in content coordinates.
#[derive(Clone, Debug)]
pub struct Band {
    x0: f32,
    x1: f32,
    left: (f32, f32),
    right: (f32, f32),
    color: Color,
    /// pulse periods per second; negative runs right to left; 0 = still
    speed: f32,
    period: f32,
}

/// The animated part of the diagram: the bands and the rectangle that holds them.
#[derive(Clone, Debug)]
pub struct FlowAnim {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    bands: Vec<Band>,
}

impl FlowAnim {
    /// Anything to animate at all.
    pub fn moving(&self) -> bool {
        self.bands.iter().any(|b| b.speed != 0.0)
    }
}

/// The bands' motion across renders: every band keeps its own pulse position (a new reading
/// changes the speed from here on, never the position), and a change of shape glides from the
/// old bands to the new ones instead of snapping.
pub struct FlowMotion {
    /// pulse position of each band, in periods
    phases: Vec<f32>,
    last: std::time::Instant,
    /// the bands as last painted, and the shape the current glide starts from
    shown: Option<FlowAnim>,
    from: Option<FlowAnim>,
    since: std::time::Instant,
}

impl Default for FlowMotion {
    fn default() -> Self {
        let now = std::time::Instant::now();
        Self { phases: Vec::new(), last: now, shown: None, from: None, since: now }
    }
}

/// How long a change of shape glides.
const GLIDE_SECS: f32 = 0.6;

impl FlowMotion {
    /// New bands from a render: glide from what is on screen when they match it.
    pub fn retarget(&mut self, to: &FlowAnim) {
        self.from = self.shown.clone().filter(|s| s.bands.len() == to.bands.len() && (s.x, s.y, s.w, s.h) == (to.x, to.y, to.w, to.h));
        self.since = std::time::Instant::now();
    }

    /// A glide is running (frames are needed even when nothing pulses).
    pub fn gliding(&self) -> bool {
        self.from.is_some() && self.since.elapsed().as_secs_f32() < GLIDE_SECS
    }
}

fn lerp(a: f32, b: f32, k: f32) -> f32 {
    a + (b - a) * k
}

/// Draw the bands at the motion's current state, moving it on by the time since the last frame.
/// `off` shifts them from content to canvas coordinates (drawn straight into the window).
pub fn paint_smooth(c: &mut Canvas, a: &FlowAnim, m: &mut FlowMotion, off: (f32, f32)) {
    let now = std::time::Instant::now();
    // a long gap (popup hidden, a stall) does not throw the pulses forward
    let dt = now.duration_since(m.last).as_secs_f32().min(0.1);
    m.last = now;
    m.phases.resize(a.bands.len(), 0.0);
    // ease-out over the glide
    let k = if m.from.is_some() { (m.since.elapsed().as_secs_f32() / GLIDE_SECS).min(1.0) } else { 1.0 };
    let k = 1.0 - (1.0 - k).powi(3);
    let mut shown = a.clone();
    for (i, b) in a.bands.iter().enumerate() {
        m.phases[i] = (m.phases[i] + dt * b.speed).rem_euclid(1.0);
        let g = match m.from.as_ref().and_then(|f| f.bands.get(i)) {
            Some(f) if k < 1.0 => Band {
                x0: lerp(f.x0, b.x0, k),
                x1: lerp(f.x1, b.x1, k),
                left: (lerp(f.left.0, b.left.0, k), lerp(f.left.1, b.left.1, k)),
                right: (lerp(f.right.0, b.right.0, k), lerp(f.right.1, b.right.1, k)),
                ..b.clone()
            },
            _ => b.clone(),
        };
        // the pulse pattern stays where it is: it is measured from the band's own start
        let pulse = (g.speed != 0.0).then(|| (m.phases[i], g.period, 0.55));
        let (dx, dy) = off;
        c.fill_ribbon(g.x0 + dx, g.x1 + dx, (g.left.0 + dy, g.left.1 + dy), (g.right.0 + dy, g.right.1 + dy), g.color, pulse);
        shown.bands[i] = g;
    }
    if k >= 1.0 {
        m.from = None;
    }
    m.shown = Some(shown);
}

/// Draw the bands at animation time `secs`.
pub fn paint_bands(c: &mut Canvas, a: &FlowAnim, secs: f32) {
    for b in &a.bands {
        let pulse = (b.speed != 0.0).then(|| ((secs * b.speed).rem_euclid(1.0), b.period, 0.55));
        c.fill_ribbon(b.x0, b.x1, b.left, b.right, b.color, pulse);
    }
}

/// Metric colours: battery charging, battery discharging, system draw. Brighter on dark.
struct Palette {
    charge: Color,
    discharge: Color,
    system: Color,
}

fn palette(light: bool) -> Palette {
    if light {
        Palette { charge: Color::rgb(0x10, 0x89, 0x3E), discharge: Color::rgb(0xC2, 0x5E, 0x00), system: Color::rgb(0x00, 0x5F, 0xB8) }
    } else {
        Palette { charge: Color::rgb(0x6C, 0xCB, 0x5F), discharge: Color::rgb(0xFC, 0xB0, 0x40), system: Color::rgb(0x60, 0xCD, 0xFF) }
    }
}

/// "58.18 W": two decimals like the fuel gauge's mW resolution allows, fewer for big numbers.
fn watts(w: f32) -> String {
    if w.abs() >= 100.0 {
        format!("{w:.0} W")
    } else {
        format!("{w:.2} W")
    }
}

/// "31.4 W": one decimal, for figures that are estimates or bounds
fn watts1(w: f32) -> String {
    format!("{w:.1} W")
}

/// The battery as a remote agent or the plain Windows API describe it, for machines without
/// the power worker.
fn basic_telemetry(m: &MetricsData) -> Telemetry {
    let mut t = Telemetry::default();
    if let Some(b) = m.battery {
        t.level = Some(b.level);
        t.on_ac = Some(b.on_ac);
        t.remaining_secs = b.remaining_secs;
    }
    if let Some(d) = m.battery_detail {
        let known = |v: u32| (v > 0).then_some(v);
        t.cells.push(Cell {
            rate_mw: (d.rate_mw != 0 || m.battery.map_or(false, |b| b.on_ac)).then_some(d.rate_mw),
            capacity_mwh: known(d.remaining_mwh),
            full_mwh: known(d.max_mwh),
            design_mwh: known(d.design_mwh),
            cycles: d.cycles,
            charging: m.battery.map_or(false, |b| b.charging),
            ..Default::default()
        });
    }
    t
}

/// The chart window: the history and consumption charts sharing `chart_h` DIP of chart height.
pub(super) fn chart_page(ui: &mut Ui, m: &MetricsData, chart_h: f32) {
    let full_wh = m.power_tel.as_ref().and_then(|tel| {
        let f: u32 = tel.cells.iter().filter_map(|c| c.full_mwh).sum();
        (f > 0).then(|| f as f32 / 1000.0)
    });
    let now = m.power_tel.as_ref().map_or(0, |t| t.at_unix);
    // charge 25 %, battery power 35 %, consumption 40 %
    let h = chart_h.max(240.0);
    history(ui, &m.power_history, now, true, full_wh, (h * 0.25, h * 0.35));
    draw_history(ui, &m.power_history, now, true, h * 0.40, draw_ceiling(m.power_tel.as_deref()));
}

pub fn draw(ui: &mut Ui, m: &MetricsData) {
    let basic;
    let tel: &Telemetry = match &m.power_tel {
        Some(t) => t,
        None => {
            basic = basic_telemetry(m);
            &basic
        }
    };
    if m.battery.is_none() && tel.cells.is_empty() && tel.level.is_none() {
        ui.title(&head(m, "popup.battery"), &sub(m, &t("popup.no_battery")));
        return;
    }
    let flow = powerflow::flow(tel);
    let level = tel.level.or(m.battery.map(|b| b.level));
    let title = match level {
        Some(l) => format!("{} \u{00B7} {}", head(m, "popup.battery"), pct(l)),
        None => head(m, "popup.battery"),
    };
    ui.title(&title, &sub(m, &state_line(tel, &flow, level)));
    let earlier = earlier_reading(tel);
    let basis = system_basis(tel, &flow, chip(tel).total);
    let has_earlier = earlier.is_some();
    // the flow across the full width; below it two columns: what is measured and learned on
    // the left, what can be set and how it went on the right
    let hw = hw_names(m, tel);
    diagram(ui, tel, &flow, level, earlier, &hw);
    ui.space(10.0);
    let local = m.host.is_empty();
    let full_wh = {
        let f: u32 = tel.cells.iter().filter_map(|c| c.full_mwh).sum();
        (f > 0).then(|| f as f32 / 1000.0)
    };
    let gap = ui.t.s(24.0);
    let w = (ui.w - gap) / 2;
    let top = ui.y;
    let h1 = column(ui, 0, w, top, &|ui| {
        readings(ui, tel, local);
        note(ui, tel, &flow, basis, has_earlier);
        // the passive model on machines that have one: what it has learned and what it still needs
        if let Some(l) = tel.learning.as_ref().filter(|_| local) {
            learning_section(ui, l, tel.on_ac == Some(true));
        }
        if local {
            standby_section(ui, m);
        }
        // a remote machine's history is what came while it was connected
        if local || !m.power_history.is_empty() {
            history(ui, &m.power_history, tel.at_unix, false, full_wh, (64.0, 72.0));
        }
    });
    // the right column: what can be set
    let h2 = column(ui, w + gap, w, top, &|ui| {
        if local {
            quick_section(ui, m);
            ui.space(6.0);
            link(ui, &t("power.open_settings"), "ms-settings:powersleep");
        }
        if local || !m.power_history.is_empty() {
            draw_history(ui, &m.power_history, tel.at_unix, false, 136.0, draw_ceiling(Some(tel)));
        }
    });
    ui.y = top + h1.max(h2);
}

/// One column of the popup: drawn on its own canvas, then copied in at `x` (its clicks and
/// tooltips moved along). Returns the height used.
fn column(ui: &mut Ui, x: i32, w: i32, top: i32, f: &dyn Fn(&mut Ui)) -> i32 {
    let mut c = crate::canvas::Canvas::new();
    // as tall as the room left on the popup's canvas (a column cannot draw past it anyway)
    c.resize(w, (ui.c.h - top).max(ui.t.s(200.0)));
    c.clear();
    let (hits, tips, used) = {
        let mut col = Ui {
            c: &mut c,
            t: ui.t,
            w,
            y: 0,
            hover: None,
            pie: None,
            tips: Vec::new(),
            hits: Vec::new(),
            icons: &mut *ui.icons,
            indent: 0,
            locks: &mut *ui.locks,
            frozen: &mut *ui.frozen,
            expanded: ui.expanded,
            proc_filter: ui.proc_filter,
            mem_bar: ui.mem_bar,
            gpu_by_vram: ui.gpu_by_vram,
            charts: Vec::new(),
            last_chart: (0, 0, 0, 0),
            ages: ui.ages,
            mouse: ui.mouse.map(|(mx, my)| (mx - x, my - top)),
            copied: ui.copied.clone(),
            anim: None,
            anim_bg: Vec::new(),
            phase: ui.phase,
            max_rows: None,
            ping_edit: None,
            flipped: ui.flipped,
            fold_open: false,
            fold: Vec::new(),
            order: ui.order,
            drag: None,
            frows: Vec::new(),
        };
        f(&mut col);
        (col.hits, col.tips, col.y)
    };
    ui.c.blit_rect(&c, 0, 0, w, used, x, top);
    ui.hits.extend(hits.into_iter().map(|mut h| {
        h.x += x;
        h.y += top;
        h
    }));
    ui.tips.extend(tips.into_iter().map(|mut tp| {
        tp.x += x;
        tp.y += top;
        tp
    }));
    used
}

/// "Charging · 1 h 12 min to full", "On battery · 3 h 20 min left", "Plugged in · not charging".
fn state_line(tel: &Telemetry, f: &Flow, level: Option<f32>) -> String {
    match f.mode {
        Mode::Charging => match powerflow::time_to_full(tel) {
            Some(s) => format!("{} \u{00B7} {}", t("popup.charging"), tf("power.to_full", &[("time", &format_duration(s))])),
            None => t("popup.charging"),
        },
        Mode::AcIdle if level.map_or(false, |l| l >= 0.995) => t("power.full"),
        Mode::AcIdle => format!("{} \u{00B7} {}", t("popup.plugged_in"), t("power.not_charging")),
        Mode::AcDischarging => format!("{} \u{00B7} {}", t("popup.plugged_in"), t("popup.discharging")),
        Mode::Battery => match tel.remaining_secs {
            Some(s) => format!("{} \u{00B7} {}", t("power.on_battery"), tf("power.left", &[("time", &format_duration(s))])),
            None => t("power.on_battery"),
        },
        Mode::Unknown => String::new(),
    }
}

/// What a card shows: the value, what kind of value it is, and whether it is a live measurement.
#[derive(Clone, Debug, PartialEq)]
struct Shown {
    value: String,
    /// small line under the caption: "calc.", "estimated ±1.1 W", "at least", "not reported"...
    quality: Option<String>,
    /// drawn dimmed: not a live measurement of this value
    dim: bool,
    /// size for the band, W
    watts: Option<f32>,
}

impl Shown {
    fn measured(w: f32, signed: bool) -> Self {
        Shown { value: if signed && w > 0.005 { format!("+{}", watts(w)) } else { watts(w) }, quality: None, dim: false, watts: Some(w.abs()) }
    }
    fn calculated(w: f32) -> Self {
        Shown { value: watts(w), quality: Some(t("power.calculated")), dim: false, watts: Some(w) }
    }
    fn estimated(w: f32, err: Option<f32>) -> Self {
        let quality = match err {
            Some(e) => tf("power.estimated_pm", &[("err", &format!("{e:.1}"))]),
            None => t("power.estimated"),
        };
        Shown { value: format!("\u{2248} {}", watts1(w)), quality: Some(quality), dim: true, watts: Some(w) }
    }
    /// a measured lower bound: only part of the consumers is metered
    fn floor(w: f32, short: bool) -> Self {
        Shown { value: format!("\u{2265} {}", watts1(w)), quality: Some(t(if short { "power.at_least" } else { "power.floor" })), dim: false, watts: Some(w) }
    }
    fn earlier(w: f32, what: String) -> Self {
        Shown { value: watts(w), quality: Some(what), dim: true, watts: None }
    }
    fn unknown() -> Self {
        Shown { value: "\u{2014}".to_string(), quality: Some(t("power.not_reported")), dim: true, watts: None }
    }
    fn from_reading(r: Reading, signed: bool) -> Self {
        match r {
            Reading::Measured(v) => Shown::measured(v, signed),
            Reading::Calculated(v) => Shown::calculated(v),
            Reading::Unavailable => Shown::unknown(),
        }
    }
}

/// How the system draw is known right now.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SystemBasis {
    Measured(f32),
    Estimated(f32, Option<f32>),
    /// CPU package + memory: what is metered at least
    Floor(f32),
    None,
}

/// The metered parts of the chip: (cores, graphics, rest of the package incl. memory) or, where
/// the core / graphics split is missing, the package + memory as one; and their sum.
struct Chip {
    /// the DRAM rail reports a non-zero value (many machines expose it but read 0)
    dram_metered: bool,
    cores: Option<f32>,
    graphics: Option<f32>,
    soc: Option<f32>,
    package: Option<f32>,
    total: Option<f32>,
}

fn chip(tel: &Telemetry) -> Chip {
    // averaged like the battery gauge, so the remainder subtracts like from like
    let meters = if tel.meters_avg.is_empty() { &tel.meters } else { &tel.meters_avg };
    let named = |suffix: &str| meters.iter().find(|m| m.name.ends_with(suffix)).map(|m| m.watts);
    let package = meters.iter().find(|m| m.role == MeterRole::CpuPackage).map(|m| m.watts);
    let dram = named("_DRAM").unwrap_or(0.0);
    let dram_metered = dram > 0.0;
    // sub-rails beyond the package are counter glitches (as WattFlow also guards)
    let sane = |v: Option<f32>| v.filter(|w| *w >= 0.0 && package.map_or(true, |p| *w <= p * 1.15));
    let (cores, graphics) = (sane(named("_PP0")), sane(named("_PP1")));
    let total = package.map(|p| p + dram);
    match (package, cores) {
        (Some(p), Some(c)) => {
            let g = graphics.unwrap_or(0.0);
            Chip { dram_metered, cores: Some(c), graphics, soc: Some((p - c - g).max(0.0) + dram), package: None, total }
        }
        (Some(_), None) => Chip { dram_metered, cores: None, graphics: None, soc: None, package: total, total },
        _ => Chip { dram_metered, cores: None, graphics: None, soc: None, package: None, total: None },
    }
}

fn system_basis(tel: &Telemetry, f: &Flow, chip_total: Option<f32>) -> SystemBasis {
    match f.system {
        Reading::Measured(v) | Reading::Calculated(v) => SystemBasis::Measured(v),
        Reading::Unavailable => match (tel.on_ac == Some(true)).then_some(tel.estimate).flatten() {
            // an estimate below the metered chip is no estimate
            Some((w, e)) if w >= chip_total.unwrap_or(0.0) => SystemBasis::Estimated(w, e),
            _ => chip_total.filter(|w| *w > 0.0).map_or(SystemBasis::None, SystemBasis::Floor),
        },
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Plug,
    Battery(f32),
    Laptop,
}

/// A small line icon centred on (cx, top).
fn icon(c: &mut Canvas, ui_s: &dyn Fn(f32) -> i32, kind: Icon, cx: f32, top: f32, col: Color) {
    let s = |v: f32| ui_s(v) as f32;
    let lw = s(1.4).max(1.0);
    match kind {
        Icon::Plug => {
            // two prongs, a rounded body, the cord
            let (bw, bh) = (s(10.0), s(6.0));
            let by = top + s(4.0);
            c.draw_line(cx - s(2.5), top, cx - s(2.5), by, lw, col);
            c.draw_line(cx + s(2.5), top, cx + s(2.5), by, lw, col);
            c.fill_round_rect((cx - bw / 2.0) as i32, by as i32, bw as i32, bh as i32, s(2.0), col);
            c.draw_line(cx, by + bh, cx, top + s(14.0), lw, col);
        }
        Icon::Battery(level) => {
            let (w, h) = (s(20.0), s(10.0));
            let (x, y) = (cx - w / 2.0 - s(1.0), top + s(2.0));
            c.stroke_round_rect_r(x as i32, y as i32, w as i32, h as i32, s(2.5), col);
            c.fill_round_rect((x + w) as i32, (y + h * 0.3) as i32, s(2.0) as i32, (h * 0.4).ceil() as i32, s(1.0), col);
            let inner = (w - s(4.0)) * level.clamp(0.0, 1.0);
            if inner >= 1.0 {
                c.fill_round_rect((x + s(2.0)) as i32, (y + s(2.0)) as i32, inner.round() as i32, (h - s(4.0)) as i32, s(1.0), col);
            }
        }
        Icon::Laptop => {
            let (w, h) = (s(16.0), s(10.0));
            let (x, y) = (cx - w / 2.0, top + s(1.0));
            c.stroke_round_rect_r(x as i32, y as i32, w as i32, h as i32, s(1.5), col);
            c.fill_round_rect((cx - w / 2.0 - s(3.0)) as i32, (y + h + s(1.0)) as i32, (w + s(6.0)) as i32, s(2.0) as i32, s(1.0), col);
        }
    }
}

fn card_frame(ui: &mut Ui, (x, y, w, h): (i32, i32, i32, i32), radius_dip: f32) {
    let r = ui.t.s(radius_dip) as f32;
    let (fill, stroke) = ui.t.style.card(ui.t.light);
    ui.c.fill_round_rect(x, y, w, h, r, fill);
    ui.c.stroke_round_rect_r(x, y, w, h, r, stroke.unwrap_or(ui.t.dim.alpha(0.22)));
}

/// A node card: icon, value, caption and a quality line where the value is not a plain
/// measurement.
fn node(ui: &mut Ui, rect: (i32, i32, i32, i32), kind: Icon, shown: &Shown, caption: &str, col: Color, value_dip: f32) {
    let (x, y, w, h) = rect;
    card_frame(ui, rect, 8.0);
    let fv = ui.font(value_dip, FW_SEMIBOLD);
    let fc = ui.font(9.5, FW_NORMAL);
    let fq = ui.font(8.5, FW_NORMAL);
    let icon_h = ui.t.s(15.0);
    let gap = ui.t.s(4.0);
    let inner = w - ui.t.s(6.0);
    let (vw, vh) = ui.c.measure_text(&shown.value, fv);
    let (_, ch) = ui.c.measure_text(caption, fc);
    let qh = shown.quality.as_ref().map_or(0, |q| ui.c.measure_text(q, fq).1);
    // short cards leave the icon out
    let with_icon = h >= icon_h + gap + vh + ch + qh + ui.t.s(6.0);
    let total = if with_icon { icon_h + gap } else { 0 } + vh + ch + qh;
    let mut cy = y + (h - total) / 2;
    let cx = x as f32 + w as f32 / 2.0;
    if with_icon {
        let th = ui.t;
        icon(ui.c, &|v| th.s(v), kind, cx, cy as f32, if shown.watts.is_none() && shown.dim { ui.t.dim } else { col });
        cy += icon_h + gap;
    }
    let vw = vw.min(inner);
    ui.clipped(&shown.value, x + (w - vw) / 2, cy, inner, fv, if shown.dim { ui.t.dim } else { ui.t.text });
    cy += vh;
    let cw = ui.c.measure_text(caption, fc).0.min(inner);
    ui.clipped(caption, x + (w - cw) / 2, cy, inner, fc, ui.t.dim);
    cy += ch;
    if let Some(q) = &shown.quality {
        let qw = ui.c.measure_text(q, fq).0.min(inner);
        ui.clipped(q, x + (w - qw) / 2, cy, inner, fq, ui.t.dim.alpha(0.8));
    }
}

/// The hardware the flow's cards are about, as this machine reports it.
#[derive(Default)]
pub struct HwNames {
    cpu: String,
    gpu: String,
    /// discrete graphics cards: name, power state (0 = awake .. 3 = asleep), board power
    /// where it was read (awake and busy only)
    dgpus: Vec<(String, Option<u8>, Option<f32>)>,
    ram: String,
    ssd: String,
    wifi: String,
    screens: String,
}

/// Drop trademarks and filler: "Intel(R) Core(TM) Ultra 9 285H" -> "Core Ultra 9 285H".
fn short_hw(s: &str) -> String {
    let mut t = s.replace("(R)", "").replace("(TM)", "").replace("(tm)", "").replace("(r)", "");
    for w in ["Intel ", "AMD ", "NVIDIA ", " Processor", " CPU", " GPU"] {
        t = t.replace(w, " ");
    }
    // "(16GB)", "320MHz", "@ 2.4GHz"
    let t: Vec<&str> = t.split_whitespace().filter(|w| !(w.starts_with('(') && w.ends_with(')')) && !w.ends_with("MHz") && !w.ends_with("GHz") && *w != "@").collect();
    t.join(" ")
}

/// Panel maker from the EDID manufacturer code.
fn panel_maker(code: &str) -> &str {
    match code {
        "SDC" => "Samsung Display",
        "BOE" => "BOE",
        "LGD" => "LG Display",
        "AUO" => "AUO",
        "CSO" => "CSOT",
        "SHP" => "Sharp",
        "IVO" => "InfoVision",
        "CMN" => "Innolux",
        "NCP" => "Nanjing CEC Panda",
        _ => code,
    }
}

pub fn hw_names(m: &MetricsData, tel: &Telemetry) -> HwNames {
    // the graphics domain of the chip: the processor maker's own adapter; every other real
    // adapter is a discrete card (its power is outside the chip's meters)
    use crate::metrics::gpu::{discrete_indices, integrated_index};
    let igpu = integrated_index(&m.gpu_adapters, &m.cpu_name);
    let gpu = igpu.map(|i| short_hw(&m.gpu_adapters[i].name)).unwrap_or_default();
    let dgpus = discrete_indices(&m.gpu_adapters, &m.cpu_name)
        .into_iter()
        .map(|i| &m.gpu_adapters[i])
        .map(|a| {
            let busy = a.d_state == Some(0) && a.load.unwrap_or(0.0) > 0.03;
            let w = a.perf.power_w.filter(|_| busy && !a.perf.power_cores_only);
            (short_hw(&a.name), a.d_state, w)
        })
        .collect();
    let total: u64 = m.mem_modules.iter().map(|x| x.size).sum();
    let ram = match m.mem_modules.first() {
        Some(x) if total > 0 => format!("{} GB {}", (total as f64 / (1u64 << 30) as f64).round(), x.mem_type).trim().to_string(),
        _ => String::new(),
    };
    let ssd = m.disks.first().map(|d| {
        let model = d.model.split('-').next().unwrap_or(&d.model).trim();
        // "SAMSUNG MZVL8…" -> "Samsung MZVL8…"
        let mut it = model.splitn(2, ' ');
        match (it.next(), it.next()) {
            (Some(b), Some(r)) if b.chars().all(|c| c.is_ascii_uppercase()) && b.len() > 3 => format!("{}{} {r}", &b[..1], b[1..].to_lowercase()),
            _ => model.to_string(),
        }
    }).unwrap_or_default();
    let wifi = m.net_ifaces.iter().map(|i| i.description.as_str()).find(|d| d.contains("Wi-Fi") || d.contains("Wireless")).map(short_hw).unwrap_or_default();
    // the built-in panels the model knows ("ABC1234#UID…")
    let panels: Vec<String> = tel.learning.as_ref().map(|l| l.panels.iter().map(|(id, _, _)| id.split('#').next().unwrap_or(id).to_string()).collect()).unwrap_or_default();
    let screens = match panels.first() {
        Some(p) if p.len() > 3 => {
            let same = panels.iter().all(|x| x == p);
            let one = format!("{} {}", panel_maker(&p[..3]), &p[3..]);
            if same && panels.len() > 1 { format!("{}\u{00D7} {one}", panels.len()) } else { one }
        }
        _ => String::new(),
    };
    HwNames { cpu: short_hw(&m.cpu_name), gpu, dgpus, ram, ssd, wifi, screens }
}

/// A component card in the third column: dot and name, what it is below, the value right.
fn part_card(ui: &mut Ui, rect: (i32, i32, i32, i32), name: &str, role: &str, shown: &Shown, col: Color) {
    let (x, y, w, h) = rect;
    card_frame(ui, rect, 6.0);
    let pad = ui.t.s(7.0);
    let fn_ = ui.font(9.5, FW_SEMIBOLD);
    let fr = ui.font(8.0, FW_NORMAL);
    let fv = ui.font(12.5, FW_SEMIBOLD);
    let fq = ui.font(7.5, FW_NORMAL);
    // the value right, its quality tag under it
    let (vw, vh) = ui.c.measure_text(&shown.value, fv);
    let qh = shown.quality.as_ref().map_or(0, |q| ui.c.measure_text(q, fq).1);
    let vtop = y + (h - vh - qh).max(0) / 2;
    ui.c.draw_text(&shown.value, x + w - pad - vw, vtop, fv, if shown.dim { ui.t.dim } else { ui.t.text });
    if let Some(q) = &shown.quality {
        let qw = ui.c.measure_text(q, fq).0;
        ui.c.draw_text(q, x + w - pad - qw, vtop + vh, fq, ui.t.dim.alpha(0.8));
    }
    // name and role left
    let (_, nh) = ui.c.measure_text("Xg", fn_);
    let rh = if role.is_empty() { 0 } else { ui.c.measure_text("Xg", fr).1 };
    let top = y + (h - nh - rh).max(0) / 2;
    let dot = ui.t.s(3.0) as f32;
    ui.c.fill_circle((x + pad) as f32 + dot, top as f32 + nh as f32 / 2.0, dot, if shown.watts.is_some() { col } else { col.alpha(0.4) });
    let lx = x + pad + ui.t.s(10.0);
    let maxw = x + w - pad - vw - ui.t.s(8.0) - lx;
    ui.clipped(name, lx, top, maxw, fn_, ui.t.text);
    if !role.is_empty() {
        ui.clipped(role, lx, top + nh, maxw, fr, ui.t.dim);
    }
}

/// Pulse speed: faster with more power, but always calm.
fn speed(w: f32) -> f32 {
    if w <= 0.05 {
        0.0
    } else {
        (0.25 + 0.07 * w.sqrt()).min(1.1)
    }
}

/// One link of the diagram: from a stack on the left edge to a target's centre on the right.
struct Link {
    to_mid: f32,
    watts: Option<f32>,
    color: Color,
    reverse: bool,
    /// drawn fainter: the size is an estimate or a floor
    soft: bool,
}

/// Bands (or dashed paths for unknown sizes) from a source edge at `x0` (stacked around
/// `from_mid`) to targets at `x1`; `px_per_w` keeps both stages of the diagram to one scale.
fn links(ui: &mut Ui, x0: f32, x1: f32, from_mid: f32, list: &[Link], px_per_w: f32, max_t: f32, bands: &mut Vec<Band>) {
    let min_t = ui.t.s(2.5) as f32;
    let gap = ui.t.s(3.0) as f32;
    let alpha = if ui.t.light { 0.42 } else { 0.5 };
    let size = |w: f32| if w <= 0.05 { min_t * 0.5 } else { (w * px_per_w).clamp(min_t, max_t) };
    let known: Vec<f32> = list.iter().filter_map(|l| l.watts.map(size)).collect();
    let stack = known.iter().sum::<f32>() + gap * known.len().saturating_sub(1) as f32;
    let mut y = from_mid - stack / 2.0;
    let period = ui.t.s(40.0) as f32;
    for l in list {
        match l.watts {
            Some(w) => {
                let th = size(w);
                let s = speed(w);
                let a = if l.soft { alpha * 0.6 } else { alpha };
                bands.push(Band { x0, x1, left: (y, y + th), right: (l.to_mid - th / 2.0, l.to_mid + th / 2.0), color: l.color.alpha(a), speed: if l.reverse { -s } else { s }, period });
                y += th + gap;
            }
            None => {
                let y0 = if known.is_empty() { from_mid } else { y + ui.t.s(2.0) as f32 };
                ui.c.dashed_curve(x0 + 2.0, x1 - 2.0, y0, l.to_mid, ui.t.s(1.5) as f32, ui.t.s(4.0) as f32, l.color.alpha(0.55));
            }
        }
    }
}

/// Colours of the chip parts and the rest.
fn part_colors(light: bool) -> [Color; 6] {
    if light {
        [Color::rgb(0x00, 0x5F, 0xB8), Color::rgb(0x00, 0x7A, 0x6E), Color::rgb(0x6B, 0x3F, 0xB8), Color::rgb(0x70, 0x70, 0x70), Color::rgb(0xC2, 0x3B, 0x8E), Color::rgb(0xA0, 0x6A, 0x00)]
    } else {
        [Color::rgb(0x60, 0xCD, 0xFF), Color::rgb(0x4C, 0xD6, 0xC0), Color::rgb(0xB1, 0x8C, 0xF2), Color::rgb(0xA8, 0xA8, 0xA8), Color::rgb(0xF4, 0x8F, 0xC8), Color::rgb(0xE8, 0xB0, 0x50)]
    }
}

fn diagram(ui: &mut Ui, tel: &Telemetry, f: &Flow, level: Option<f32>, earlier: Option<(f32, String)>, hw: &HwNames) {
    let p = palette(ui.t.light);
    let chip = chip(tel);
    let dgpu_w: f32 = hw.dgpus.iter().filter_map(|d| d.2).sum();
    let basis = match system_basis(tel, f, chip.total) {
        SystemBasis::Estimated(s, e) => SystemBasis::Estimated(s + dgpu_w, e),
        SystemBasis::Floor(s) => SystemBasis::Floor(s + dgpu_w),
        b => b,
    };
    let system = match basis {
        SystemBasis::Measured(v) => Shown::measured(v, false),
        SystemBasis::Estimated(v, e) => Shown::estimated(v, e),
        SystemBasis::Floor(v) => Shown::floor(v, false),
        SystemBasis::None => match earlier {
            Some((w, what)) => Shown::earlier(w, what),
            None => Shown::unknown(),
        },
    };
    let on_ac = matches!(f.mode, Mode::Charging | Mode::AcIdle | Mode::AcDischarging);
    // battery as drawn: + into it on AC, the discharge as a positive flow out of it otherwise
    let discharging = matches!(f.mode, Mode::AcDischarging | Mode::Battery | Mode::Unknown);
    let battery = match f.battery {
        Reading::Measured(v) => Shown::measured(if discharging { -v } else { v }, !discharging),
        other => Shown::from_reading(other, false),
    };
    let rate = f.battery.value();
    // adapter: measured / calculated as the flow model has it; else battery + the system basis
    let adapter = match f.adapter {
        Reading::Measured(_) | Reading::Calculated(_) => Shown::from_reading(f.adapter, false),
        Reading::Unavailable => match (rate, basis) {
            (Some(r), SystemBasis::Estimated(s, e)) => Shown::estimated((r + s).max(0.0), e),
            (Some(r), SystemBasis::Floor(s)) => Shown::floor((r + s).max(0.0), true),
            _ => Shown::unknown(),
        },
    };
    let soft = !matches!(basis, SystemBasis::Measured(_));
    // the chip parts, then the rest of the machine (screens, SSD, Wi-Fi, losses)
    let rest = match basis {
        SystemBasis::Measured(s) => chip.total.map(|c| Shown::calculated((s - c - dgpu_w).max(0.0))),
        // the small card has no room for the ±: it is on the System card
        SystemBasis::Estimated(s, _) => chip.total.map(|c| Shown::estimated((s - c - dgpu_w).max(0.0), None)),
        _ => chip.total.map(|_| match &tel.learning {
            Some(l) if !l.ready => Shown { value: "\u{2014}".to_string(), quality: Some(tf("power.learning_short", &[("n", &l.n.to_string()), ("total", &(l.n + l.points_needed).max(l.min_points).to_string())])), dim: true, watts: None },
            _ => Shown::unknown(),
        }),
    };
    let pc = part_colors(ui.t.light);
    // (name, what it is, value, colour, tooltip); the name is the hardware where it is known
    let named = |hwname: &str, role: String| if hwname.is_empty() { (role, String::new()) } else { (hwname.to_string(), role) };
    // the rest of the machine: what it holds on this one
    let others: Vec<&str> = [hw.ram.as_str(), hw.ssd.as_str(), hw.wifi.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    let others = others.join(" \u{00B7} ");
    let mut parts: Vec<(String, String, Shown, Color, String)> = Vec::new();
    if let Some(c) = chip.cores {
        let (n, r) = named(&hw.cpu, t("power.part_cores"));
        parts.push((n, r, Shown::measured(c, false), pc[0], t("power.tip_cores")));
        if let Some(g) = chip.graphics {
            let (n, r) = named(&hw.gpu, t("power.part_graphics"));
            parts.push((n, r, Shown::measured(g, false), pc[1], t("power.tip_graphics")));
        }
        if let Some(s) = chip.soc {
            // "+ memory" only where the memory rail actually reports
            let (k, tip) = if chip.dram_metered { ("power.part_soc", "power.tip_soc") } else { ("power.part_uncore", "power.tip_uncore") };
            parts.push((t(k), t("power.role_uncore"), Shown::measured(s, false), pc[2], t(tip)));
        }
    } else if let Some(pk) = chip.package {
        let (n, r) = named(&hw.cpu, t("power.cpu_package"));
        parts.push((n, r, Shown::measured(pk, false), pc[0], t("power.tip_package")));
    }
    // discrete graphics: measured board power while busy, else asleep / idle (not read, so it
    // can sleep)
    for (name, state, w) in &hw.dgpus {
        let shown = match (w, state) {
            (Some(w), _) => Shown::measured(*w, false),
            (None, Some(s)) if *s >= 3 => Shown { value: t("power.dgpu_asleep"), quality: Some("~0 W".into()), dim: true, watts: None },
            (None, Some(0)) => Shown { value: t("power.dgpu_awake"), quality: Some(t("power.not_read")), dim: true, watts: None },
            _ => Shown::unknown(),
        };
        let green = if ui.t.light { Color::rgb(0x3E, 0x7A, 0x00) } else { Color::rgb(0x8C, 0xD1, 0x3C) };
        parts.push((name.clone(), t("power.role_dgpu"), shown, green, t("power.tip_dgpu")));
    }
    if let Some(r) = rest {
        // once learned: the screens and the conversion losses as the model has them, the rest up
        // to the measured (or estimated) total as the remainder, so the parts still add up
        match (tel.parts, r.watts) {
            (Some(pp), Some(total)) if pp.screens + pp.conversion > 0.05 && total > 0.0 => {
                let k = ((total * 0.95) / (pp.screens + pp.conversion)).min(1.0);
                let (scr, conv) = (pp.screens * k, pp.conversion * k);
                let other = (total - scr - conv).max(0.0);
                if scr > 0.05 {
                    let (n, r) = named(&hw.screens, t("power.part_screens"));
                    parts.push((n, r, Shown::estimated(scr, None), pc[4], t("power.tip_screens")));
                }
                if conv > 0.05 {
                    parts.push((t("power.part_conversion"), t("power.role_conversion"), Shown::estimated(conv, None), pc[5], t("power.tip_conversion")));
                }
                let other_shown = if matches!(basis, SystemBasis::Measured(_)) { Shown::calculated(other) } else { Shown::estimated(other, None) };
                let tip = if others.is_empty() { t("power.tip_other") } else { format!("{}\n\n{others}", t("power.tip_other")) };
                parts.push((t("power.part_other"), others.clone(), other_shown, pc[3], tip));
            }
            _ => {
                let tip = if others.is_empty() { t("power.tip_rest") } else { format!("{}\n\n{others}", t("power.tip_rest")) };
                parts.push((t("power.part_rest"), others.clone(), r, pc[3], tip));
            }
        }
    }

    let three = !parts.is_empty();
    // the diagram grows with the number of parts (a card needs about 40 DIP)
    let h = if three { ui.t.s(176.0).max(ui.t.s(40.0) * parts.len() as i32 + ui.t.s(6.0) * (parts.len() as i32 - 1)) } else { ui.t.s(132.0) };
    let top = ui.y;
    let th = ui.t;
    let s = |v: f32| th.s(v);
    // columns: source | (battery, system) | parts
    let (w1, w2, w3) = if three { (s(86.0), s(90.0), s(240.0)) } else { (s(100.0), s(100.0), 0) };
    let x2 = if three { w1 + (ui.w - w1 - w2 - w3) / 2 } else { ui.w - w2 };
    let x3 = ui.w - w3;
    let value_dip = if three { 14.5 } else { 17.0 };
    let gap_v = s(8.0);
    // part cards: height each
    let n_parts = parts.len() as i32;
    let part_gap = s(6.0);
    let ph = if three { ((h - part_gap * (n_parts - 1)) / n_parts).min(s(46.0)) } else { 0 };
    // scale: the largest flow fills about half the height, but no band may get thicker than
    // the card it ends at (a 60 W laptop otherwise overflows the part cards), and the bands
    // leaving a card must stack within it; one scale for both stages
    let biggest = [adapter.watts, battery.watts, system.watts].iter().flatten().fold(20.0f32, |m, v| m.max(*v));
    let max_t = h as f32 * 0.5;
    let mut px_per_w = max_t / biggest;
    let src_h = if three { s(112.0) } else { h };
    let sys_h = if !three { h } else if on_ac { h - s(62.0) - gap_v } else { s(112.0) };
    let fit = |px: f32, room: f32, w: f32| if w > 0.05 { px.min(room / w) } else { px };
    px_per_w = fit(px_per_w, src_h as f32 * 0.85, battery.watts.unwrap_or(0.0).abs() + system.watts.unwrap_or(0.0));
    px_per_w = fit(px_per_w, sys_h as f32 * 0.85, system.watts.unwrap_or(0.0).max(parts.iter().filter_map(|p| p.2.watts).sum()));
    let part_max_t = ph as f32 * 0.8;
    if three {
        let biggest_part = parts.iter().filter_map(|p| p.2.watts).fold(0.0f32, f32::max);
        px_per_w = fit(px_per_w, part_max_t, biggest_part);
    }
    let level = level.unwrap_or(0.0);
    let mut bands = Vec::new();
    let mid = (top + h / 2) as f32;
    let src = (0, top + (h - src_h) / 2, w1, src_h);
    let sys_rect;
    if on_ac {
        let bh = if three { s(62.0) } else { (h - gap_v) / 2 };
        let brect = (x2, top, w2, bh);
        sys_rect = (x2, top + bh + gap_v, w2, h - bh - gap_v);
        let text = ui.t.text;
        node(ui, src, Icon::Plug, &adapter, &t("power.adapter"), text, value_dip);
        let (bcap, bcol) = if discharging { (t("power.from_battery"), p.discharge) } else { (t("power.into_battery"), p.charge) };
        node(ui, brect, Icon::Battery(level), &battery, &bcap, bcol, value_dip);
        node(ui, sys_rect, Icon::Laptop, &system, &t("power.system"), p.system, value_dip);
        let l = [
            Link { to_mid: (brect.1 + brect.3 / 2) as f32, watts: battery.watts, color: bcol, reverse: discharging, soft: false },
            Link { to_mid: (sys_rect.1 + sys_rect.3 / 2) as f32, watts: system.watts, color: p.system, reverse: false, soft },
        ];
        links(ui, w1 as f32, x2 as f32, mid, &l, px_per_w, max_t, &mut bands);
    } else {
        let sh = if three { s(112.0) } else { h };
        sys_rect = (x2, top + (h - sh) / 2, w2, sh);
        node(ui, src, Icon::Battery(level), &battery, &t("power.from_battery"), p.discharge, value_dip);
        node(ui, sys_rect, Icon::Laptop, &system, &t("power.system"), p.system, value_dip);
        let l = [Link { to_mid: (sys_rect.1 + sys_rect.3 / 2) as f32, watts: system.watts.or(battery.watts), color: p.discharge, reverse: false, soft: false }];
        links(ui, w1 as f32, x2 as f32, mid, &l, px_per_w, max_t, &mut bands);
    }
    if three {
        let (n, g) = (n_parts, part_gap);
        let used = ph * n + g * (n - 1);
        let mut y = top + (h - used) / 2;
        let mut l = Vec::new();
        for (name, role, shown, col, tip) in &parts {
            part_card(ui, (x3, y, w3, ph), name, role, shown, *col);
            ui.tips_for_rect(x3, y, w3, ph, tip.clone());
            l.push(Link { to_mid: (y + ph / 2) as f32, watts: shown.watts, color: *col, reverse: false, soft: shown.dim });
            y += ph + g;
        }
        let from_mid = (sys_rect.1 + sys_rect.3 / 2) as f32;
        links(ui, (x2 + w2) as f32, x3 as f32, from_mid, &l, px_per_w, part_max_t, &mut bands);
    }
    // the animated area: the whole diagram (bands only touch the gaps between the cards)
    let a = FlowAnim { x: 0, y: top, w: ui.w, h, bands };
    if !a.bands.is_empty() {
        ui.anim_bg = ui.c.copy_region(a.x, a.y, a.w, a.h);
        paint_bands(ui.c, &a, ui.phase);
        ui.anim = Some(a);
    }
    ui.y = top + h;
}

/// Label | small tag | value, the tag naming calculated values.
fn row(ui: &mut Ui, label: &str, tag: Option<&str>, value: &str) {
    ui.proc_row(label, tag, value);
}

fn readings(ui: &mut Ui, tel: &Telemetry, local: bool) {
    let calc = t("power.calculated");
    if local && tel.at_unix > 0 {
        row(ui, &t("power.read_at"), None, &clock(tel.at_unix));
    }
    if let Some(mv) = tel.voltage_mv() {
        row(ui, &t("power.voltage"), None, &format!("{:.2} V", mv as f32 / 1000.0));
    }
    if let Some(ma) = tel.current_ma() {
        row(ui, &t("power.current"), Some(&calc), &format!("{:.2} A", ma as f32 / 1000.0));
    }
    if let (Some(now), Some(full)) = (tel.capacity_mwh(), tel.full_mwh()) {
        row(ui, &t("popup.capacity"), None, &format!("{:.1} / {:.1} Wh", now as f32 / 1000.0, full as f32 / 1000.0));
    }
    if let (Some(h), Some(design)) = (tel.health(), tel.design_mwh()) {
        let start = ui.y;
        row(ui, &t("power.health"), None, &format!("{} \u{00B7} {:.1} Wh", pct(h), design as f32 / 1000.0));
        ui.hover_tip(start, t("power.health_tip"));
    }
    if let Some(c) = tel.cycles() {
        row(ui, &t("popup.cycles"), None, &c.to_string());
    }
    if let Some(c) = tel.temp_c() {
        let text = if ui.t.temp_f { format!("{:.0} \u{00B0}F", c * 9.0 / 5.0 + 32.0) } else { format!("{c:.0} \u{00B0}C") };
        row(ui, &t("power.temperature"), None, &text);
    }
    if let Some((kind, max_ma)) = tel.source {
        let mut text = match kind {
            SourceKind::Ac => t("power.source_ac"),
            SourceKind::Usb => t("power.source_usb"),
            SourceKind::Wireless => t("power.source_wireless"),
        };
        if let Some(w) = tel.source_w {
            text.push_str(&format!(" \u{00B7} {w} W"));
        }
        if let Some(ma) = max_ma {
            text.push_str(&format!(" \u{00B7} {}", tf("power.max_current", &[("a", &format!("{:.2}", ma as f32 / 1000.0))])));
        }
        row(ui, &t("power.charger"), None, &text);
    }
    // energy meters: shown on their own, never folded into the flows above
    for mc in if tel.meters_avg.is_empty() { &tel.meters } else { &tel.meters_avg } {
        let label = match mc.role {
            MeterRole::Platform => t("power.platform"),
            MeterRole::CpuPackage => t("power.cpu_package"),
            MeterRole::Other => continue,
        };
        let start = ui.y;
        row(ui, &label, Some("EMI"), &watts(mc.watts));
        ui.hover_tip(start, tf("power.meter_tip", &[("name", &mc.name)]));
    }
    if local {
        let start = ui.y;
        let on = tel.saver;
        let value = if on { t("power.on") } else { t("power.off") };
        let f = ui.font(11.0, FW_NORMAL);
        let (vw, h) = ui.c.measure_text(&value, f);
        ui.clipped(&t("power.energy_saver"), 0, ui.y, ui.w - vw - ui.t.s(8.0), f, ui.t.dim);
        ui.c.draw_text(&value, ui.w - vw, ui.y, f, if on { palette(ui.t.light).discharge } else { ui.t.text });
        ui.y += h + ui.t.s(3.0);
        ui.clickable(start, Action::OpenUri("ms-settings:batterysaver".into()));
        ui.hover_tip(start, t("power.open_settings"));
    }
    if let Some(cell) = tel.cells.first() {
        let parts: Vec<String> = [cell.name.clone(), cell.manufacturer.clone(), cell.chemistry.clone()].into_iter().filter(|s| !s.is_empty()).collect();
        if !parts.is_empty() {
            let mut text = parts.join(" \u{00B7} ");
            if tel.cells.len() > 1 {
                text.push_str(&format!(" (+{})", tel.cells.len() - 1));
            }
            ui.info_row(&[(text, None)], "", ui.t.dim);
        }
    }
}

/// What the diagram can and cannot tell on this machine, in one sentence.
/// What the System card stands on, in one sentence.
fn note(ui: &mut Ui, tel: &Telemetry, f: &Flow, basis: SystemBasis, earlier: bool) {
    let key = match f.mode {
        Mode::Battery if tel.platform_watts().is_some() => "power.note_battery_meter",
        Mode::Battery => "power.note_battery",
        _ if matches!(basis, SystemBasis::Estimated(..)) => "power.note_ac_estimate",
        _ if matches!(basis, SystemBasis::Floor(_)) => "power.note_ac_floor",
        _ if earlier && matches!(f.system, Reading::Unavailable) => "power.note_ac_earlier",
        _ if matches!(f.system, Reading::Unavailable) => "power.note_ac_unknown",
        _ => "power.note_ac_meter",
    };
    ui.space(4.0);
    let f = ui.font(9.5, FW_NORMAL);
    wrap(ui, &t(key), f, ui.t.dim.alpha(0.85));
}

/// readings older than this are not shown: the load has surely changed
const EARLIER_MAX_SECS: u64 = 7 * 86_400;

/// The draw last measured on battery with its age ("on battery, 12 min ago"), for the System
/// card on AC when nothing live describes it.
fn earlier_reading(tel: &Telemetry) -> Option<(f32, String)> {
    if tel.on_ac != Some(true) || tel.platform_watts().is_some() {
        return None;
    }
    let (w, at) = tel.last_on_battery?;
    let age = tel.at_unix.saturating_sub(at);
    (age <= EARLIER_MAX_SECS).then(|| (w, tf("power.earlier", &[("age", &age_text(age))])))
}

/// "40 s", "12 min", "3 h", "2 d"
fn age_text(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86_399 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86_400),
    }
}

/// Dim text word-wrapped across the full width.
pub(super) fn wrap(ui: &mut Ui, text: &str, f: FontSpec, color: Color) {
    let h = ui.c.measure_text("X", f).1;
    let mut line = String::new();
    // CJK text breaks between any characters (it has few or no spaces); other text at spaces
    let cjk = text.chars().any(|c| (c as u32) >= 0x2E80);
    let tokens: Vec<String> = if cjk { text.chars().map(|c| c.to_string()).collect() } else { text.split(' ').map(|s| s.to_string()).collect() };
    let sep = if cjk { "" } else { " " };
    for word in tokens {
        let candidate = if line.is_empty() { word.clone() } else { format!("{line}{sep}{word}") };
        if line.is_empty() || ui.c.measure_text(&candidate, f).0 <= ui.w {
            line = candidate;
        } else {
            ui.c.draw_text(&line, 0, ui.y, f, color);
            ui.y += h;
            line = word;
        }
    }
    if !line.is_empty() {
        ui.c.draw_text(&line, 0, ui.y, f, color);
        ui.y += h;
    }
    ui.space(2.0);
}

/// "23:30:33" local time of a unix timestamp.
fn clock(at_unix: u64) -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let now_unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let st = unsafe { GetLocalTime() };
    let day_secs = st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64;
    let s = (day_secs - (now_unix as i64 - at_unix as i64)).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// A right-aligned accent link.
fn link(ui: &mut Ui, text: &str, uri: &str) {
    let f = ui.font(10.5, FW_NORMAL);
    let start = ui.y;
    let (_, h) = ui.c.draw_text(text, 0, ui.y, f, ui.t.accent);
    ui.y += h + ui.t.s(2.0);
    ui.clickable(start, Action::OpenUri(uri.to_string()));
}

// ---------------------------------------------------------------- quick settings, standby

/// A small dim caption above a control.
fn caption(ui: &mut Ui, text: &str) {
    let f = ui.font(10.0, FW_NORMAL);
    let (_, h) = ui.c.draw_text_clipped(text, 0, ui.y, ui.w, f, ui.t.dim);
    ui.y += h + ui.t.s(3.0);
}

/// Keep awake, screen-off timeouts.
fn quick_section(ui: &mut Ui, m: &MetricsData) {
    use super::cards::segmented;
    use crate::metrics::quick::{minutes, SCREEN_CHOICES};
    ui.section(&t("power.quick_title"));
    caption(ui, &t("power.awake_title"));
    segmented(ui, &[
        (t("power.off"), !m.keep_awake, Some(Action::KeepAwake(false)), t("power.awake_tip")),
        (t("power.on"), m.keep_awake, Some(Action::KeepAwake(true)), t("power.awake_tip")),
    ]);
    ui.space(4.0);
    if let Some((ac, dc)) = m.screen_off {
        for (on_ac, cur, key) in [(true, ac, "power.screen_ac"), (false, dc, "power.screen_dc")] {
            let mut cap = t(key);
            if !SCREEN_CHOICES.contains(&cur) {
                cap = format!("{cap} \u{00B7} {}", tf("power.screen_now", &[("min", &minutes(cur).to_string())]));
            }
            caption(ui, &cap);
            let items: Vec<(String, bool, Option<Action>, String)> = SCREEN_CHOICES
                .iter()
                .map(|s| (if *s == 0 { t("power.never") } else { minutes(*s).to_string() }, *s == cur, Some(Action::ScreenOff(on_ac, *s)), t("power.screen_tip")))
                .collect();
            segmented(ui, &items);
            ui.space(2.0);
        }
    }
}

/// Battery lost during the last standbys on battery.
fn standby_section(ui: &mut Ui, m: &MetricsData) {
    if m.battery.is_none() {
        return;
    }
    ui.section(&t("power.standby_title"));
    let f = ui.font(9.5, FW_NORMAL);
    if m.sleeps.is_empty() {
        wrap(ui, &t("power.standby_none"), f, ui.t.dim);
        ui.y += ui.t.s(4.0);
        return;
    }
    for e in m.sleeps.iter().rev().take(4) {
        let label = format!("{} \u{2192} {} \u{00B7} {}", &clock(e.from)[..5], &clock(e.to)[..5], format_duration(e.secs().min(u32::MAX as u64) as u32));
        let mut value = format!("\u{2212}{:.0} %", e.lost_pct());
        if let Some(w) = e.avg_w() {
            value = format!("{value} \u{00B7} {}", watts(w));
        }
        row(ui, &label, None, &value);
    }
    wrap(ui, &t("power.standby_note"), f, ui.t.dim.alpha(0.8));
    ui.y += ui.t.s(4.0);
}

// ---------------------------------------------------------------- learning model

/// "≥ 7 min": whole minutes, rounded up.
fn minutes_up(secs: u32) -> String {
    ((secs + 59) / 60).max(1).to_string()
}

/// How far the passive model has come: measurements collected, what each screen still needs,
/// how far off its predictions were before it learned each new point, and what to do next.
fn learning_section(ui: &mut Ui, l: &crate::metrics::powermodel::Learning, on_ac: bool) {
    let p = palette(ui.t.light);
    ui.section(&t("power.model_title"));
    // learned: one line, the details on a click
    if l.ready {
        let open = ui.expanded.contains("model");
        let acc = match l.rms {
            Some(r) => tf("power.model_accuracy", &[("w", &format!("{r:.1}"))]),
            None => t("power.model_unchecked"),
        };
        let line = tf("power.model_done", &[("n", &l.n.to_string()), ("acc", &acc)]);
        let f = ui.font(10.5, FW_NORMAL);
        let start = ui.y;
        let (_, h) = ui.c.draw_text_clipped(&line, 0, ui.y, ui.w - ui.t.s(70.0), f, p.charge);
        let fl = ui.font(9.5, FW_NORMAL);
        let link = t(if open { "popup.less" } else { "power.model_details" });
        let lw = ui.c.measure_text(&link, fl).0;
        ui.c.draw_text(&link, ui.w - lw, ui.y + ui.t.s(1.0), fl, ui.t.accent);
        ui.y += h + ui.t.s(5.0);
        ui.clickable(start, Action::Expand("model".into()));
        if !open {
            return;
        }
    }
    // collected / needed in all (the target grows when a screen still needs its states)
    let total = (l.n + l.points_needed).max(l.min_points);
    let frac = if l.ready { 1.0 } else { l.n as f32 / total as f32 };
    let col = if l.ready { p.charge } else { ui.t.accent };
    // once learned, the counts keep going: shown as they are, without the target
    let count = if l.ready { l.n.to_string() } else { format!("{}/{}", l.n.min(total), total) };
    ui.bar_row(&t("power.measurements"), frac, &count, col);
    for (i, (_, on, off)) in l.panels.iter().enumerate() {
        let need = l.min_per_state;
        let f = (on.min(&need) + off.min(&need)) as f32 / (2 * need) as f32;
        let value = if *on >= need && *off >= need {
            tf("power.on_off_done", &[("on", &on.to_string()), ("off", &off.to_string())])
        } else {
            tf("power.on_off", &[("on", &on.min(&need).to_string()), ("off", &off.min(&need).to_string()), ("need", &need.to_string())])
        };
        ui.bar_row(&tf("power.screen_n", &[("n", &(i + 1).to_string())]), f, &value, if f >= 1.0 { p.charge } else { ui.t.accent });
    }
    // the learning curve: |error| of each new measurement before it was learned
    if l.errors.len() >= 2 {
        let abs: Vec<f32> = l.errors.iter().map(|e| e.abs()).collect();
        let scale = abs.iter().cloned().fold(2.0f32, f32::max);
        let norm: Vec<f32> = abs.iter().map(|v| v / scale).collect();
        ui.space(2.0);
        let (x, y, w, h) = ui.chart_box(34.0);
        let lw = ui.t.s(1.5) as f32;
        ui.c.area_chart(x, y, w, h, &norm, ui.t.accent, ui.t.style.chart_fill(), lw);
        let recent = abs.iter().rev().take(5).sum::<f32>() / abs.len().min(5) as f32;
        let curve = t("power.curve");
        let last = format!("{} \u{00B7} {}", tf("power.recent_err", &[("err", &format!("{recent:.1}"))]), tf("popup.scale", &[("rate", &format!("{scale:.0} W"))]));
        ui.legend(&[(&curve, ui.t.accent), (&last, ui.t.dim.alpha(0.0))]);
    }
    let accuracy = match l.rms {
        Some(e) => tf("power.accuracy_value", &[("err", &format!("{e:.1}")), ("n", &l.checks.to_string())]),
        None => t("power.not_yet"),
    };
    let start = ui.y;
    row(ui, &t("power.accuracy"), None, &accuracy);
    ui.hover_tip(start, t("power.accuracy_tip"));
    // what to do next
    let mins = minutes_up(l.min_secs);
    let mut hint = if l.ready {
        t("power.hint_ready")
    } else if on_ac {
        tf("power.hint_unplug", &[("min", &mins), ("points", &l.points_needed.to_string())])
    } else {
        tf("power.hint_learning", &[("min", &mins), ("points", &l.points_needed.to_string())])
    };
    for (i, (_, on, off)) in l.panels.iter().enumerate() {
        let (need_on, need_off) = (l.min_per_state.saturating_sub(*on), l.min_per_state.saturating_sub(*off));
        let screen = (i + 1).to_string();
        let part = match (need_on, need_off) {
            (0, 0) => None,
            (on, 0) => Some(tf("power.hint_panel_on", &[("n", &screen), ("on", &on.to_string())])),
            (0, off) => Some(tf("power.hint_panel_off", &[("n", &screen), ("off", &off.to_string())])),
            (on, off) => Some(tf("power.hint_panel", &[("n", &screen), ("on", &on.to_string()), ("off", &off.to_string())])),
        };
        if let Some(part) = part {
            hint.push(' ');
            hint.push_str(&part);
        }
    }
    ui.space(2.0);
    let f = ui.font(9.5, FW_NORMAL);
    wrap(ui, &hint, f, if l.ready { ui.t.dim.alpha(0.85) } else { ui.t.text.alpha(0.9) });
}

// ---------------------------------------------------------------- history

/// Value of the series at time `at`: interpolated between the two neighbouring points when they
/// are close enough together, else None (a gap: the app was not running).
fn value_at(points: &[HistPoint], at: f64, pick: impl Fn(&HistPoint) -> f32) -> Option<f32> {
    let i = points.partition_point(|p| (p.at as f64) < at);
    let max_gap = (HIST_STEP * 3) as f64;
    let prev = i.checked_sub(1).map(|j| &points[j]);
    let next = points.get(i);
    let v = match (prev, next) {
        (Some(a), Some(b)) if (b.at - a.at) as f64 <= max_gap => {
            let span = (b.at - a.at).max(1) as f64;
            let k = ((at - a.at as f64) / span).clamp(0.0, 1.0) as f32;
            let (va, vb) = (pick(a), pick(b));
            if va.is_finite() && vb.is_finite() {
                va + (vb - va) * k
            } else if (at - a.at as f64) < (b.at as f64 - at) {
                va
            } else {
                vb
            }
        }
        (Some(a), _) if at - (a.at as f64) <= HIST_STEP as f64 * 1.5 => pick(a),
        (_, Some(b)) if (b.at as f64) - at <= HIST_STEP as f64 * 1.5 => pick(b),
        _ => return None,
    };
    v.is_finite().then_some(v)
}

/// Series sampled per pixel column across the last `window` seconds before `now`.
fn columns(points: &[HistPoint], now: u64, window: u64, w: i32, pick: impl Fn(&HistPoint) -> f32 + Copy) -> Vec<Option<f32>> {
    (0..w)
        .map(|c| {
            let at = now as f64 - window as f64 + (c as f64 + 0.5) / w as f64 * window as f64;
            value_at(points, at, pick)
        })
        .collect()
}

/// Section header with the 1 h / 6 h / 24 h range buttons on the right.
fn range_header(ui: &mut Ui, title: &str, hours: u32, big: bool) {
    ui.space(10.0);
    let f = ui.font(9.5, FW_SEMIBOLD);
    let (tw0, h) = ui.c.draw_text(&title.to_uppercase(), 0, ui.y, f, ui.t.dim);
    // larger / smaller, next to the title
    // in the popup: open the charts in their own window (that window has all ranges)
    if !big {
        let fl = ui.font(9.0, FW_NORMAL);
        let lx = tw0 + ui.t.s(10.0);
        let link = t("power.chart_bigger");
        let (lw, _) = ui.c.draw_text(&link, lx, ui.y + ui.t.s(1.0), fl, ui.t.accent);
        ui.hits.push(Hit { x: lx, y: ui.y, w: lw, h, action: Action::Track });
    }
    let fb = ui.font(8.5, FW_SEMIBOLD);
    let (pad_x, gap) = (ui.t.s(6.0), ui.t.s(4.0));
    let bh = h + ui.t.s(2.0);
    let by = ui.y - ui.t.s(1.0);
    let mut x = ui.w;
    let opts: &[u32] = if big { &[168, 72, 24, 6, 1, 0] } else { &[24, 6, 1] };
    for &opt in opts {
        let label = match opt {
            0 => t("power.since_unplug"),
            h if h >= 48 => tf("power.days", &[("n", &(h / 24).to_string())]),
            h => tf("power.hours", &[("n", &h.to_string())]),
        };
        let (tw, th) = ui.c.measure_text(&label, fb);
        let bw = tw + pad_x * 2;
        x -= bw;
        let r = ui.t.s(2.5) as f32;
        let on = opt == hours;
        if on {
            ui.c.fill_round_rect(x, by, bw, bh, r, ui.t.text.alpha(0.16));
        } else {
            ui.c.stroke_round_rect_r(x, by, bw, bh, r, ui.t.dim.alpha(0.5));
        }
        ui.c.draw_text(&label, x + pad_x, by + (bh - th) / 2, fb, if on { ui.t.text } else { ui.t.dim.alpha(0.7) });
        ui.hits.push(Hit { x, y: by, w: bw, h: bh, action: Action::BatteryRange(opt) });
        x -= gap;
    }
    ui.y += h + ui.t.s(8.0);
}

/// Seconds to add to unix time for local wall-clock time.
fn local_offset() -> i64 {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let st = unsafe { GetLocalTime() };
    let local_day = st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64;
    // the difference within a day, folded into -12 h .. +14 h
    let mut off = (local_day - now.rem_euclid(86_400)).rem_euclid(86_400);
    if off > 14 * 3600 {
        off -= 86_400;
    }
    // whole quarter hours (time zones), the seconds between the two reads dropped
    (off as f64 / 900.0).round() as i64 * 900
}

/// Round clock times inside the window for the time axis: (unix time, "21:00").
fn time_ticks(now: u64, window: u64) -> Vec<(u64, String)> {
    let step: i64 = match window / 3600 {
        0..=1 => 900,
        2..=6 => 3600,
        _ => 4 * 3600,
    };
    let off = local_offset();
    let start = now as i64 - window as i64;
    let mut at = ((start + off).div_euclid(step) + 1) * step - off;
    let mut out = Vec::new();
    while at < now as i64 {
        let local = (at + off).rem_euclid(86_400);
        out.push((at as u64, format!("{:02}:{:02}", local / 3600, local % 3600 / 60)));
        at += step;
    }
    out
}

/// Pixel column of time `at` in a chart of width `w` covering `window` seconds before `now`.
fn time_x(at: u64, now: u64, window: u64, w: i32) -> i32 {
    ((at as f64 - (now as f64 - window as f64)) / window as f64 * w as f64).round() as i32
}

/// Faint vertical lines at the time ticks.
fn time_grid(ui: &mut Ui, (x, y, w, h): (i32, i32, i32, i32), ticks: &[(u64, String)], now: u64, window: u64) {
    for (at, _) in ticks {
        let tx = x + time_x(*at, now, window, w);
        ui.c.fill_rect(tx, y + 1, 1, h - 2, ui.t.dim.alpha(0.12));
    }
}

/// A small dim label inside a chart (scale values), left-aligned at (x, y).
fn chart_label(ui: &mut Ui, text: &str, x: i32, y: i32) {
    let f = ui.font(8.0, FW_NORMAL);
    ui.c.draw_text(text, x, y, f, ui.t.dim.alpha(0.85));
}

/// The cursor over a chart: a line at the mouse, a dot on the value and "21:34 · 72 %".
/// `y_of` maps a value to its pixel row; the chart is registered so the mouse is followed.
fn chart_cursor(ui: &mut Ui, (x, y, w, h): (i32, i32, i32, i32), series: &[Option<f32>], now: u64, window: u64, y_of: &dyn Fn(f32) -> f32, text: &dyn Fn(f32) -> String, col: &dyn Fn(f32) -> Color) {
    ui.hits.push(Hit { x, y, w, h, action: Action::Track });
    let Some((mx, my)) = ui.mouse else { return };
    if mx < x || mx >= x + w || my < y || my >= y + h {
        return;
    }
    let i = (mx - x).clamp(0, w - 1) as usize;
    ui.c.fill_rect(mx, y + 1, 1, h - 2, ui.t.text.alpha(0.45));
    let at = now as f64 - window as f64 + (i as f64 + 0.5) / w as f64 * window as f64;
    let local = (at as i64 + local_offset()).rem_euclid(86_400);
    let when = format!("{:02}:{:02}", local / 3600, local % 3600 / 60);
    let label = match series.get(i).copied().flatten() {
        Some(v) => {
            let c = col(v);
            ui.c.fill_circle(mx as f32 + 0.5, y_of(v), ui.t.s(3.0) as f32, c);
            format!("{when} \u{00B7} {}", text(v))
        }
        None => format!("{when} \u{00B7} {}", t("power.no_data")),
    };
    // the label above the chart's top edge, beside the line, kept inside the chart's width
    let f = ui.font(9.0, FW_SEMIBOLD);
    let (tw, th) = ui.c.measure_text(&label, f);
    let pad = ui.t.s(4.0);
    let bw = tw + pad * 2;
    let bh = th + ui.t.s(2.0);
    let bx = if mx + ui.t.s(6.0) + bw <= x + w { mx + ui.t.s(6.0) } else { mx - ui.t.s(6.0) - bw }.max(x);
    let by = y + ui.t.s(2.0);
    let bg = if ui.t.light { Color::rgba(0xFF, 0xFF, 0xFF, 235) } else { Color::rgba(0x20, 0x20, 0x20, 235) };
    ui.c.fill_round_rect(bx, by, bw, bh, ui.t.s(3.0) as f32, bg);
    ui.c.stroke_round_rect_r(bx, by, bw, bh, ui.t.s(3.0) as f32, ui.t.dim.alpha(0.4));
    ui.c.draw_text(&label, bx + pad, by + (bh - th) / 2, f, ui.t.text);
}

/// When the last stretch on battery began (the newest run of discharging points).
fn unplugged_at(points: &[HistPoint]) -> Option<u64> {
    let last = points.iter().rposition(|p| p.watts < -0.05)?;
    let mut start = points[last].at;
    for p in points[..=last].iter().rev() {
        if p.watts < -0.05 {
            start = p.at;
        } else {
            break;
        }
    }
    Some(start)
}

/// Figures for the large view: average and peak discharge, energy, % used, a full charge's
/// runtime at that average.
fn history_figures(ui: &mut Ui, points: &[HistPoint], from: u64, now: u64, full_wh: Option<f32>) {
    let pts: Vec<&HistPoint> = points.iter().filter(|p| p.at >= from && p.at <= now).collect();
    let dis: Vec<f32> = pts.iter().filter(|p| p.watts < -0.05).map(|p| -p.watts).collect();
    ui.space(4.0);
    if dis.is_empty() {
        let f = ui.font(9.5, FW_NORMAL);
        wrap(ui, &t("power.no_discharge"), f, ui.t.dim);
        return;
    }
    let avg = dis.iter().sum::<f32>() / dis.len() as f32;
    let peak = dis.iter().copied().fold(0.0f32, f32::max);
    let secs = dis.len() as u64 * HIST_STEP;
    let wh = avg * secs as f32 / 3600.0;
    // level lost while discharging (charging in between does not count against it)
    let used: f32 = pts.windows(2).filter(|w| w[1].watts < -0.05 && w[1].at - w[0].at <= HIST_STEP * 3).map(|w| (w[0].level - w[1].level).max(0.0)).sum();
    ui.row(&t("power.stat_avg"), &watts(avg));
    ui.row(&t("power.stat_peak"), &watts(peak));
    ui.row(&t("power.stat_energy"), &tf("power.stat_energy_val", &[("wh", &format!("{wh:.1}")), ("time", &format_duration(secs as u32))]));
    ui.row(&t("power.stat_pct"), &format!("{:.0} %", used * 100.0));
    if let Some(full) = full_wh.filter(|f| *f > 1.0) {
        ui.row(&t("power.stat_runtime"), &format_duration((full / avg * 3600.0) as u32));
    }
}

/// The charge level and battery power charts; `heights` in DIP. `big`: the chart window.
pub(super) fn history(ui: &mut Ui, points: &[HistPoint], now: u64, big: bool, full_wh: Option<f32>, heights: (f32, f32)) {
    let p = palette(ui.t.light);
    let now = if now == 0 { points.last().map_or(0, |p| p.at) } else { now };
    // the small view knows 1 / 6 / 24 h; the large one also 3 and 7 days and "since unplugged"
    let hours = match ui.t.battery_hours {
        h @ (1 | 6 | 24) => h,
        h if big => h,
        _ => 24,
    };
    let window = if hours == 0 { unplugged_at(points).map_or(24 * 3600, |s| now.saturating_sub(s).max(600)) } else { hours as u64 * 3600 };
    range_header(ui, &t("power.history"), hours, big);
    let ticks = time_ticks(now, window);

    // charge level, 0..100 %, with lines at 50 % and the clock times
    let (x, y, w, h) = ui.chart_box(heights.0);
    ui.c.fill_rect(x + 1, y + h / 2, w - 2, 1, ui.t.dim.alpha(0.15));
    time_grid(ui, (x, y, w, h), &ticks, now, window);
    let level = columns(points, now, window, w, |p| p.level);
    let line_w = ui.t.s(1.5) as f32;
    let mut prev: Option<(f32, f32)> = None;
    for (i, v) in level.iter().enumerate() {
        let cx = x + i as i32;
        match v {
            Some(v) => {
                let vh = v.clamp(0.0, 1.0) * h as f32;
                ui.c.fill_bar(cx, y + h, 1, vh, p.charge.alpha(0.22));
                let pt = (cx as f32 + 0.5, (y + h) as f32 - vh);
                if let Some(q) = prev {
                    ui.c.draw_line(q.0, q.1, pt.0, pt.1, line_w, p.charge);
                }
                prev = Some(pt);
            }
            None => prev = None,
        }
    }
    let lh = ui.c.measure_text("0", ui.font(8.0, FW_NORMAL)).1;
    chart_label(ui, "100 %", x + ui.t.s(3.0), y + ui.t.s(1.0));
    chart_label(ui, "50 %", x + ui.t.s(3.0), y + h / 2 + ui.t.s(1.0));
    let _ = lh;
    let charge_col = p.charge;
    chart_cursor(ui, (x, y, w, h), &level, now, window, &|v| (y + h) as f32 - v.clamp(0.0, 1.0) * h as f32, &|v| pct(v), &|_| charge_col);
    let lo_hi = level.iter().flatten().fold(None, |acc: Option<(f32, f32)>, v| Some(acc.map_or((*v, *v), |(a, b)| (a.min(*v), b.max(*v)))));
    let range = lo_hi.map(|(a, b)| format!("{} \u{2013} {}", pct(a), pct(b))).unwrap_or_else(|| t("power.collecting"));
    let level_label = t("power.level_history");
    ui.legend(&[(&level_label, p.charge), (&range, ui.t.dim.alpha(0.0))]);

    // battery power, signed around a centre line: charging up, discharging down
    let (x, y, w, h) = ui.chart_box(heights.1);
    let power = columns(points, now, window, w, |p| p.watts);
    let scale = power.iter().flatten().fold(10.0f32, |m, v| m.max(v.abs()));
    let mid = y as f32 + h as f32 / 2.0;
    ui.c.fill_rect(x, mid.round() as i32, w, 1, ui.t.dim.alpha(0.3));
    time_grid(ui, (x, y, w, h), &ticks, now, window);
    let mut prev: Option<(f32, f32)> = None;
    for (i, v) in power.iter().enumerate() {
        let cx = x + i as i32;
        match v {
            Some(v) => {
                let dy = v / scale * (h as f32 / 2.0 - 1.0);
                let col = if *v >= 0.0 { p.charge } else { p.discharge };
                if *v >= 0.0 {
                    ui.c.fill_bar(cx, mid.round() as i32, 1, dy, col.alpha(0.25));
                } else {
                    ui.c.fill_rect(cx, mid.round() as i32 + 1, 1, (-dy).round() as i32, col.alpha(0.25));
                }
                let pt = (cx as f32 + 0.5, mid - dy);
                if let Some(q) = prev {
                    ui.c.draw_line(q.0, q.1, pt.0, pt.1, line_w, col);
                }
                prev = Some(pt);
            }
            None => prev = None,
        }
    }
    // the scale on the chart: top = charging at full scale, bottom = discharging
    let lh = ui.c.measure_text("0", ui.font(8.0, FW_NORMAL)).1;
    chart_label(ui, &format!("+{scale:.0} W"), x + ui.t.s(3.0), y + ui.t.s(1.0));
    chart_label(ui, "0", x + ui.t.s(3.0), mid.round() as i32 - lh - ui.t.s(1.0));
    chart_label(ui, &format!("\u{2212}{scale:.0} W"), x + ui.t.s(3.0), y + h - lh - ui.t.s(1.0));
    let (pc, pd) = (p.charge, p.discharge);
    let half = h as f32 / 2.0 - 1.0;
    chart_cursor(ui, (x, y, w, h), &power, now, window, &|v| mid - v / scale * half, &|v| if v >= 0.0 { format!("+{v:.1} W") } else { format!("\u{2212}{:.1} W", -v) }, &|v| if v >= 0.0 { pc } else { pd });
    // the time axis under both charts
    let f = ui.font(8.0, FW_NORMAL);
    let mut last_end = i32::MIN;
    for (at, label) in &ticks {
        let (tw, th) = ui.c.measure_text(label, f);
        let tx = (x + time_x(*at, now, window, w) - tw / 2).clamp(x, x + w - tw);
        if tx > last_end + ui.t.s(6.0) {
            ui.c.draw_text(label, tx, ui.y + ui.t.s(1.0), f, ui.t.dim.alpha(0.85));
            last_end = tx + tw;
        }
        let _ = th;
    }
    ui.y += ui.c.measure_text("0", f).1 + ui.t.s(5.0);
    let (charge, discharge) = (t("power.charging_series"), t("power.discharging_series"));
    ui.legend(&[(&charge, p.charge), (&discharge, p.discharge)]);
    if big {
        history_figures(ui, points, now.saturating_sub(window), now, full_wh);
    }
}

/// The most a battery machine can draw, W: a little over its charger's rating where it reports
/// one, else what the largest laptop chargers give. A reading above it (a sensor glitch) neither
/// stretches the draw chart's scale nor leaves the chart.
fn draw_ceiling(tel: Option<&Telemetry>) -> f32 {
    tel.map_or(330.0, powerflow::ceiling)
}

/// The machine's draw over time, stacked by consumer: the chip parts (measured), screens and
/// conversion (learned) and the remainder of the total. Where the total is unknown (on AC before
/// the model has learned) only the metered chip parts are stacked. `ceiling`: see `draw_ceiling`.
pub(super) fn draw_history(ui: &mut Ui, points: &[HistPoint], now: u64, big: bool, height: f32, ceiling: f32) {
    let now = if now == 0 { points.last().map_or(0, |p| p.at) } else { now };
    let hours = match ui.t.battery_hours {
        h @ (1 | 6 | 24) => h,
        h if big => h,
        _ => 24,
    };
    let window = if hours == 0 { unplugged_at(points).map_or(24 * 3600, |s| now.saturating_sub(s).max(600)) } else { hours as u64 * 3600 };
    // the title, aligned with the history header on the left
    ui.space(10.0);
    let f = ui.font(9.5, FW_SEMIBOLD);
    let (_, th) = ui.c.draw_text(&t("power.draw_history").to_uppercase(), 0, ui.y, f, ui.t.dim);
    ui.y += th + ui.t.s(6.0);
    let ticks = time_ticks(now, window);
    let (x, y, w, h) = ui.chart_box(height);
    // a point drawing more than the machine can (a glitch, or one saved before the ceiling) is
    // left out altogether rather than drawn
    let sane = move |p: &HistPoint| p.split.total.is_nan() || p.split.total <= ceiling;
    let series: Vec<Vec<Option<f32>>> = (0..6).map(|k| columns(points, now, window, w, move |p| if sane(p) && p.split.parts[k] <= ceiling { p.split.parts[k] } else { f32::NAN })).collect();
    let total = columns(points, now, window, w, move |p| if sane(p) { p.split.total } else { f32::NAN });
    // per column: the stacked sum (the total where known, else what is metered)
    let stack: Vec<Option<f32>> = (0..w as usize)
        .map(|i| {
            let s: f32 = series.iter().filter_map(|s| s[i]).sum();
            let any = series.iter().any(|s| s[i].is_some());
            total[i].map(|t| t.max(s)).or(any.then_some(s))
        })
        .collect();
    // the scale follows the highest plausible reading; a glitch above the ceiling stays at the top
    let peak = stack.iter().flatten().fold(0.0f32, |m, v| m.max(*v));
    let scale = ((peak / 5.0).ceil() * 5.0).max(5.0);
    ui.c.fill_rect(x + 1, y + h / 2, w - 2, 1, ui.t.dim.alpha(0.15));
    time_grid(ui, (x, y, w, h), &ticks, now, window);
    let pc = part_colors(ui.t.light);
    // slot -> colour: cores, graphics, chip rest, screens, conversion, other
    let cols = [pc[0], pc[1], pc[2], pc[4], pc[5], pc[3]];
    let bottom = (y + h - 1) as f32;
    let px = (h - 2) as f32 / scale;
    for i in 0..w as usize {
        let mut cum = 0.0f32;
        for (k, s) in series.iter().enumerate() {
            let Some(v) = s[i].filter(|v| *v > 0.0) else { continue };
            let top_px = (y + 1) as f32;
            let (lo, hi) = ((bottom - cum * px).max(top_px).round() as i32, (bottom - (cum + v) * px).max(top_px).round() as i32);
            if lo > hi {
                ui.c.fill_rect(x + i as i32, hi, 1, lo - hi, cols[k].alpha(0.55));
                ui.c.fill_rect(x + i as i32, hi, 1, 1, cols[k]);
            }
            cum += v;
        }
    }
    let lh = ui.c.measure_text("0", ui.font(8.0, FW_NORMAL)).1;
    chart_label(ui, &format!("{scale:.0} W"), x + ui.t.s(3.0), y + ui.t.s(1.0));
    chart_label(ui, &format!("{:.0} W", scale / 2.0), x + ui.t.s(3.0), y + h / 2 + ui.t.s(1.0));
    let _ = lh;
    let text = ui.t.text;
    chart_cursor(ui, (x, y, w, h), &stack, now, window, &|v| bottom - v * px, &|v| format!("{v:.1} W"), &|_| text);
    // the time axis
    let f = ui.font(8.0, FW_NORMAL);
    let mut last_end = i32::MIN;
    for (at, label) in &ticks {
        let (tw, _) = ui.c.measure_text(label, f);
        let tx = (x + time_x(*at, now, window, w) - tw / 2).clamp(x, x + w - tw);
        if tx > last_end + ui.t.s(6.0) {
            ui.c.draw_text(label, tx, ui.y + ui.t.s(1.0), f, ui.t.dim.alpha(0.85));
            last_end = tx + tw;
        }
    }
    ui.y += ui.c.measure_text("0", f).1 + ui.t.s(5.0);
    // the legend with each part's average over the range, wrapped to the width
    let names = [t("power.draw_cpu"), t("power.draw_graphics"), t("power.part_uncore"), t("power.part_screens"), t("power.draw_conversion"), t("power.draw_other")];
    let avg = |s: &[Option<f32>]| {
        let v: Vec<f32> = s.iter().flatten().copied().collect();
        (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
    };
    let fl = ui.font(9.5, FW_NORMAL);
    let r = ui.t.s(3.0) as f32;
    let (mut lx, mut row_h) = (0, 0);
    let mut shown = 0;
    for (k, s) in series.iter().enumerate() {
        let Some(a) = avg(s) else { continue };
        shown += 1;
        let label = format!("{} {}", names[k], if a < 9.95 { format!("{a:.1} W") } else { format!("{a:.0} W") });
        let (tw, th) = ui.c.measure_text(&label, fl);
        let item = ui.t.s(9.0) + tw;
        if lx > 0 && lx + item > ui.w {
            lx = 0;
            ui.y += row_h + ui.t.s(3.0);
        }
        row_h = th;
        ui.c.fill_circle(lx as f32 + r, ui.y as f32 + th as f32 / 2.0, r, cols[k]);
        ui.c.draw_text(&label, lx + ui.t.s(9.0), ui.y, fl, ui.t.dim);
        lx += item + ui.t.s(12.0);
    }
    if shown == 0 {
        let c = t("power.collecting");
        let (_, th) = ui.c.draw_text(&c, 0, ui.y, fl, ui.t.dim);
        row_h = th;
    }
    ui.y += row_h + ui.t.s(4.0);
    // what the numbers are
    let known_total = total.iter().any(|v| v.is_some());
    let fs = ui.font(9.0, FW_NORMAL);
    wrap(ui, &t(if known_total { "power.draw_note" } else { "power.draw_note_chip" }), fs, ui.t.dim.alpha(0.85));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_names_are_shortened() {
        assert_eq!(short_hw("Intel(R) Core(TM) Ultra 9 285H"), "Core Ultra 9 285H");
        assert_eq!(short_hw("Intel(R) Arc(TM) 140T GPU (16GB)"), "Arc 140T");
        assert_eq!(short_hw("Intel(R) Wi-Fi 7 BE201 320MHz"), "Wi-Fi 7 BE201");
        assert_eq!(short_hw("AMD Ryzen 7 7840U w/ Radeon 780M Graphics"), "Ryzen 7 7840U w/ Radeon 780M Graphics");
        assert_eq!(panel_maker("SDC"), "Samsung Display");
    }

    fn pts(v: &[(u64, f32)]) -> Vec<HistPoint> {
        v.iter().map(|(at, l)| HistPoint { at: *at, level: *l, watts: f32::NAN, split: crate::metrics::powerflow::Split::NONE }).collect()
    }

    #[test]
    fn history_interpolates_between_close_points() {
        let p = pts(&[(1000, 0.5), (1030, 0.6)]);
        let v = value_at(&p, 1015.0, |p| p.level).unwrap();
        assert!((v - 0.55).abs() < 1e-4);
    }

    #[test]
    fn history_leaves_gaps_where_the_app_was_not_running() {
        let p = pts(&[(1000, 0.5), (5000, 0.9)]);
        assert_eq!(value_at(&p, 3000.0, |p| p.level), None);
        // a point still covers its own neighbourhood
        assert_eq!(value_at(&p, 1020.0, |p| p.level), Some(0.5));
    }

    #[test]
    fn unknown_power_stays_a_gap() {
        let p = pts(&[(1000, 0.5), (1030, 0.6)]);
        assert_eq!(value_at(&p, 1015.0, |p| p.watts), None);
    }

    /// `cargo test battery_previews -- --ignored`: renders the view for several power states
    /// (plus this machine's live reading) to target/previews/*.bmp.
    #[test]
    #[ignore]
    fn battery_previews() {
        use crate::metrics::power::{MeterChannel, PowerMonitor};
        use crate::widgets::{ColorMode, Theme};
        use std::sync::Arc;
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("previews");
        std::fs::create_dir_all(&out).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        // 6 h: discharge, a gap (app closed), then charging
        let mut hist = Vec::new();
        for i in 0..720u64 {
            let at = now - (720 - i) * HIST_STEP;
            if (300..340).contains(&i) {
                continue;
            }
            let (level, watts) = if i < 300 { (0.95 - i as f32 * 0.0018, -9.0 - (i as f32 * 0.07).sin() * 4.0) } else { (0.41 + (i - 340) as f32 * 0.0014, 58.0 - (i - 340) as f32 * 0.08) };
            // the split: a busy CPU on battery, idle graphics, the learned screens and losses
            let total = if watts < 0.0 { -watts } else { 14.0 + (i as f32 * 0.05).sin() * 3.0 };
            let cores = 2.5 + (i as f32 * 0.11).sin().abs() * 3.0;
            let parts = [cores, 0.6, 1.4, 3.1, 1.2, (total - cores - 0.6 - 1.4 - 3.1 - 1.2).max(0.0)];
            hist.push(HistPoint { at, level, watts, split: crate::metrics::powerflow::Split { total, parts } });
        }
        let cell = |rate: i32, cap: u32| Cell { name: "Battery".into(), manufacturer: "Example".into(), chemistry: "LIon".into(), capacity_mwh: Some(cap), full_mwh: Some(71_916), design_mwh: Some(75_076), voltage_mv: Some(12_660), rate_mw: Some(rate), cycles: Some(52), charging: rate > 0, ..Default::default() };
        let pkg = MeterChannel { name: "RAPL_Package0_PKG".into(), watts: 6.42, role: MeterRole::CpuPackage };
        let psys = MeterChannel { name: "RAPL_Package0_PSYS".into(), watts: 14.6, role: MeterRole::Platform };
        let mut live = None;
        // the live reading from the worker
        let mut mon = PowerMonitor::start(1000);
        // the energy meter reports watts from its second read on: wait for a sample with them
        mon.set_active(true, 1000);
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Some((t, _)) = mon.take() {
                let done = !t.meters.is_empty();
                live = Some((*t).clone());
                if done {
                    break;
                }
            }
        }
        let mut cases: Vec<(&str, Telemetry)> = vec![
            ("charging", Telemetry { at_unix: now, level: Some(0.8), on_ac: Some(true), cells: vec![cell(58_180, 57_533)], meters: vec![pkg.clone()], ..Default::default() }),
            ("charging_meter", Telemetry { at_unix: now, level: Some(0.8), on_ac: Some(true), cells: vec![cell(58_180, 57_533)], meters: vec![psys.clone(), pkg.clone()], source: Some((SourceKind::Usb, Some(5000))), ..Default::default() }),
            ("battery", Telemetry { at_unix: now, level: Some(0.62), on_ac: Some(false), remaining_secs: Some(3 * 3600 + 1200), cells: vec![cell(-14_600, 44_588)], meters: vec![pkg], saver: true, ..Default::default() }),
        ];
        let rapl = || vec![MeterChannel { name: "RAPL_Package0_PKG".into(), watts: 14.73, role: MeterRole::CpuPackage }, MeterChannel { name: "RAPL_Package0_PP0".into(), watts: 9.12, role: MeterRole::Other }, MeterChannel { name: "RAPL_Package0_PP1".into(), watts: 0.21, role: MeterRole::Other }, MeterChannel { name: "RAPL_Package0_DRAM".into(), watts: 0.0, role: MeterRole::Other }];
        let learning = |n: u32, on: u32, off: u32| {
            let mut m = crate::metrics::powermodel::Learning { n, min_points: 20, min_per_state: 10, panels: vec![("ABC1234#UID12345".into(), on, off)], errors: vec![6.2, 4.8, 5.1, 3.0, 2.2, 2.6, 1.7, 1.2, 1.5, 0.9, 1.1, 0.8], ..Default::default() };
            let deficits = 10u32.saturating_sub(on) + 10u32.saturating_sub(off);
            m.points_needed = 20u32.saturating_sub(n).max(deficits);
            m.ready = m.points_needed == 0;
            m.min_secs = if m.points_needed == 0 { 0 } else { 150 + 30 * (m.points_needed - 1) };
            if m.ready {
                m.rms = Some(1.1);
                m.checks = 14;
            }
            m
        };
        cases.push(("ac_learning", Telemetry { at_unix: now, level: Some(0.88), on_ac: Some(true), cells: vec![cell(12_850, 63_300)], meters: rapl(), learning: Some(learning(10, 3, 10)), ..Default::default() }));
        cases.push(("ac_ready", Telemetry { at_unix: now, level: Some(0.92), on_ac: Some(true), cells: vec![cell(16_640, 66_300)], meters: rapl(), estimate: Some((27.9, Some(1.1))), learning: Some(learning(34, 12, 22)), ..Default::default() }));
        cases.push(("ac_floor", Telemetry { at_unix: now, level: Some(0.92), on_ac: Some(true), cells: vec![cell(16_640, 66_300)], meters: rapl(), ..Default::default() }));
        cases.push(("ac_estimate", Telemetry { at_unix: now, level: Some(0.92), on_ac: Some(true), cells: vec![cell(16_640, 66_300)], meters: rapl(), last_on_battery: Some((14.6, now - 12 * 60)), estimate: Some((27.9, Some(1.1))), ..Default::default() }));
        cases.push(("ac_earlier", Telemetry { at_unix: now, level: Some(0.8), on_ac: Some(true), cells: vec![cell(0, 57_533)], last_on_battery: Some((14.6, now - 12 * 60)), ..Default::default() }));
        if let Some(l) = live {
            cases.push(("live", l));
        }
        for (name, tel) in cases {
            for light in [false, true] {
                for lang in ["en", "zh"] {
                    if lang == "zh" && light {
                        continue;
                    }
                    crate::i18n::set_language(lang);
                    let (text, dim) = if light { (Color::rgb(0x1B, 0x1B, 0x1B), Color::rgba(0x1B, 0x1B, 0x1B, 160)) } else { (Color::rgb(255, 255, 255), Color::rgba(255, 255, 255, 150)) };
                    let theme = Theme {
                        text, dim, accent: if light { Color::rgb(0x00, 0x5F, 0xB8) } else { Color::rgb(0x60, 0xCD, 0xFF) }, outline: None, light, scale: 1.5, inner_h: 20, labels: true, font_scale: 1.0, split_pe: true, chart_w: 34.0, bar_w: 3.0,
                        modes: [ColorMode::Accent; crate::widgets::MODULE_COUNT], label: None, net_rx: true, net_tx: true, net_bits: false, net_upload_first: false, temp_f: false, animate: true, battery_hours: 6, style: crate::style::Style::Windows, battery_view: Default::default(),
                    };
                    let w = theme.s(super::super::BATTERY_WIDTH_DIP - 28.0);
                    let mut c = Canvas::new();
                    c.resize(w, theme.s(1500.0));
                    c.clear();
                    let mut icons = crate::procicon::IconCache::default();
                    let mut locks = Vec::new();
                    let mut frozen = None;
                    let mut expanded = std::collections::HashSet::new();
                    // the light previews show the large history view
                    if light {
                        expanded.insert("hist_big".to_string());
                    }
                    let m = MetricsData {
                        battery: Some(crate::metrics::battery::Battery { level: tel.level.unwrap_or(0.0), charging: tel.charging(), on_ac: tel.on_ac.unwrap_or(false), remaining_secs: tel.remaining_secs }),
                        // a learned split, as a finished model gives it
                        power_tel: Some(Arc::new({
                            let mut t2 = tel.clone();
                            t2.parts = Some(crate::metrics::powermodel::Parts { screens: 3.1, conversion: 1.2, base: 2.0 });
                            t2
                        })),
                        power_history: Arc::new(hist.clone()),
                        cpu_name: "Intel(R) Core(TM) Ultra 9 285H".into(),
                        screen_off: Some((300, 180)),
                        keep_awake: true,
                        sleeps: vec![crate::metrics::sleepdrain::SleepEvent { from: 1_700_000_000, to: 1_700_033_000, pct_from: 0.81, pct_to: 0.77, mwh_from: Some(58_300), mwh_to: Some(55_400) }],
                        ..Default::default()
                    };
                    let mut ui = Ui { c: &mut c, t: &theme, w, y: 0, hover: None, pie: None, tips: Vec::new(), hits: Vec::new(), icons: &mut icons, indent: 0, locks: &mut locks, frozen: &mut frozen, expanded: &expanded, proc_filter: (true, true), mem_bar: false, gpu_by_vram: false, charts: Vec::new(), last_chart: (0, 0, 0, 0), ages: (0.0, 1.0), mouse: None, copied: None, anim: None, anim_bg: Vec::new(), phase: 0.4, max_rows: None, ping_edit: None, flipped: Box::leak(Box::new(std::collections::HashSet::new())), fold_open: false, fold: Vec::new(), order: &[], drag: None, frows: Vec::new() };
                    draw(&mut ui, &m);
                    // a second redraw of the same popup: what each second costs once it is open
                    let y_was = ui.y;
                    ui.y = 0;
                    let t_draw = std::time::Instant::now();
                    draw(&mut ui, &m);
                    println!("redraw time {name}: {:.1} ms", t_draw.elapsed().as_secs_f64() * 1000.0);
                    ui.y = y_was;
                    let used = ui.y;
                    // the card as the popup composes it: padding around the content
                    let pad = theme.s(14.0);
                    let mut card = Canvas::new();
                    card.resize(w + pad * 2, used + pad * 2);
                    card.clear();
                    card.blit_rect(&c, 0, 0, c.w, c.h, pad, pad);
                    let bg = if light { Color::rgb(0xF3, 0xF3, 0xF3) } else { Color::rgb(0x2B, 0x2B, 0x2B) };
                    let file = format!("{name}_{}{}.bmp", if light { "light" } else { "dark" }, if lang == "zh" { "_zh" } else { "" });
                    card.save_bmp(&out.join(file), bg).unwrap();
                }
            }
        }
        crate::i18n::set_language("en");
        println!("previews in {}", out.display());
    }

    /// `cargo test chart_page_timing -- --ignored --nocapture`: what one redraw of the chart
    /// window costs with a full week of history, and its picture in target/previews.
    #[test]
    #[ignore]
    fn chart_page_timing() {
        use crate::widgets::{ColorMode, Theme};
        use std::sync::Arc;
        crate::i18n::set_language("en");
        let now = 1_800_000_000u64;
        let hist: Vec<HistPoint> = (0..crate::metrics::power::HIST_LEN as u64)
            .map(|i| {
                let at = now - (crate::metrics::power::HIST_LEN as u64 - i) * HIST_STEP;
                let x = i as f32;
                let total = 12.0 + (x * 0.01).sin() * 4.0;
                let parts = [3.0 + (x * 0.03).sin().abs() * 3.0, 0.6, 1.4, 3.1, 1.2, 2.0];
                HistPoint { at, level: 0.5 + (x * 0.002).sin() * 0.4, watts: (x * 0.004).sin() * 20.0, split: crate::metrics::powerflow::Split { total, parts } }
            })
            .collect();
        for hours in [1u32, 168] {
            let theme = Theme {
                text: Color::rgb(255, 255, 255), dim: Color::rgba(255, 255, 255, 150), accent: Color::rgb(0x60, 0xCD, 0xFF), outline: None, light: false, scale: 1.5, inner_h: 20, labels: true, font_scale: 1.0, split_pe: true, chart_w: 34.0, bar_w: 3.0,
                modes: [ColorMode::Accent; crate::widgets::MODULE_COUNT], label: None, net_rx: true, net_tx: true, net_bits: false, net_upload_first: false, temp_f: false, animate: true, battery_hours: hours, style: crate::style::Style::Windows, battery_view: Default::default(),
            };
            let m = MetricsData { power_tel: Some(Arc::new(Telemetry { at_unix: now, ..Default::default() })), power_history: Arc::new(hist.clone()), ..Default::default() };
            let (w, h) = (1400, 1050);
            let mut c = Canvas::new();
            let mut icons = crate::procicon::IconCache::default();
            let (mut locks, mut frozen) = (Vec::new(), None);
            let expanded = std::collections::HashSet::new();
            let flipped = std::collections::HashSet::new();
            let mut times = Vec::new();
            for i in 0..5 {
                let t0 = std::time::Instant::now();
                c.resize(w, h);
                c.clear();
                let mut ui = Ui { c: &mut c, t: &theme, w, y: 0, hover: None, pie: None, tips: Vec::new(), hits: Vec::new(), icons: &mut icons, indent: 0, locks: &mut locks, frozen: &mut frozen, expanded: &expanded, proc_filter: (true, true), mem_bar: false, gpu_by_vram: false, charts: Vec::new(), last_chart: (0, 0, 0, 0), ages: (0.0, 1.0), mouse: Some((300 + i * 40, 200)), copied: None, anim: None, anim_bg: Vec::new(), phase: 0.0, max_rows: None, ping_edit: None, flipped: &flipped, fold_open: false, fold: Vec::new(), order: &[], drag: None, frows: Vec::new() };
                chart_page(&mut ui, &m, 520.0);
                times.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            println!("chart page {hours} h: {:?} ms", times.iter().map(|t| (t * 10.0).round() / 10.0).collect::<Vec<_>>());
            let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("previews");
            std::fs::create_dir_all(&out).unwrap();
            c.save_bmp(&out.join(format!("charts_{hours}h.bmp")), Color::rgb(0x20, 0x20, 0x20)).unwrap();
        }
    }

    /// `cargo test remote_mac_preview -- --ignored`: a Mac's battery popup from its agent's data
    /// (9-30 W system draw, on the charger), in target/previews/remote_mac.bmp.
    #[test]
    #[ignore]
    fn remote_mac_preview() {
        use crate::widgets::{ColorMode, Theme};
        use std::sync::Arc;
        crate::i18n::set_language("ru");
        let now = 1_800_000_000u64;
        let hist: Vec<HistPoint> = [30.0f32, 24.4, 19.0, 17.5, 22.0, 26.0, 18.0]
            .iter()
            .enumerate()
            .map(|(i, w)| HistPoint { at: now - (7 - i as u64) * HIST_STEP, level: 1.0, watts: *w, split: crate::metrics::powerflow::Split { total: *w, parts: [f32::NAN, f32::NAN, f32::NAN, f32::NAN, f32::NAN, *w] } })
            .collect();
        let cell = crate::metrics::power::Cell { capacity_mwh: Some(85_600), full_mwh: Some(88_000), design_mwh: Some(108_900), voltage_mv: Some(12_526), rate_mw: Some(0), cycles: Some(614), temp_c: Some(33.0), on_line: true, ..Default::default() };
        let meters = vec![crate::metrics::power::MeterChannel { name: "System".into(), watts: 18.0, role: MeterRole::Platform }];
        let tel = Telemetry { at_unix: now, cells: vec![cell], level: Some(1.0), on_ac: Some(true), source: Some((SourceKind::Ac, Some(4990))), source_w: Some(140), meters, ..Default::default() };
        let theme = Theme {
            text: Color::rgb(255, 255, 255), dim: Color::rgba(255, 255, 255, 150), accent: Color::rgb(0x60, 0xCD, 0xFF), outline: None, light: false, scale: 1.0, inner_h: 20, labels: true, font_scale: 1.0, split_pe: true, chart_w: 34.0, bar_w: 3.0,
            modes: [ColorMode::Accent; crate::widgets::MODULE_COUNT], label: None, net_rx: true, net_tx: true, net_bits: false, net_upload_first: false, temp_f: false, animate: false, battery_hours: 6, style: crate::style::Style::Windows, battery_view: Default::default(),
        };
        let m = MetricsData { host: "mac".into(), power_tel: Some(Arc::new(tel)), power_history: Arc::new(hist), battery: Some(crate::metrics::battery::Battery { level: 1.0, charging: false, on_ac: true, remaining_secs: None }), ..Default::default() };
        let (w, h) = (560, 900);
        let mut c = Canvas::new();
        c.resize(w, h);
        c.clear();
        let mut icons = crate::procicon::IconCache::default();
        let (mut locks, mut frozen) = (Vec::new(), None);
        let expanded = std::collections::HashSet::new();
        let flipped = std::collections::HashSet::new();
        let mut ui = Ui { c: &mut c, t: &theme, w, y: 0, hover: None, pie: None, tips: Vec::new(), hits: Vec::new(), icons: &mut icons, indent: 0, locks: &mut locks, frozen: &mut frozen, expanded: &expanded, proc_filter: (true, true), mem_bar: false, gpu_by_vram: false, charts: Vec::new(), last_chart: (0, 0, 0, 0), ages: (0.0, 1.0), mouse: None, copied: None, anim: None, anim_bg: Vec::new(), phase: 0.0, max_rows: None, ping_edit: None, flipped: &flipped, fold_open: false, fold: Vec::new(), order: &[], drag: None, frows: Vec::new() };
        draw(&mut ui, &m);
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("previews");
        std::fs::create_dir_all(&out).unwrap();
        c.save_bmp(&out.join("remote_mac.bmp"), Color::rgb(0x20, 0x20, 0x20)).unwrap();
        crate::i18n::set_language("en");
    }

    #[test]
    fn earlier_reading_only_on_ac_and_while_fresh() {
        crate::i18n::set_language("en");
        let mut tel = Telemetry { at_unix: 10_000, on_ac: Some(true), last_on_battery: Some((14.6, 10_000 - 720)), ..Default::default() };
        assert_eq!(earlier_reading(&tel), Some((14.6, "on battery, 12 min ago".to_string())));
        tel.on_ac = Some(false);
        assert_eq!(earlier_reading(&tel), None, "on battery the live reading is shown");
        tel.on_ac = Some(true);
        tel.at_unix += EARLIER_MAX_SECS;
        assert_eq!(earlier_reading(&tel), None, "too old");
    }

    fn rapl(pkg: f32, pp0: Option<f32>, pp1: Option<f32>, dram: Option<f32>) -> Telemetry {
        use crate::metrics::power::MeterChannel;
        let mut meters = vec![MeterChannel { name: "RAPL_Package0_PKG".into(), watts: pkg, role: MeterRole::CpuPackage }];
        for (n, v) in [("RAPL_Package0_PP0", pp0), ("RAPL_Package0_PP1", pp1), ("RAPL_Package0_DRAM", dram)] {
            if let Some(v) = v {
                meters.push(MeterChannel { name: n.into(), watts: v, role: MeterRole::Other });
            }
        }
        Telemetry { meters, on_ac: Some(true), ..Default::default() }
    }

    #[test]
    fn chip_splits_into_cores_graphics_and_the_rest_of_the_package() {
        // the WattFlow screenshot: package 27.2 = cores 21.1 + graphics 0.1 + 6.0
        let c = chip(&rapl(27.2, Some(21.1), Some(0.1), Some(0.0)));
        assert_eq!(c.cores, Some(21.1));
        assert_eq!(c.graphics, Some(0.1));
        assert!((c.soc.unwrap() - 6.0).abs() < 1e-4);
        assert!((c.total.unwrap() - 27.2).abs() < 1e-4);
        // memory counts towards the SoC part and the total
        let c = chip(&rapl(10.0, Some(6.0), Some(1.0), Some(2.0)));
        assert!((c.soc.unwrap() - 5.0).abs() < 1e-4);
        assert!((c.total.unwrap() - 12.0).abs() < 1e-4);
        assert!(c.dram_metered);
        assert!(!chip(&rapl(10.0, Some(6.0), Some(1.0), Some(0.0))).dram_metered, "a rail reading 0 meters nothing");
        // no core rail: the package as one part
        let c = chip(&rapl(10.0, None, None, None));
        assert_eq!((c.cores, c.package), (None, Some(10.0)));
        // a core rail above the package is a glitch
        assert_eq!(chip(&rapl(10.0, Some(30.0), None, None)).cores, None);
    }

    #[test]
    fn system_basis_prefers_measurement_then_estimate_then_floor() {
        let f = |sys: Reading| Flow { mode: Mode::AcIdle, adapter: Reading::Unavailable, battery: Reading::Measured(0.0), system: sys };
        let mut tel = rapl(10.0, None, None, Some(1.0));
        assert_eq!(system_basis(&tel, &f(Reading::Measured(30.0)), Some(11.0)), SystemBasis::Measured(30.0));
        assert_eq!(system_basis(&tel, &f(Reading::Unavailable), Some(11.0)), SystemBasis::Floor(11.0));
        tel.estimate = Some((19.0, Some(1.0)));
        assert_eq!(system_basis(&tel, &f(Reading::Unavailable), Some(11.0)), SystemBasis::Estimated(19.0, Some(1.0)));
        // an estimate below the metered chip is not believed
        tel.estimate = Some((9.0, None));
        assert_eq!(system_basis(&tel, &f(Reading::Unavailable), Some(11.0)), SystemBasis::Floor(11.0));
    }
}
