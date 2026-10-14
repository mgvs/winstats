//! Click popup: a top-level layered window anchored to the clicked widget
//! showing details for that module. Content is drawn with the same Canvas as the strip.

use crate::canvas::{Canvas, Color, FW_NORMAL, FW_SEMIBOLD, FontSpec, GREEN, RED, YELLOW};
use crate::metrics::MetricsData;
use crate::util::{format_bytes, format_bytes_rate, format_duration, format_net_rate, pcwstr, wide};
use crate::i18n::{t, tf};
use crate::widgets::{Kind, Theme};
use std::time::Instant;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};
use windows::Win32::Foundation::{HINSTANCE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, GetDC, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT, ReleaseDC};
use windows::Win32::UI::WindowsAndMessaging::*;

const WIDTH_DIP: f32 = 300.0;
const PAD_DIP: f32 = 14.0;
const RADIUS_DIP: f32 = 10.0;
const SHADOW_DIP: f32 = 14.0;

pub const DOWN: Color = Color::rgb(0x2E, 0x9C, 0xF6);
pub const UP: Color = Color::rgb(0xF4, 0x43, 0x36);
pub const READ: Color = Color::rgb(0x9C, 0x6A, 0xF6);
pub const WRITE: Color = Color::rgb(0xFF, 0x98, 0x00);

/// timer id of the copy flash on the popup window
pub const COPY_TIMER: usize = 7;
const COPY_FLASH_SECS: f32 = 0.8;

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
    /// pin a process row (its icon becomes a kill button); the index is the row in the list
    LockProc(usize, crate::metrics::processes::ProcInfo),
    /// terminate the process
    KillProc(u32),
    /// show / hide the download (true) or upload series in the network chart
    ToggleNetSeries(bool),
    /// put the text on the clipboard (IP, MAC)
    Copy(String),
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

/// A pinned process row: keeps showing this process (the last known values once it drops out of
/// the top list) and offers the kill button in place of the icon.
struct Lock {
    row: usize,
    snap: crate::metrics::processes::ProcInfo,
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
    /// text just copied to the clipboard and when: its segment lights up for a moment
    copied: Option<(String, Instant)>,
    /// pinned process rows
    locks: Vec<Lock>,
    /// the process list as shown when the mouse entered it: rows keep their places while the
    /// mouse is over the list so a click lands on the row that was aimed at
    frozen: Option<Vec<crate::metrics::processes::ProcInfo>>,
    /// index of the hit under the mouse; a change redraws (hover highlights)
    hit_hover: Option<usize>,
    /// mouse position in content coords while it is over the popup
    mouse: Option<(i32, i32)>,
    /// hovered truncated label, drawn as a tooltip on the next render
    tip: Option<usize>,
    /// content offset inside the window (set by render)
    pad: i32,
    canvas: Canvas,
    content: Canvas,
    /// executable icons for the process lists
    icons: crate::procicon::IconCache,
    /// layered window under the popup carrying its soft shadow (the non-layered modes cannot
    /// paint outside their own rectangle); transparent to the mouse, never activated
    shadow: HWND,
    shadow_canvas: Canvas,
    pub kind: Option<Kind>,
    /// index of the strip widget the popup belongs to: a click on it closes the popup, a click
    /// on another widget of the same module just hands the popup over
    widget: Option<usize>,
    /// remote machine key the popup shows; None = local
    pub source: Option<String>,
    pub visible: bool,
    pub hidden_at: Option<Instant>,
    anchor: RECT,
    tray: RECT,
    light: bool,
    /// height fixed while the popup stays open, so content changes do not make it jump
    locked_h: i32,
    /// what the fixed height was computed for: row counts that change the layout (an adapter
    /// or a drive came or went); a change lets the height be recomputed
    shape: Option<(Kind, usize, usize, usize)>,
    /// how the window background is drawn: DWM acrylic or the self-drawn layered card
    /// the best background this Windows offers (Layered when DWM refused)
    native: crate::backdrop::Mode,
    /// the background in use: `native`, or the accent policy for "more transparent"
    backdrop: crate::backdrop::Mode,
    /// what the DWM background was last set up for: mode, dark theme, opacity
    applied: Option<(crate::backdrop::Mode, bool, crate::backdrop::Opacity)>,
    /// background opacity from the menu
    pub opacity: crate::backdrop::Opacity,
    /// for re-creating the window: DWM does not take a backdrop change on a window that has had one
    hinstance: HINSTANCE,
    class: String,
}

