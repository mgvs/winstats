//! The battery charts in their own resizable window, opened from the popup's charts: the popup
//! stays compact, and the charts get all the room the window has.

use super::{battery_view, Action, Hit, Lock, Ui};
use crate::canvas::{Canvas, Color};
use crate::i18n::t;
use crate::metrics::MetricsData;
use crate::util::{pcwstr, wide};
use crate::widgets::Theme;
use std::collections::HashSet;
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const CLASS: &str = "WinStatsChart";

pub struct ChartWindow {
    hinstance: HINSTANCE,
    pub hwnd: HWND,
    pub visible: bool,
    canvas: Canvas,
    content: Canvas,
    pad: i32,
    /// the picture is out of date: redrawn on the next paint (mouse moves only mark it, so a
    /// burst of them costs one redraw)
    dirty: bool,
    /// the chart height must be measured again (size or data changed)
    remeasure: bool,
    /// the chart height in DIP for the last measured client size
    chart_h: f32,
    size: (i32, i32),
    hits: Vec<Hit>,
    /// mouse in content coords
    mouse: Option<(i32, i32)>,
    // what a Ui borrows; the charts use none of it
    icons: crate::procicon::IconCache,
    locks: Vec<Lock>,
    frozen: Option<Vec<crate::metrics::processes::ProcInfo>>,
    expanded: HashSet<String>,
    flipped: HashSet<String>,
}

impl ChartWindow {
    pub fn new(hinstance: HINSTANCE) -> Self {
        Self {
            hinstance,
            hwnd: HWND::default(),
            visible: false,
            canvas: Canvas::new(),
            content: Canvas::new(),
            pad: 0,
            dirty: true,
            remeasure: true,
            chart_h: 300.0,
            size: (0, 0),
            hits: Vec::new(),
            mouse: None,
            icons: Default::default(),
            locks: Vec::new(),
            frozen: None,
            expanded: HashSet::new(),
            flipped: HashSet::new(),
        }
    }