/// The popup window with the background for `opacity`; returns it with the mode in use and the
/// native mode, which drops to Layered when DWM refuses every backdrop.
fn create_window(hinstance: HINSTANCE, class: &str, native: crate::backdrop::Mode, opacity: crate::backdrop::Opacity) -> Option<(HWND, crate::backdrop::Mode, crate::backdrop::Mode)> {
    use crate::backdrop::{Mode, apply, mode_for};
    let cls = wide(class);
    let title = wide("winstats popup");
    let want = mode_for(native, opacity);
    let layered = if want == Mode::Layered { WS_EX_LAYERED } else { WINDOW_EX_STYLE(0) };
    // DWM gives a backdrop only to framed windows; the frame itself is cut away in WM_NCCALCSIZE
    let style = if want == Mode::Layered { WS_POPUP } else { WS_POPUP | WS_THICKFRAME };
    let hwnd = unsafe {
        CreateWindowExW(layered | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, pcwstr(&cls), pcwstr(&title), style, 0, 0, 10, 10, None, None, hinstance, None).ok()?
    };
    if want == Mode::Layered || apply(hwnd, want, false, opacity) {
        return Some((hwnd, want, native));
    }
    if want != native && apply(hwnd, native, false, opacity) {
        crate::log!("popup backdrop {want:?} refused, using {native:?}");
        return Some((hwnd, native, native));
    }
    crate::log!("popup backdrop {want:?} refused, falling back to a layered window");
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, WS_POPUP.0 as _);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST).0 as _);
    }
    Some((hwnd, Mode::Layered, Mode::Layered))
}

impl Popup {
    pub fn new(hinstance: HINSTANCE, class: &str, opacity: crate::backdrop::Opacity) -> Option<Self> {
        let (hwnd, backdrop, native) = create_window(hinstance, class, crate::backdrop::mode(), opacity)?;
        let shadow = unsafe {
            let cls = wide("WinStatsShadow");
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                pcwstr(&cls),
                pcwstr(&cls),
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
            .unwrap_or_default()
        };
        Some(Self {
            hwnd,
            shadow,
            shadow_canvas: Canvas::new(),
            hinstance,
            class: class.to_string(),
            native,
            backdrop,
            applied: None,
            opacity,
            canvas: Canvas::new(),
            content: Canvas::new(),
            icons: Default::default(),
            kind: None,
            widget: None,
            source: None,
            hover: None,
            pie: None,
            tips: Vec::new(),
            hits: Vec::new(),
            copied: None,
            locks: Vec::new(),
            frozen: None,
            hit_hover: None,
            mouse: None,
            tip: None,
            pad: 0,
            visible: false,
            hidden_at: None,
            anchor: RECT::default(),
            tray: RECT::default(),
            light: false,
            locked_h: 0,
            shape: None,
        })
    }

    /// Called from the strip's click handler. `anchor` and `tray` are screen rects.
    pub fn toggle(&mut self, kind: Kind, source: Option<String>, widget: usize, anchor: RECT, tray: RECT) -> bool {
        let just_hidden = self.hidden_at.map(|t| t.elapsed().as_millis() < 300).unwrap_or(false);
        // "open" also covers the popup the click itself just deactivated (and so hid)
        let open = self.visible || (!self.visible && just_hidden);
        let same = self.kind == Some(kind) && self.source == source;
        if open && same && self.widget == Some(widget) {
            if self.visible {
                self.hide("toggle");
            }
            return false;
        }
        if open && same {
            // another widget of the same module: the popup stays where it is and now belongs
            // to that widget; re-shown in place when the click had deactivated it
            self.widget = Some(widget);
            if self.visible {
                return false;
            }
            self.visible = true;
            return true;
        }
        self.kind = Some(kind);
        self.widget = Some(widget);
        self.source = source;
        self.anchor = anchor;
        self.tray = tray;
        self.visible = true;
        self.locked_h = 0;
        self.hover = None;
        self.pie = None;
        self.tips.clear();
        self.hits.clear();
        self.tip = None;
        self.hit_hover = None;
        self.mouse = None;
        if !same {
            self.locks.clear();
        }
        self.frozen = None;
        true
    }

    /// Light up the segment holding `text` (just copied); a timer redraws until the flash ends.
    pub fn flash_copied(&mut self, text: &str) {
        self.copied = Some((text.to_string(), Instant::now()));
        unsafe {
            SetTimer(self.hwnd, COPY_TIMER, 30, None);
        }
    }

    /// Progress of the copy flash in 0..1; ends it (and its timer) when over.
    fn copy_progress(&mut self) -> Option<(String, f32)> {
        let (text, at) = self.copied.clone()?;
        let p = at.elapsed().as_secs_f32() / COPY_FLASH_SECS;
        if p >= 1.0 {
            self.copied = None;
            unsafe {
                let _ = KillTimer(self.hwnd, COPY_TIMER);
            }
            return None;
        }
        Some((text, p))
    }

    /// The content changed shape (an interface came or went): let the next render pick a new
    /// height instead of keeping the one from when the popup opened.
    pub fn relayout(&mut self) {
        self.locked_h = 0;
    }

    /// Pin / unpin a process row (a click on the row).
    pub fn toggle_lock(&mut self, row: usize, p: crate::metrics::processes::ProcInfo) {
        if let Some(i) = self.locks.iter().position(|l| l.row == row) {
            self.locks.remove(i);
        } else {
            self.locks.retain(|l| l.snap.pid != p.pid);
            self.locks.push(Lock { row, snap: p });
        }
    }

    pub fn unlock(&mut self, pid: u32) {
        self.locks.retain(|l| l.snap.pid != pid);
    }

    /// The mouse left the window: drop the hover state; true when a redraw is needed.
    pub fn mouse_leave(&mut self) -> bool {
        let changed = self.mouse.is_some() || self.tip.is_some() || self.hover.is_some();
        self.mouse = None;
        self.tip = None;
        self.hover = None;
        self.hit_hover = None;
        self.frozen = None;
        changed
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
                let _ = ShowWindow(self.shadow, SW_HIDE);
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
        self.mouse = Some((cx, cy));
        let over = self.hits.iter().position(|h| cx >= h.x && cx < h.x + h.w && cy >= h.y && cy < h.y + h.h);
        if over != self.hit_hover {
            self.hit_hover = over;
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

    /// WM_PAINT of the DWM-backed window: put the last frame back.
    pub fn paint(&mut self) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            if self.backdrop != crate::backdrop::Mode::Layered {
                self.canvas.paint_dc(hdc);
            }
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    pub fn render(&mut self, t: &Theme, light: bool, m: &MetricsData, first: bool) {
        let Some(kind) = self.kind else { return };
        if !self.visible {
            return;
        }
        self.light = light;
        let w = t.s(WIDTH_DIP);
        let pad = t.s(PAD_DIP);
        // the layered window gets no DWM shadow: a soft one is painted around the card instead
        let layered = self.native == crate::backdrop::Mode::Layered;
        let blur = t.s(SHADOW_DIP);
        let sh_dy = t.s(2.0);
        let sh = if layered { blur + sh_dy } else { 0 };
        if !layered {
            // system material <-> accent policy: only a new window takes the other one
            if crate::backdrop::mode_for(self.native, self.opacity) != self.backdrop {
                if let Some((hwnd, backdrop, native)) = create_window(self.hinstance, &self.class, self.native, self.opacity) {
                    unsafe {
                        let _ = DestroyWindow(self.hwnd);
                    }
                    (self.hwnd, self.backdrop, self.native) = (hwnd, backdrop, native);
                    self.applied = None;
                }
            }
            let key = (self.backdrop, !light, self.opacity);
            if self.applied != Some(key) {
                self.applied = Some(key);
                crate::backdrop::apply(self.hwnd, self.backdrop, !light, self.opacity);
            }
        }

        let shape = Some((kind, m.net_ifaces.len(), m.disks.len(), m.drives.len()));
        if shape != self.shape {
            self.shape = shape;
            self.locked_h = 0;
        }
        // pass 1: content on a tall transparent canvas
        self.content.resize(w - pad * 2, t.s(900.0));
        self.content.clear();
        let copied = self.copy_progress();
        let mut ui = Ui { c: &mut self.content, t, w: w - pad * 2, y: 0, hover: self.hover, pie: None, tips: Vec::new(), hits: Vec::new(), icons: &mut self.icons, indent: 0, locks: &mut self.locks, frozen: &mut self.frozen, mouse: self.mouse, copied };
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
            p.cx += (sh + pad) as f32;
            p.cy += (sh + pad) as f32;
            p
        });
        self.tips = std::mem::take(&mut ui.tips);
        self.hits = std::mem::take(&mut ui.hits);
        self.pad = sh + pad;
        if self.tip.map_or(false, |i| i >= self.tips.len()) {
            self.tip = None;
        }
        self.draw_tooltip(t, light, w - pad * 2);
        let content_h = end_y.max(self.content.used_height()).max(t.s(40.0));
        // never shrink while open; grow only if content really needs more room
        let h = (content_h + pad * 2).max(self.locked_h);
        self.locked_h = h;

        // pass 2: background + content
        self.canvas.resize(w + sh * 2, h + sh * 2);
        self.canvas.clear();
        let card = |a: u8| if light { Color::rgba(0xF9, 0xF9, 0xF9, a) } else { Color::rgba(0x20, 0x20, 0x20, a) };
        let bg = card(self.opacity.card_alpha());
        let r = t.s(RADIUS_DIP) as f32;
        let overlay = self.opacity.overlay();
        if self.backdrop == crate::backdrop::Mode::System && overlay > 0.0 {
            // the system material cannot be thickened: a theme fill over it does that
            self.canvas.fill_rect(0, 0, w, h, card((overlay * 255.0).round() as u8));
        }
        if layered {
            // like the DWM one: just a faint wide halo, no dark rim
            let shadow = Color::rgba(0, 0, 0, if light { 26 } else { 70 });
            self.canvas.fill_shadow(sh, sh, w, h, r, blur as f32, sh_dy, shadow);
            self.canvas.fill_round_rect(sh, sh, w, h, r, bg);
            self.canvas.stroke_round_rect_r(sh, sh, w, h, r, t.dim.alpha(0.35));
        }
        self.canvas.blit(&self.content, sh + pad, sh + pad);

        // position: above (or below) the taskbar, centred on the clicked widget
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = unsafe {
            let hm = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let _ = GetMonitorInfoW(hm, &mut mi);
            mi.rcMonitor
        };
        let margin = t.s(8.0);
        let tray_vertical = self.tray.bottom - self.tray.top > self.tray.right - self.tray.left;
        let (x, y) = if tray_vertical {
            // taskbar at the left or right edge: beside it, centred on the clicked widget
            let cy = (self.anchor.top + self.anchor.bottom) / 2;
            let y = (cy - h / 2).clamp(mon.top + margin, (mon.bottom - margin - h).max(mon.top + margin));
            let x = if self.tray.left > mon.left + 10 { self.tray.left - w - margin } else { self.tray.right + margin };
            (x, y)
        } else {
            let cx = (self.anchor.left + self.anchor.right) / 2;
            let x = (cx - w / 2).clamp(mon.left + margin, (mon.right - margin - w).max(mon.left + margin));
            let y = if self.tray.top > mon.top + 10 { self.tray.top - h - margin } else { self.tray.bottom + margin };
            (x, y)
        };

        unsafe {
            let flags = if first { SWP_SHOWWINDOW } else { SWP_SHOWWINDOW | SWP_NOACTIVATE };
            let (x, y) = (x - sh, y - sh);
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w + sh * 2, h + sh * 2, flags);
            if layered {
                if let Err(e) = self.canvas.present_at(self.hwnd, x, y) {
                    crate::log!("popup UpdateLayeredWindow failed: {e}");
                }
            } else {
                let hdc = GetDC(self.hwnd);
                self.canvas.paint_dc(hdc);
                ReleaseDC(self.hwnd, hdc);
                // the Windows 11 transient backdrop brings the faint flyout shadow of its own;
                // the Windows 10 accent policy does not, so a faint one goes into the layered
                // window underneath
                if self.backdrop == crate::backdrop::Mode::Accent {
                    let halo = blur + sh_dy;
                    let shadow = Color::rgba(0, 0, 0, if light { 16 } else { 48 });
                    self.shadow_canvas.resize(w + halo * 2, h + halo * 2);
                    self.shadow_canvas.clear();
                    self.shadow_canvas.fill_shadow(halo, halo, w, h, r, blur as f32, sh_dy, shadow);
                    let _ = SetWindowPos(self.shadow, self.hwnd, x - halo, y - halo, w + halo * 2, h + halo * 2, SWP_NOACTIVATE | SWP_SHOWWINDOW);
                    if let Err(e) = self.shadow_canvas.present_at(self.shadow, x - halo, y - halo) {
                        crate::log!("shadow UpdateLayeredWindow failed: {e}");
                    }
                } else {
                    let _ = ShowWindow(self.shadow, SW_HIDE);
                }
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
    icons: &'a mut crate::procicon::IconCache,
    /// left offset of row labels (room for an icon in the process lists)
    indent: i32,
    locks: &'a mut Vec<Lock>,
    frozen: &'a mut Option<Vec<crate::metrics::processes::ProcInfo>>,
    /// mouse position in content coords, when over the popup
    mouse: Option<(i32, i32)>,
    /// text just copied and the flash progress 0..1
    copied: Option<(String, f32)>,
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
        self.title_r(title, subtitle, "");
    }

    /// Title with a subtitle line that also carries a short right-aligned text.
    fn title_r(&mut self, title: &str, subtitle: &str, right: &str) {
        let f = self.font(15.0, FW_SEMIBOLD);
        let (_, h) = self.c.draw_text(title, 0, self.y, f, self.t.text);
        self.y += h;
        if !subtitle.is_empty() || !right.is_empty() {
            let f = self.font(9.5, FW_NORMAL);
            let mut max_w = self.w;
            if !right.is_empty() {
                let (rw, _) = self.c.measure_text(right, f);
                self.c.draw_text(right, self.w - rw, self.y, f, self.t.dim);
                max_w -= rw + self.t.s(8.0);
            }
            let (_, h) = self.clipped(subtitle, 0, self.y, max_w, f, self.t.dim);
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
        self.clipped(label, self.indent, self.y, self.w - self.indent - vw - self.t.s(8.0), f, label_color);
        self.c.draw_text(value, self.w - vw, self.y, f, color);
        self.y += h + self.t.s(3.0);
    }

    /// Process row: name, an optional small dim tag in front of the value, the value.
    fn proc_row(&mut self, label: &str, tag: Option<&str>, value: &str) {
        let f = self.font(11.0, FW_NORMAL);
        let (vw, h) = self.c.measure_text(value, f);
        let mut right = self.w - vw - self.t.s(8.0);
        if let Some(tag) = tag {
            let ft = self.font(9.0, FW_NORMAL);
            let (tw, th) = self.c.measure_text(tag, ft);
            right -= tw;
            self.c.draw_text(tag, right, self.y + (h - th) / 2, ft, self.t.dim.alpha(0.8));
            right -= self.t.s(6.0);
        }
        self.clipped(label, self.indent, self.y, right - self.indent, f, self.t.dim);
        self.c.draw_text(value, self.w - vw, self.y, f, self.t.text);
        self.y += h + self.t.s(3.0);
    }

    /// Dim line of segments joined with `sep`; a segment with a copy text is a click-to-copy
    /// target with its own hit and a "Copy" tooltip.
    fn info_row(&mut self, segs: &[(String, Option<String>)], sep: &str, color: Color) {
        let f = self.font(11.0, FW_NORMAL);
        let h = self.c.measure_text("X", f).1;
        // flush left, right under the interface name
        let mut x = 0;
        let ft = self.font(9.5, FW_NORMAL);
        for (i, (text, copy)) in segs.iter().enumerate() {
            if i > 0 {
                let (w, _) = self.c.draw_text(sep, x, self.y, f, color);
                x += w;
            }
            let flash = match (&self.copied, copy) {
                (Some((ct, p)), Some(c)) if ct == c => Some(*p),
                _ => None,
            };
            let mut col = color;
            if let Some(p) = flash {
                // accent glow behind the text that fades out
                let (tw, _) = self.c.measure_text(text, f);
                let pad = self.t.s(3.0);
                self.c.fill_round_rect(x - pad, self.y, tw + pad * 2, h, pad as f32, self.t.accent.alpha(0.4 * (1.0 - p)));
                col = self.t.text;
            }
            let (w, _) = self.c.draw_text_clipped(text, x, self.y, (self.w - x).max(1), f, col);
            if let Some(c) = copy {
                self.hits.push(Hit { x, y: self.y, w, h, action: Action::Copy(c.clone()) });
                let tip = if flash.is_some() { t("popup.copied") } else { t("popup.copy") };
                self.tips.push(Tip { x, y: self.y, w, h, font: ft, text: tip });
            }
            x += w;
            if x >= self.w {
                break;
            }
        }
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
        self.clipped(label, self.indent, self.y, label_w - self.indent, f, self.t.text);
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
    /// Per-core bars. The two threads of an SMT core sit close together with the virtual one
    /// drawn lighter; hovering a bar names the core and shows its load.
    fn cores(&mut self, cores: &[f32], classes: &[u8], breaks_at: &[bool], smt: &[bool], ids: &[u16]) {
        let n = cores.len().max(1) as i32;
        let (x, y, w, h) = self.chart_box(56.0);
        let pad = self.t.s(4.0);
        let has_smt = smt.iter().any(|s| *s);
        let gap_core = if has_smt { self.t.s(4.0) } else { self.t.s(2.0) };
        let gap_thread = self.t.s(1.0);
        let gap_at = |i: usize| -> i32 {
            if i == 0 {
                0
            } else if smt.get(i).cloned().unwrap_or(false) {
                gap_thread
            } else {
                gap_core
            }
        };
        let gaps: i32 = (0..cores.len()).map(gap_at).sum();
        let breaks = if self.t.split_pe { breaks_at.iter().filter(|b| **b).count() as i32 } else { 0 };
        let extra = self.t.s(4.0);
        let per_break = 2 * extra + 1 + gap_core;
        let bw = ((w - pad * 2 - gaps - breaks * per_break) / n).max(1);
        let total = bw * n + gaps + breaks * per_break;
        let mut bx = x + (w - total) / 2;
        let max_h = (h - pad * 2) as f32;
        let top = classes.iter().cloned().max().unwrap_or(0);
        let hybrid = classes.iter().any(|c| *c != top);
        let f = self.font(9.5, FW_NORMAL);
        for (i, v) in cores.iter().enumerate() {
            let class = classes.get(i).cloned().unwrap_or(top);
            let is_smt = smt.get(i).cloned().unwrap_or(false);
            bx += gap_at(i);
            if self.t.split_pe && breaks_at.get(i).cloned().unwrap_or(false) {
                bx += extra;
                self.c.fill_rect(bx, y + pad, 1, max_h as i32, self.t.dim.alpha(0.35));
                bx += 1 + extra + gap_core;
            }
            let mut col = self.t.chart_color(Kind::Cpu, *v);
            if class != top {
                col = col.alpha(0.6);
            }
            if is_smt {
                col = col.alpha(0.55);
            }
            self.c.fill_rect(bx, y + pad, bw, max_h as i32, self.t.dim.alpha(0.1));
            self.c.fill_bar(bx, y + h - pad, bw, (v * max_h).max(1.0), col);
            let id = ids.get(i).map(|d| d.to_string()).unwrap_or_else(|| i.to_string());
            let mut name = tf("popup.cpu_n", &[("n", &id)]);
            let mut tags: Vec<&str> = Vec::new();
            if hybrid {
                tags.push(if class == top { "P" } else { "E" });
            }
            if is_smt {
                tags.push("SMT");
            }
            if !tags.is_empty() {
                name = format!("{name} ({})", tags.join(", "));
            }
            self.tips.push(Tip { x: bx, y: y + pad, w: bw, h: max_h as i32, font: f, text: format!("{name}: {}", pct(*v)) });
            bx += bw;
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

    /// Process list; always takes the room of `n` rows so the popup height stays stable. Local
    /// lists get the executable's icon in front of the name; a click pins the row and turns the
    /// icon into a kill button (remote machines only send names).
    fn processes(&mut self, list: &[crate::metrics::processes::ProcInfo], mode: ProcMode, n: usize, local: bool) {
        let text_h = self.c.measure_text("X", self.font(11.0, FW_NORMAL)).1;
        let icon = if local { self.t.s(16.0) } else { 0 };
        // at 100% scaling the text is barely taller than an icon, so keep some air between icons
        let gap = self.t.s(3.0).max(icon + self.t.s(7.0) - text_h);
        let row_h = text_h + gap;
        let inset = self.t.s(2.0);
        if !local {
            self.locks.clear();
        }
        // while the mouse is over the list its rows stay where they are (values still update)
        let list_top = self.y - gap / 2;
        let over_list = local && self.mouse.map_or(false, |(mx, my)| mx >= 0 && mx < self.w && my >= list_top && my < list_top + row_h * n as i32);
        let shown: Vec<crate::metrics::processes::ProcInfo> = if over_list {
            let mut shown = self.frozen.clone().unwrap_or_else(|| list.to_vec());
            for p in shown.iter_mut() {
                if let Some(q) = list.iter().find(|q| q.pid == p.pid) {
                    *p = q.clone();
                }
            }
            *self.frozen = Some(shown.clone());
            shown
        } else {
            *self.frozen = None;
            list.to_vec()
        };
        let list = &shown[..];
        let pinned: Vec<u32> = self.locks.iter().map(|l| l.snap.pid).collect();
        if local {
            let alive: Vec<u32> = list.iter().map(|p| p.pid).chain(pinned.iter().cloned()).collect();
            self.icons.retain_pids(&alive);
        }
        if list.is_empty() && self.locks.is_empty() {
            let f = self.font(10.5, FW_NORMAL);
            let text = if mode == ProcMode::Io { t("popup.nothing_io") } else { t("popup.collecting") };
            self.c.draw_text(&text, 0, self.y, f, self.t.dim);
            self.y += row_h * n as i32 - (gap - self.t.s(3.0));
            return;
        }
        let mut rest = list.iter().filter(|p| !pinned.contains(&p.pid));
        for row in 0..n {
            let lock = self.locks.iter().position(|l| l.row == row);
            let p = match lock {
                Some(li) => {
                    // fresh values while the process is still listed, else the last known ones
                    if let Some(p) = list.iter().find(|p| p.pid == self.locks[li].snap.pid) {
                        self.locks[li].snap = p.clone();
                    }
                    self.locks[li].snap.clone()
                }
                None => match rest.next() {
                    Some(p) => p.clone(),
                    None => {
                        self.y += row_h;
                        continue;
                    }
                },
            };
            let top = self.y - gap / 2;
            let locked = lock.is_some();
            let hovered = local && self.mouse.map_or(false, |(mx, my)| mx >= 0 && mx < self.w && my >= top && my < top + row_h);
            if locked || hovered {
                let bg = self.t.text.alpha(if locked { 0.10 } else { 0.05 });
                self.c.fill_round_rect(0, top, self.w, row_h, self.t.s(3.0) as f32, bg);
            }
            if icon > 0 {
                let iy = self.y + (text_h - icon) / 2;
                let box_w = inset * 2 + icon;
                if locked && self.icons.can_kill(p.pid) {
                    // kill button in place of the icon
                    let over = self.mouse.map_or(false, |(mx, my)| mx >= 0 && mx < box_w && my >= iy && my < iy + icon);
                    let col = if over { Color::rgb(0xE5, 0x48, 0x48) } else { self.t.text.alpha(0.75) };
                    let (cx, cy) = ((inset + icon / 2) as f32, (iy + icon / 2) as f32);
                    let r = icon as f32 * 0.24;
                    let lw = (self.t.scale * 1.5).max(1.0);
                    self.c.draw_line(cx - r, cy - r, cx + r, cy + r, lw, col);
                    self.c.draw_line(cx - r, cy + r, cx + r, cy - r, lw, col);
                    let f = self.font(9.5, FW_NORMAL);
                    self.tips.push(Tip { x: 0, y: iy, w: box_w, h: icon, font: f, text: t("popup.kill_process") });
                    self.hits.push(Hit { x: 0, y: iy, w: box_w, h: icon, action: Action::KillProc(p.pid) });
                } else if let Some(img) = self.icons.get(p.pid, icon) {
                    let (w, h, px) = (img.w, img.h, img.px.clone());
                    self.c.draw_image(&px, w, h, inset, iy, icon, icon);
                }
                self.indent = box_w + self.t.s(4.0);
            }
            let value = match mode {
                ProcMode::Cpu => format!("{:.1}%", p.cpu * 100.0),
                ProcMode::Mem => format_bytes(p.mem),
                ProcMode::Io => format_bytes_rate(p.io),
            };
            // a 32-bit process on 64-bit Windows is marked; local only (agents do not report it)
            let tag = if local && self.icons.is_32bit(p.pid) { Some(t("popup.bits32")) } else { None };
            self.proc_row(&p.name, tag.as_deref(), &value);
            // proc_row steps by the text height alone; rows here are taller to fit the icons
            self.y = top + gap / 2 + row_h;
            self.indent = 0;
            if local {
                self.hits.push(Hit { x: 0, y: top, w: self.w, h: row_h, action: Action::LockProc(row, p) });
            }
        }
        // the extra air between icon rows is not wanted below the last one
        self.y -= gap - self.t.s(3.0);
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

/// "1 Gbit/s", "2.5 Gbit/s", "100 Mbit/s"
fn link_speed(bps: u64) -> String {
    let g = bps as f64 / 1e9;
    if g >= 1.0 {
        if (g - g.round()).abs() < 0.05 {
            format!("{} Gbit/s", g.round())
        } else {
            format!("{g:.1} Gbit/s")
        }
    } else if bps >= 1_000_000 {
        format!("{} Mbit/s", bps / 1_000_000)
    } else {
        format!("{} kbit/s", bps / 1000)
    }
}

fn pct(v: f32) -> String {
    format!("{}%", (v * 100.0).round() as i32)
}

// ---------------------------------------------------------------- modules

/// "3.9 GHz" or "800 MHz".
fn clock_text(mhz: u32) -> String {
    if mhz >= 1000 {
        format!("{:.1} GHz", mhz as f64 / 1000.0)
    } else {
        format!("{mhz} MHz")
    }
}

/// "31% · 3.9 GHz" when the clock is known.
fn with_clock(value: String, mhz: Option<u32>) -> String {
    match mhz {
        Some(m) => format!("{value} · {}", clock_text(m)),
        None => value,
    }
}

/// Popup title: "<host> · CPU" for a remote machine, "CPU" locally.
fn head(m: &MetricsData, key: &str) -> String {
    if m.host.is_empty() {
        t(key)
    } else {
        format!("{} · {}", m.host, t(key))
    }
}

/// Subtitle with the offline mark for a machine that stopped answering.
fn sub(m: &MetricsData, s: &str) -> String {
    if !m.offline {
        return s.to_string();
    }
    if s.is_empty() {
        t("popup.offline")
    } else {
        format!("{s} · {}", t("popup.offline"))
    }
}

fn draw_cpu(ui: &mut Ui, m: &MetricsData) {
    let mut line = m.cpu_name.clone();
    if let Some(c) = m.cpu_temp {
        if !line.is_empty() {
            line.push_str(" · ");
        }
        line.push_str(&format!("{c:.0} \u{00B0}C"));
    }
    ui.title(&head(m, "popup.cpu"), &sub(m, &line));
    let hist: Vec<f32> = m.cpu_history.iter().cloned().collect();
    ui.chart(&hist, 70.0, ui.t.accent);
    ui.row_colored(&t("popup.load"), &pct(m.cpu_total), ui.t.usage(m.cpu_total, ui.t.text));
    ui.row(&t("popup.system"), &pct(m.cpu_system));
    ui.row(&t("popup.user"), &pct(m.cpu_user));
    ui.row(&t("popup.idle"), &pct((1.0 - m.cpu_total).max(0.0)));
    if m.load_avg.len() == 3 {
        ui.row(&t("popup.load_avg"), &format!("{:.2} / {:.2} / {:.2}", m.load_avg[0], m.load_avg[1], m.load_avg[2]));
    }
    if m.cpu_sockets > 1 {
        ui.row(&t("popup.sockets"), &m.cpu_sockets.to_string());
    }
    let sockets = if m.cpu_sockets > 1 { tf("popup.sockets_suffix", &[("n", &m.cpu_sockets.to_string())]) } else { String::new() };
    if m.cpu_hybrid() {
        // load and current clock per core class
        let (pn, en, pl, el) = m.cpu_pe();
        let top = m.cpu_classes.iter().cloned().max().unwrap_or(0);
        let low = m.cpu_classes.iter().cloned().min().unwrap_or(0);
        ui.row(&tf("popup.p_cores", &[("n", &pn.to_string())]), &with_clock(pct(pl), m.cpu_clock(Some(top))));
        ui.row(&tf("popup.e_cores", &[("n", &en.to_string())]), &with_clock(pct(el), m.cpu_clock(Some(low))));
        ui.section(&tf("popup.cores_pe", &[("p", &pn.to_string()), ("e", &en.to_string()), ("sockets", &sockets)]));
    } else {
        if let Some(mhz) = m.cpu_clock(None) {
            ui.row(&t("popup.clock"), &clock_text(mhz));
        }
        ui.section(&tf("popup.cores", &[("n", &m.cpu_cores.len().to_string()), ("sockets", &sockets)]));
    }
    let breaks = m.cpu_breaks();
    ui.cores(&m.cpu_cores, &m.cpu_classes, &breaks, &m.cpu_smt, &m.cpu_ids);
    ui.section(&t("popup.top_processes"));
    ui.processes(&m.top_cpu, ProcMode::Cpu, m.top_n, m.host.is_empty());
}

fn draw_mem(ui: &mut Ui, m: &MetricsData) {
    let total = tf("popup.total", &[("size", &format_bytes(m.mem.total))]);
    let mut line = match &m.mem_summary {
        Some(s) => format!("{total} · {s}"),
        None => total,
    };
    if let Some(n) = m.mem_channels {
        line.push_str(&format!(" · {}", tf("popup.channels", &[("n", &n.to_string())])));
    }
    ui.title(&head(m, "popup.memory"), &sub(m, &line));
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
    ui.processes(&m.top_mem, ProcMode::Mem, m.top_n, m.host.is_empty());
}

fn draw_gpu(ui: &mut Ui, m: &MetricsData) {
    ui.title(&head(m, "popup.gpu"), &sub(m, &m.gpu_name));
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

fn draw_net(ui: &mut Ui, m: &MetricsData) {
    let line = if m.net_ip.is_empty() { m.net_iface.clone() } else { format!("{} · {}", m.net_iface, m.net_ip) };
    // link speed of the interface in the title, on the right
    let speed = m.net_ifaces.iter().find(|i| i.alias == m.net_iface).map(|i| i.speed).filter(|s| *s > 0).map(link_speed).unwrap_or_default();
    ui.title_r(&head(m, "popup.network"), &sub(m, &line), &speed);
    // the Download / Upload rows switch their series in the chart on and off
    let (show_rx, show_tx) = (ui.t.net_rx, ui.t.net_tx);
    let bits = ui.t.net_bits;
    let rate = |v| format_net_rate(v, bits);
    let rx: Vec<f32> = if show_rx { m.net_history.iter().map(|v| v.0).collect() } else { Vec::new() };
    let tx: Vec<f32> = if show_tx { m.net_history.iter().map(|v| v.1).collect() } else { Vec::new() };
    let max = ui.chart2(&rx, &tx, 70.0, DOWN, UP, 10.0 * 1024.0);
    let (down, up) = (t("popup.download"), t("popup.upload"));
    ui.legend(&[(&down, ui.on_off(DOWN, show_rx)), (&up, ui.on_off(UP, show_tx)), (&tf("popup.scale", &[("rate", &rate(max as f64))]), ui.t.dim.alpha(0.0))]);
    let start = ui.y;
    ui.row_lv(&down, &rate(m.net_rx), ui.on_off(ui.t.dim, show_rx), ui.on_off(DOWN, show_rx));
    ui.clickable(start, Action::ToggleNetSeries(true));
    let start = ui.y;
    ui.row_lv(&up, &rate(m.net_tx), ui.on_off(ui.t.dim, show_tx), ui.on_off(UP, show_tx));
    ui.clickable(start, Action::ToggleNetSeries(false));
    if !m.net_ping_host.is_empty() {
        // ICMP probe to the configured host while the popup is open
        let ms = |v: Option<f32>| v.map(|v| format!("{v:.1} ms")).unwrap_or_else(|| t("popup.na"));
        ui.section(&tf("popup.ping", &[("host", &m.net_ping_host)]));
        ui.row(&t("popup.latency"), &ms(m.net_latency));
        ui.row(&t("popup.jitter"), &ms(m.net_jitter));
    }
    ui.section(&t("popup.since_start"));
    ui.row(&t("popup.received"), &format_bytes(m.net_total_rx));
    ui.row(&t("popup.sent"), &format_bytes(m.net_total_tx));
    if !m.net_ifaces.is_empty() {
        ui.section(&t("popup.interfaces"));
        for i in &m.net_ifaces {
            let start = ui.y;
            // hardware name (the Windows alias when an agent sends none)
            let name = if i.description.is_empty() { i.alias.clone() } else { i.description.clone() };
            let value = if i.up { format!("{} / {}", rate(i.rx_bps), rate(i.tx_bps)) } else { t("popup.disconnected") };
            let (lc, vc) = (ui.on_off(ui.t.text, i.enabled), ui.on_off(ui.t.text, i.enabled));
            ui.row_lv(&name, &value, lc, if i.up { vc } else { ui.on_off(ui.t.dim, i.enabled) });
            // one line each: IPv4 addresses (every one copies on click), MAC (copies), link speed
            let dc = ui.on_off(ui.t.dim, i.enabled);
            if !i.ssid.is_empty() {
                // Wi-Fi: the network joined
                ui.info_row(&[(i.ssid.clone(), None)], "", dc);
            }
            if !i.ip.is_empty() {
                // shown as "10.10.24.30/24", copied without the prefix length
                let ips: Vec<(String, Option<String>)> = i.ip.split(", ").map(|a| (a.to_string(), Some(a.split('/').next().unwrap_or(a).to_string()))).collect();
                ui.info_row(&ips, ", ", dc);
            }
            if !i.mac.is_empty() {
                ui.info_row(&[(i.mac.clone(), Some(i.mac.clone()))], "", dc);
            }
            if i.speed > 0 {
                // Wi-Fi: "150 Mbit/s (802.11n, 2.4 GHz)"
                let speed = if i.wifi.is_empty() { link_speed(i.speed) } else { format!("{} ({})", link_speed(i.speed), i.wifi) };
                ui.info_row(&[(speed, None)], "", dc);
            }
            ui.clickable(start, Action::ToggleNet(i.alias.clone()));
        }
    }
}

fn draw_disk(ui: &mut Ui, m: &MetricsData) {
    let line = match m.disks.len() {
        0 => String::new(),
        1 => m.disks[0].model.clone(),
        n => tf("popup.physical_disks_n", &[("n", &n.to_string())]),
    };
    ui.title(&head(m, "popup.disk"), &sub(m, &line));
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
                    desc.push_str(" · ");
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
    ui.processes(&m.top_io, ProcMode::Io, m.top_n, m.host.is_empty());
    ui.section(&t("popup.drives"));
    for d in &m.drives {
        let used = d.total.saturating_sub(d.free);
        let frac = used as f32 / d.total.max(1) as f32;
        let mut label = if d.label.is_empty() { d.name.clone() } else { format!("{} {}", d.name, d.label) };
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

fn draw_battery(ui: &mut Ui, m: &MetricsData) {
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