    /// Open (or bring forward) the window, centred on the screen under the mouse the first time.
    pub fn show(&mut self, t: &Theme, light: bool, m: &MetricsData) {
        if self.hwnd.is_invalid() {
            let cls = wide(CLASS);
            let title = wide(&title());
            let (w, h) = (t.s(980.0), t.s(760.0));
            let (x, y) = unsafe {
                let mut pt = windows::Win32::Foundation::POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mon = windows::Win32::Graphics::Gdi::MonitorFromPoint(pt, windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST);
                let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO { cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32, ..Default::default() };
                let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi);
                let r = mi.rcWork;
                (r.left + ((r.right - r.left) - w).max(0) / 2, r.top + ((r.bottom - r.top) - h).max(0) / 2)
            };
            let hwnd = unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), pcwstr(&cls), pcwstr(&title), WS_OVERLAPPEDWINDOW, x, y, w, h, None, None, self.hinstance, None) };
            let Ok(hwnd) = hwnd else {
                crate::log!("chart window: CreateWindowExW failed");
                return;
            };
            self.hwnd = hwnd;
        }
        unsafe {
            // the title bar follows the theme
            let dark = BOOL(if light { 0 } else { 1 });
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            let _ = ShowWindow(self.hwnd, if IsIconic(self.hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.visible = true;
        self.remeasure = true;
        self.render(t, light, m);
        self.invalidate();
    }

    /// New data or another size: measure the layout again on the next paint.
    pub fn refresh(&mut self) {
        if self.visible {
            self.remeasure = true;
            self.invalidate();
        }
    }

    /// Redraw on the next paint.
    pub fn invalidate(&mut self) {
        self.dirty = true;
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    pub fn dirty(&self) -> bool {
        self.visible && self.dirty
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
            self.mouse = None;
        }
    }

    /// Draw into the back buffer and ask for a paint.
    pub fn render(&mut self, t: &Theme, light: bool, m: &MetricsData) {
        if !self.visible {
            return;
        }
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        if w < 40 || h < 40 {
            return;
        }
        let pad = t.s(20.0);
        self.pad = pad;
        let (cw, ch) = (w - pad * 2, h - pad * 2);
        // the charts take what is left after the labels: measured with a first pass, only when
        // the size or the data changed
        if self.remeasure || self.size != (w, h) {
            let probe = 300.0f32;
            let mouse = self.mouse.take();
            let used = self.draw(t, cw, ch.max(1) * 2, m, probe);
            self.mouse = mouse;
            let extra_dip = (ch - used) as f32 / t.s(100.0) as f32 * 100.0;
            self.chart_h = (probe + extra_dip).max(240.0);
            self.size = (w, h);
            self.remeasure = false;
        }
        self.draw(t, cw, ch.max(1), m, self.chart_h);
        self.dirty = false;
        // background + content
        self.canvas.resize(w, h);
        let bg = if light { Color::rgb(0xF9, 0xF9, 0xF9) } else { Color::rgb(0x20, 0x20, 0x20) };
        self.canvas.fill_rect(0, 0, w, h, bg);
        self.canvas.blit_rect(&self.content, 0, 0, cw, ch, pad, pad);
    }

    /// Lay the charts out on the content canvas; returns the height used.
    fn draw(&mut self, t: &Theme, w: i32, h: i32, m: &MetricsData, chart_h: f32) -> i32 {
        self.content.resize(w, h);
        self.content.clear();
        let mut ui = Ui {
            c: &mut self.content,
            t,
            w,
            y: 0,
            hover: None,
            pie: None,
            tips: Vec::new(),
            hits: Vec::new(),
            icons: &mut self.icons,
            indent: 0,
            locks: &mut self.locks,
            frozen: &mut self.frozen,
            expanded: &self.expanded,
            proc_filter: (true, true),
            mem_bar: false,
            gpu_by_vram: false,
            charts: Vec::new(),
            last_chart: (0, 0, 0, 0),
            ages: (m.sample_age(1, 0), if m.sample_ms > 0.0 { m.sample_ms / 1000.0 } else { 1.0 }),
            mouse: self.mouse,
            copied: None,
            anim: None,
            anim_bg: Vec::new(),
            phase: 0.0,
            max_rows: None,
            ping_edit: None,
            flipped: &self.flipped,
            fold_open: false,
            fold: Vec::new(),
            order: &[],
            drag: None,
            frows: Vec::new(),
        };
        battery_view::chart_page(&mut ui, m, chart_h);
        self.hits = std::mem::take(&mut ui.hits);
        ui.y
    }

    pub fn paint(&mut self) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            self.canvas.paint_dc(hdc);
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    /// Mouse at window coords (None: it left). True when the window needs a redraw.
    pub fn mouse_at(&mut self, p: Option<(i32, i32)>) -> bool {
        let p = p.map(|(x, y)| (x - self.pad, y - self.pad));
        let before = self.mouse;
        self.mouse = p;
        // the cursor on a chart follows the mouse; elsewhere only leaving a chart redraws
        let on_chart = |p: Option<(i32, i32)>| p.map_or(false, |(x, y)| self.hits.iter().any(|h| matches!(h.action, Action::Track) && x >= h.x && x < h.x + h.w && y >= h.y && y < h.y + h.h));
        on_chart(p) || on_chart(before)
    }

    /// A click at window coords: the action under it (the range buttons).
    pub fn click(&self, x: i32, y: i32) -> Option<Action> {
        let (x, y) = (x - self.pad, y - self.pad);
        self.hits
            .iter()
            .find(|h| x >= h.x && x < h.x + h.w && y >= h.y && y < h.y + h.h)
            .map(|h| h.action.clone())
            .filter(|a| !matches!(a, Action::Track))
    }
}

/// The window's title in the current language (for a language switch while it is open).
pub fn title() -> String {
    format!("WinStats \u{00B7} {}", t("power.history"))
}
