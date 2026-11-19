//! Click popup: a top-level layered window anchored to the clicked widget
//! showing details for that module. Content is drawn with the same Canvas as the strip.

use crate::canvas::{Canvas, Color, FW_NORMAL, FW_SEMIBOLD, FontSpec, GREEN, ORANGE, RED, YELLOW};
use crate::metrics::MetricsData;
use std::collections::VecDeque;
use crate::util::{format_bytes, format_bytes_rate, format_net_rate, pcwstr, unit, wide};
use crate::i18n::{t, tf};
use crate::widgets::{Kind, Theme};
use std::time::Instant;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};
use windows::Win32::Foundation::{HINSTANCE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, GetDC, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect, PAINTSTRUCT, ReleaseDC};
use windows::Win32::UI::WindowsAndMessaging::*;

#[path = "popup_battery.rs"]
mod battery_view;
#[path = "popup_cards.rs"]
mod cards;
#[path = "popup_chart.rs"]
pub mod chart_window;


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
/// timer id of the power-flow animation frames (battery popup)
pub const ANIM_TIMER: usize = 8;
/// ms between animation frames (~30 fps); each frame redraws the flow bands only
/// One frame per tick of Windows' 15.6 ms timer: about 64 frames a second (16 would round up
/// to every second tick, 32 frames). The motion is time-based, so the frames' small jitter
/// does not show.
const ANIM_FRAME_MS: u32 = 15;
/// process rows kept when a popup has to shrink to fit the screen
const MIN_ROWS: usize = 3;
/// popup width of the battery module, which carries the power-flow diagram
const BATTERY_WIDTH_DIP: f32 = 780.0;
const COPY_FLASH_SECS: f32 = 0.8;
/// frames of a held chart catching up after the mouse left it
pub const CHART_TIMER: usize = 8;
const CATCH_UP_SECS: f32 = 0.5;
/// timer id of the caret blink while a text field is being edited
pub const EDIT_TIMER: usize = 9;
/// caret blink half-period
const CARET_MS: u128 = 530;

/// The history charts as they were when the mouse came onto one: they stand still under the
/// mouse (the rows keep updating), and when it leaves they scroll the skipped samples in.
struct ChartHold {
    /// `MetricsData::samples` when the hold began
    at: u64,
    cpu: VecDeque<f32>,
    mem: VecDeque<f32>,
    gpu: VecDeque<f32>,
    vram: VecDeque<f32>,
    net: VecDeque<(f32, f32)>,
    disk: VecDeque<(f32, f32)>,
    /// the mouse left: when, and how many samples were skipped by then
    release: Option<(Instant, usize)>,
}

impl ChartHold {
    fn new(m: &MetricsData) -> Self {
        ChartHold { at: m.samples, cpu: m.cpu_history.clone(), mem: m.mem_history.clone(), gpu: m.gpu_history.clone(), vram: m.vram_history.clone(), net: m.net_history.clone(), disk: m.disk_history.clone(), release: None }
    }

    /// `m` with its histories `behind` samples short of the live ones, seen through the window
    /// the hold began with: `behind` = all skipped samples is the held picture, 0 the live one.
    fn view(&self, m: &MetricsData, behind: usize) -> MetricsData {
        let skipped = m.samples.saturating_sub(self.at) as usize;
        let mut v = m.clone();
        v.cpu_history = held(&self.cpu, &m.cpu_history, skipped, behind);
        v.mem_history = held(&self.mem, &m.mem_history, skipped, behind);
        v.gpu_history = held(&self.gpu, &m.gpu_history, skipped, behind);
        v.vram_history = held(&self.vram, &m.vram_history, skipped, behind);
        v.net_history = held(&self.net, &m.net_history, skipped, behind);
        v.disk_history = held(&self.disk, &m.disk_history, skipped, behind);
        v.charts_behind = behind.min(skipped);
        v
    }
}

/// The held history followed by the `skipped` samples that came since, seen through a window of
/// the held length that ends `behind` samples before the newest one.
fn held<T: Copy>(snap: &VecDeque<T>, live: &VecDeque<T>, skipped: usize, behind: usize) -> VecDeque<T> {
    if snap.is_empty() {
        return live.clone();
    }
    let skipped = skipped.min(live.len());
    let all: Vec<T> = snap.iter().copied().chain(live.iter().skip(live.len() - skipped).copied()).collect();
    let start = skipped - behind.min(skipped);
    all[start..start + snap.len()].iter().copied().collect()
}

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
    /// a video adapter row: count it in the GPU load or not (by adapter name)
    SelectGpu(String),
    /// pin a process row (its icon becomes a kill button); the index is the row in the list
    LockProc(usize, crate::metrics::processes::ProcInfo),
    /// terminate the process
    KillProc(u32),
    /// show / hide the download (true) or upload series in the network chart
    ToggleNetSeries(bool),
    /// put the text on the clipboard (IP, MAC)
    Copy(String),
    /// open / close an expandable block (CPU instruction list, disk details) by key
    Expand(String),
    /// show a row of a "More" fold with the fold closed too, or only with it open, by key
    ToggleRow(String),
    /// a row was dragged: the new order of the rows (all popups, see `merge_order`)
    MoveRows(Vec<String>),
    /// SYS (true) / USER (false) filter button of the process lists
    ProcFilter(bool),
    /// "Where the memory goes" as a pie (false) or one bar (true)
    MemBar(bool),
    /// the GPU process list by load (false) or by video memory (true)
    GpuSort(bool),
    /// open a URI (Windows settings pages)
    OpenUri(String),
    /// hours shown by the battery history charts
    BatteryRange(u32),
    /// keep the PC and screen awake
    KeepAwake(bool),
    /// screen-off timeout: on AC (true) or battery, seconds (0 = never)
    ScreenOff(bool, u32),
    /// a chart that shows a cursor at the mouse: redrawn on every mouse move over it
    Track,
    /// open a text field in the network popup: 0 / 1 = ping targets, 2 = custom DNS servers
    EditPingHost(u8),
    /// focus field n of the open edit (two-field DNS editor)
    EditFocus(u8),
    /// switch DNS: -1 = automatic, 0.. = metrics::dns::PRESETS
    Dns(i8),
    /// apply the saved custom DNS servers (opens the editor when there are none)
    DnsCustom,
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
    /// where the box goes horizontally (centred on it) instead of the left edge of the area:
    /// the chart value under the mouse follows the mouse
    ax: Option<i32>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: FontSpec,
    text: String,
}

/// A row of a "More" fold, collected first and drawn in the user's order (`Ui::flush_fold`).
struct FRow {
    key: &'static str,
    /// shown with the fold closed
    on: bool,
    label: String,
    value: String,
    color: Color,
    extra: Extra,
}

/// What goes with a row of a "More" fold.
enum Extra {
    None,
    /// a click on the row opens this text under it (the expand key, the text)
    Expand(&'static str, String),
    /// hovering the row shows this
    Tip(String),
}

/// The mouse button went down on a row of a "More" fold: a click when it comes up in place,
/// a drag once the mouse moves.
struct Press {
    /// window coords of the press
    x: i32,
    y: i32,
    key: String,
    dragging: bool,
    /// mouse y in content coords
    cy: i32,
}

/// "01:22", "1:05:09": a time span in minutes and seconds, with hours when it has any.
fn ago(secs: f32) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// The sample of an `n`-long history drawn `dx` px into a `w` px wide chart, as `line_chart`
/// lays it out: a short history stretched across the width, a long one with its newest `w`
/// samples, one per column.
fn series_index(n: usize, w: i32, dx: i32) -> Option<usize> {
    if n == 0 || w <= 0 {
        return None;
    }
    let dx = dx.clamp(0, w - 1) as usize;
    let w = w as usize;
    if n == 1 {
        Some(0)
    } else if n < w {
        Some(((dx * (n - 1)) as f32 / (w - 1).max(1) as f32).round() as usize)
    } else {
        Some(n - w + dx)
    }
}

/// Where rows with these keys (in their default order) go given the user's `order`: listed keys
/// by their place in it, an unlisted one right after the listed row it follows by default.
/// Rows sharing a key (one per fan) stay together. Returns indices into `keys`.
fn arrange(keys: &[&str], order: &[String]) -> Vec<usize> {
    let pos = |k: &str| order.iter().position(|o| o == k).map(|p| p as isize);
    let mut v: Vec<(isize, u8, usize)> = Vec::with_capacity(keys.len());
    for (i, k) in keys.iter().enumerate() {
        v.push(match pos(k) {
            Some(p) => (p, 0, i),
            None => ((0..i).rev().find_map(|j| pos(keys[j])).unwrap_or(-1), 1, i),
        });
    }
    v.sort();
    v.into_iter().map(|(_, _, i)| i).collect()
}

/// The stored order after a drag: the shown rows in their new order, and every other key of the
/// old order (rows not shown now, the other popups) right after the key it followed before.
fn merge_order(old: &[String], shown: &[String]) -> Vec<String> {
    let mut out = shown.to_vec();
    for (i, k) in old.iter().enumerate() {
        if out.contains(k) {
            continue;
        }
        let at = old[..i].iter().rev().find_map(|p| out.iter().position(|o| o == p)).map_or(0, |p| p + 1);
        out.insert(at, k.clone());
    }
    out
}

/// Where a dragged row would land: the slot among the other rows (key, y, height) and the y
/// of the line that shows it.
fn drop_slot(rows: &[(String, i32, i32)], key: &str, y: i32) -> (usize, i32) {
    let others: Vec<&(String, i32, i32)> = rows.iter().filter(|r| r.0 != key).collect();
    let n = others.iter().take_while(|r| r.1 + r.2 / 2 < y).count();
    let line = match others.get(n) {
        Some(r) => r.1,
        None => others.last().map_or(y, |r| r.1 + r.2),
    };
    (n, line)
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
    /// expandable blocks the user opened ("cpu_features", "disk:<model>")
    expanded: std::collections::HashSet<String>,
    /// SYS / USER filter of the process lists, as configured (drawn on the section header)
    pub proc_filter: (bool, bool),
    /// the memory breakdown as one bar instead of the pie, as configured
    pub mem_bar: bool,
    /// the GPU process list by video memory, as configured
    pub gpu_by_vram: bool,
    /// rows of the "More" folds moved from their default place, as configured for the machine
    /// shown (set before each render)
    pub rows_flipped: std::collections::HashSet<String>,
    /// the order of the fold rows the user dragged, as configured for the machine shown
    pub row_order: Vec<String>,
    /// fold rows of the last render: key, y and height in content coords
    frows: Vec<(String, i32, i32)>,
    /// the mouse button is down on a fold row
    press: Option<Press>,
    /// history charts of the last render (content coords): the mouse over one shows its values
    charts: Vec<(i32, i32, i32, i32)>,
    /// the mouse was over a chart at the last move
    in_chart: bool,
    /// the charts stand still while the mouse is over one, then catch up
    hold: Option<ChartHold>,
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
    /// power-flow bands of the last render, redrawn alone by the animation frames
    anim: Option<battery_view::FlowAnim>,
    /// content and window pixels under the bands, without them
    anim_bg_content: Vec<u32>,
    anim_bg_canvas: Vec<u32>,
    /// where the band rectangle lands in the window canvas
    anim_at: (i32, i32),
    anim_running: bool,
    anim_epoch: Instant,
    /// the flow bands' continuous motion (battery popup)
    motion: battery_view::FlowMotion,
    /// content scrolled out at the top (px), the most it can be, and the visible content height
    scroll: i32,
    max_scroll: i32,
    view_h: i32,
    /// frames, paint seconds, present seconds, since
    anim_stats: (u32, f64, f64, Instant),
    /// full redraws and their seconds since the last stats line
    render_stats: (u32, f64),
    /// window position of the last render (layered windows are presented there)
    win_pos: (i32, i32),
    /// the ping target or DNS servers being edited in place (network popup)
    pub edit: Option<PingEdit>,
}

/// A text edit open in the network popup: one field for a ping target, two (preferred and
/// alternate server) for custom DNS.
pub struct PingEdit {
    /// what is edited: 0 / 1 = ping targets, 2 = custom DNS servers
    pub slot: u8,
    pub fields: Vec<crate::textfield::TextField>,
    /// the field with the keyboard
    pub focus: usize,
    /// the last Enter was refused: shown in red until the next key
    pub error: bool,
    /// last key: the caret shows steadily for a moment after typing
    pub since: Instant,
}

impl PingEdit {
    /// the field with the keyboard
    pub fn field(&mut self) -> &mut crate::textfield::TextField {
        &mut self.fields[self.focus]
    }

    /// the fields' texts, trimmed, empty ones left out
    pub fn texts(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.text.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }

    /// Move the keyboard to field `i`.
    pub fn focus_on(&mut self, i: usize) {
        if i < self.fields.len() && i != self.focus {
            self.fields[self.focus].all = false;
            self.focus = i;
            let f = &mut self.fields[i];
            f.caret = f.text.chars().count();
        }
    }
}

/// What the drawing code needs of one field.
#[derive(Clone)]
struct FieldView {
    text: String,
    before: String,
    all: bool,
}

/// What the drawing code needs of the open edit.
#[derive(Clone)]
struct EditView {
    slot: u8,
    fields: Vec<FieldView>,
    focus: usize,
    error: bool,
    caret_on: bool,
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
            expanded: Default::default(),
            proc_filter: (true, true),
            mem_bar: false,
            gpu_by_vram: false,
            rows_flipped: Default::default(),
            row_order: Vec::new(),
            frows: Vec::new(),
            press: None,
            charts: Vec::new(),
            in_chart: false,
            hold: None,
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
            anim: None,
            anim_bg_content: Vec::new(),
            anim_bg_canvas: Vec::new(),
            anim_at: (0, 0),
            anim_running: false,
            anim_epoch: Instant::now(),
            motion: Default::default(),
            scroll: 0,
            max_scroll: 0,
            view_h: 0,
            anim_stats: (0, 0.0, 0.0, Instant::now()),
            render_stats: (0, 0.0),
            win_pos: (0, 0),
            edit: None,
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
        self.scroll = 0;
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
        self.press = None;
        self.end_hold();
        true
    }

    /// Drop the held charts at once (another popup, the window hidden).
    fn end_hold(&mut self) {
        if self.hold.take().is_some() {
            unsafe {
                let _ = KillTimer(self.hwnd, CHART_TIMER);
            }
        }
    }

    /// The mouse left the charts: let the held ones scroll the skipped samples in.
    fn release_hold(&mut self) {
        if let Some(h) = &mut self.hold {
            if h.release.is_none() {
                h.release = Some((Instant::now(), 0));
                unsafe {
                    SetTimer(self.hwnd, CHART_TIMER, 15, None);
                }
            }
        }
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

    /// Open or close an expandable block.
    /// An expandable block is open ("dns": the DNS list of the network popup).
    pub fn is_expanded(&self, key: &str) -> bool {
        self.expanded.contains(key)
    }

    pub fn toggle_expand(&mut self, key: String) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
    }

    pub fn unlock(&mut self, pid: u32) {
        self.locks.retain(|l| l.snap.pid != pid);
    }

    /// The mouse left the window: drop the hover state; true when a redraw is needed.
    pub fn mouse_leave(&mut self) -> bool {
        if self.in_chart {
            self.in_chart = false;
            self.release_hold();
        }
        let changed = self.mouse.is_some() || self.tip.is_some() || self.hover.is_some();
        self.mouse = None;
        self.tip = None;
        self.hover = None;
        self.hit_hover = None;
        self.frozen = None;
        changed
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<&Hit> {
        let (cx, cy) = (x - self.pad, y - self.pad + self.scroll);
        if !self.in_view(y) {
            return None;
        }
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

    /// The button went down at (x, y), window coords: true when it is on a fold row, which
    /// then waits for `release` (a click) or a move (a drag).
    pub fn press(&mut self, x: i32, y: i32) -> bool {
        let cy = y - self.pad;
        let Some(row) = self.frows.iter().find(|r| cy >= r.1 && cy < r.1 + r.2) else { return false };
        self.press = Some(Press { x, y, key: row.0.clone(), dragging: false, cy });
        true
    }

    /// A row is being dragged.
    pub fn dragging(&self) -> bool {
        self.press.as_ref().map_or(false, |p| p.dragging)
    }

    /// The button came up: None when nothing was pressed; Some(action) after a press, the click
    /// of the pressed place or the new order after a drag (None inside when nothing changed).
    pub fn release(&mut self, x: i32, y: i32) -> Option<Option<Action>> {
        let p = self.press.take()?;
        if !p.dragging {
            return Some(self.click(p.x, p.y));
        }
        let _ = x;
        let (slot, _) = drop_slot(&self.frows, &p.key, y - self.pad);
        let mut shown: Vec<String> = Vec::new();
        for r in &self.frows {
            if r.0 != p.key && !shown.contains(&r.0) {
                shown.push(r.0.clone());
            }
        }
        let before: Vec<String> = self.frows.iter().map(|r| r.0.clone()).collect();
        shown.insert(slot.min(shown.len()), p.key.clone());
        if shown == before {
            return Some(None);
        }
        Some(Some(Action::MoveRows(merge_order(&self.row_order, &shown))))
    }

    /// The mouse capture went away mid-press: true when a drag was showing (redraw).
    pub fn cancel_press(&mut self) -> bool {
        self.press.take().map_or(false, |p| p.dragging)
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
            self.stop_anim();
            self.end_edit();
            self.press = None;
            self.end_hold();
        }
    }

    /// Mouse moved over the popup (window coords): returns true when the hovered slice or
    /// truncated label changed.
    /// Window y inside the visible content (not the padding a scrolled list runs under).
    fn in_view(&self, y: i32) -> bool {
        self.max_scroll == 0 || (y >= self.pad && y < self.pad + self.view_h)
    }

    /// Scroll by `px` (mouse wheel); true when the view moved.
    pub fn scroll_by(&mut self, px: i32) -> bool {
        let to = (self.scroll + px).clamp(0, self.max_scroll);
        let moved = to != self.scroll;
        self.scroll = to;
        if moved {
            self.tip = None;
        }
        moved
    }

    pub fn mouse_move(&mut self, x: i32, y: i32) -> bool {
        let (cx, cy) = (x - self.pad, y - self.pad + self.scroll);
        let tip = self.tips.iter().position(|t| cx >= t.x && cx < t.x + t.w && cy >= t.y && cy < t.y + t.h);
        let mut changed = false;
        // over a history chart every move shows another moment: redraw, and once more on leaving
        let in_chart = self.charts.iter().any(|&(x, y, w, h)| cx >= x && cx < x + w && cy >= y && cy < y + h);
        if in_chart || self.in_chart {
            changed = true;
        }
        if in_chart {
            // back onto a chart while it was catching up: the next render holds it afresh
            if self.hold.as_ref().map_or(false, |h| h.release.is_some()) {
                self.end_hold();
            }
        } else if self.in_chart {
            self.release_hold();
        }
        self.in_chart = in_chart;
        if let Some(p) = &mut self.press {
            // a few pixels of travel turn the press into a drag
            if !p.dragging && (y - p.y).abs() > 4 {
                p.dragging = true;
            }
            if p.dragging && p.cy != cy {
                p.cy = cy;
                changed = true;
            }
        }
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
        // a chart with a cursor follows the mouse
        if over.map_or(false, |i| matches!(self.hits[i].action, Action::Track)) {
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
        let x = tip.ax.map_or(tip.x, |a| a - w / 2).min(content_w - w).max(0);
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
        let t_render = Instant::now();
        self.render_inner(t, light, m, first);
        self.render_stats.0 += 1;
        self.render_stats.1 += t_render.elapsed().as_secs_f64();
    }

    fn render_inner(&mut self, t: &Theme, light: bool, m: &MetricsData, first: bool) {
        let Some(kind) = self.kind else { return };
        if !self.visible {
            return;
        }
        // the mouse over a chart holds the charts; after it left they catch up, eased out
        if self.in_chart && self.hold.is_none() {
            self.hold = Some(ChartHold::new(m));
        }
        let mut done = false;
        let behind = self.hold.as_mut().map(|h| {
            let skipped = m.samples.saturating_sub(h.at) as usize;
            match &mut h.release {
                None => skipped,
                Some((at, from)) => {
                    if *from == 0 {
                        *from = skipped.max(1);
                    }
                    let p = (at.elapsed().as_secs_f32() / CATCH_UP_SECS).min(1.0);
                    let eased = 1.0 - (1.0 - p).powi(3);
                    done = p >= 1.0;
                    ((*from as f32) * (1.0 - eased)).round() as usize
                }
            }
        });
        if done {
            self.end_hold();
        }
        let view;
        let m = match (&self.hold, behind) {
            (Some(h), Some(b)) => {
                view = h.view(m, b);
                &view
            }
            _ => m,
        };
        self.light = light;
        let pad = t.s(PAD_DIP);
        let work_w = {
            let pt = windows::Win32::Foundation::POINT { x: (self.anchor.left + self.anchor.right) / 2, y: (self.anchor.top + self.anchor.bottom) / 2 };
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            unsafe {
                let _ = GetMonitorInfoW(windows::Win32::Graphics::Gdi::MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut mi);
            }
            mi.rcWork.right - mi.rcWork.left
        };
        let w = match kind {
            Kind::Battery => t.s(BATTERY_WIDTH_DIP),
            _ => t.s(WIDTH_DIP),
        };
        // never wider than the screen
        let w = if work_w > 0 { w.min(work_w - t.s(16.0)) } else { w };
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
                    // the timer died with the old window
                    self.anim_running = false;
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
        // pass 1: content on a transparent canvas. The canvas grows to the layout (a fixed
        // height cut long popups off and left their reserved rows empty), and a popup that would
        // not fit on the screen shows fewer process rows instead of running off the bottom.
        let copied = self.copy_progress();
        let phase = self.anim_epoch.elapsed().as_secs_f32();
        let avail = {
            let pt = windows::Win32::Foundation::POINT { x: (self.anchor.left + self.anchor.right) / 2, y: (self.anchor.top + self.anchor.bottom) / 2 };
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            unsafe {
                let _ = GetMonitorInfoW(windows::Win32::Graphics::Gdi::MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut mi);
            }
            let work = mi.rcWork.bottom - mi.rcWork.top;
            if work > 0 { work - t.s(8.0) * 2 - pad * 2 } else { i32::MAX }
        };
        let ping_edit = self.edit.as_ref().map(|e| EditView {
            slot: e.slot,
            fields: e.fields.iter().map(|f| FieldView { text: f.text.clone(), before: f.before_caret().to_string(), all: f.all }).collect(),
            focus: e.focus,
            error: e.error,
            // steady right after a key, then blinking
            caret_on: (e.since.elapsed().as_millis() / CARET_MS) % 2 == 0,
        });
        let mut canvas_h = t.s(900.0);
        let mut max_rows: Option<usize> = None;
        let mut attempt = 0;
        let end_y = loop {
            attempt += 1;
            self.content.resize(w - pad * 2, canvas_h);
            self.content.clear();
            let mut ui = Ui { c: &mut self.content, t, w: w - pad * 2, y: 0, hover: self.hover, pie: None, tips: Vec::new(), hits: Vec::new(), icons: &mut self.icons, indent: 0, locks: &mut self.locks, frozen: &mut self.frozen, expanded: &self.expanded, proc_filter: self.proc_filter, mem_bar: self.mem_bar, gpu_by_vram: self.gpu_by_vram, mouse: self.mouse, copied: copied.clone(), anim: None, anim_bg: Vec::new(), phase, max_rows, ping_edit: ping_edit.clone(), flipped: &self.rows_flipped, fold_open: false, charts: Vec::new(), last_chart: (0, 0, 0, 0), ages: (m.sample_age(1, 0), if m.sample_ms > 0.0 { m.sample_ms / 1000.0 } else { 1.0 }), fold: Vec::new(), order: &self.row_order, drag: self.press.as_ref().filter(|p| p.dragging).map(|p| (p.key.clone(), p.cy)), frows: Vec::new() };
            match kind {
                Kind::Cpu => draw_cpu(&mut ui, m),
                Kind::Mem => draw_mem(&mut ui, m),
                Kind::Gpu => draw_gpu(&mut ui, m),
                Kind::Net => draw_net(&mut ui, m),
                Kind::Disk => draw_disk(&mut ui, m),
                Kind::Battery => battery_view::draw(&mut ui, m),
            }
            // layout cursor, not painted pixels: reserved (empty) rows count too
            let y = ui.y;
            if attempt < 10 {
                // the canvas first: a popup without process rows (battery) only scrolls, and
                // spending the attempts on rows that change nothing cut its content off
                if y > canvas_h {
                    canvas_h = y + t.s(16.0);
                    continue;
                }
                if y > avail {
                    let cur = max_rows.unwrap_or(m.top_n);
                    let row = t.s(22.0);
                    let next = cur.saturating_sub(((y - avail + row - 1) / row) as usize).max(MIN_ROWS);
                    if next < cur {
                        max_rows = Some(next);
                        continue;
                    }
                }
            }
            self.anim = ui.anim.take();
            self.anim_bg_content = std::mem::take(&mut ui.anim_bg);
            // pie geometry from content coords to window coords
            self.pie = ui.pie.take().map(|mut p| {
                p.cx += (sh + pad) as f32;
                p.cy += (sh + pad) as f32;
                p
            });
            self.tips = std::mem::take(&mut ui.tips);
            self.hits = std::mem::take(&mut ui.hits);
            // the fold rows drawn, for dragging them
            self.frows = std::mem::take(&mut ui.frows);
            self.charts = std::mem::take(&mut ui.charts);
            break y;
        };
        self.pad = sh + pad;
        // the bands come off the content: they are drawn straight into the window below, at the
        // running motion's state (a render does not restart them). Under a tooltip they stay in
        // the content, so the tooltip covers them.
        if let Some(a) = self.anim.clone() {
            self.motion.retarget(&a);
            self.content.put_region(a.x, a.y, a.w, a.h, &self.anim_bg_content);
            if !self.anim_in_view(&a) {
                battery_view::paint_smooth(&mut self.content, &a, &mut self.motion, (0.0, 0.0));
            }
        }
        if self.tip.map_or(false, |i| i >= self.tips.len()) {
            self.tip = None;
        }
        self.draw_tooltip(t, light, w - pad * 2);
        let content_h = end_y.max(self.content.used_height()).max(t.s(40.0));
        // never shrink while open; grow only if content really needs more room (and never past
        // the screen: a height locked before a shorter layout must not leave a gap)
        let h = (content_h + pad * 2).max(self.locked_h).min((content_h + pad * 2).max(avail + pad * 2));
        // taller than the screen: the window stops at the screen, the content scrolls
        let h = h.min(avail + pad * 2);
        self.locked_h = h;
        self.view_h = h - pad * 2;
        self.max_scroll = (content_h - self.view_h).max(0);
        self.scroll = self.scroll.clamp(0, self.max_scroll);

        // pass 2: background + content
        self.canvas.resize(w + sh * 2, h + sh * 2);
        self.canvas.clear();
        // the style's own tint, else the opacity setting's theme fill
        let (tint, overlay) = match t.style.popup_fill(light) {
            Some((c, a)) => (c, a),
            None => (if light { Color::rgb(0xF9, 0xF9, 0xF9) } else { Color::rgb(0x20, 0x20, 0x20) }, self.opacity.overlay()),
        };
        let card = |a: u8| Color { a, ..tint };
        let bg = card(self.opacity.card_alpha().max((overlay * 255.0).round() as u8));
        let r = t.s(RADIUS_DIP) as f32;
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
        // the visible part of the content (all of it unless it scrolls)
        self.canvas.blit_rect(&self.content, 0, self.scroll, w - pad * 2, self.view_h, sh + pad, sh + pad);
        if self.max_scroll > 0 {
            // a thin scroll bar in the right padding
            let track = self.view_h;
            let thumb = (track as i64 * track as i64 / (content_h as i64).max(1)) as i32;
            let ty = sh + pad + ((track - thumb) as i64 * self.scroll as i64 / self.max_scroll.max(1) as i64) as i32;
            let bw = t.s(3.0);
            self.canvas.fill_round_rect(sh + w - pad / 2 - bw / 2, ty, bw, thumb.max(t.s(24.0)), bw as f32 / 2.0, t.dim.alpha(0.5));
        }
        if let Some(a) = self.anim.clone() {
            // the finished window under the bands (without them): each frame copies it back and
            // draws the bands on it, no blending of the whole area again
            self.anim_at = (sh + pad + a.x, sh + pad + a.y - self.scroll);
            self.anim_bg_canvas = self.canvas.copy_region(self.anim_at.0, self.anim_at.1, a.w, a.h);
            if self.anim_in_view(&a) {
                let off = ((self.anim_at.0 - a.x) as f32, (self.anim_at.1 - a.y) as f32);
                battery_view::paint_smooth(&mut self.canvas, &a, &mut self.motion, off);
            }
        }

        // position: above (or below) the taskbar, centred on the clicked widget
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        // The popup HWND can still be on the primary display or on the
        // display used by the previous popup. Use the clicked widget instead.
        let hm = unsafe { MonitorFromRect(&self.anchor, MONITOR_DEFAULTTONEAREST) };
        if !unsafe { GetMonitorInfoW(hm, &mut mi) }.as_bool() {
            self.hide("popup monitor unavailable");
            return;
        }
        let mon = mi.rcMonitor;
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
            self.win_pos = (x, y);
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
        // frames only while something flows and motion is welcome
        let want = self.anim.as_ref().map_or(false, |a| a.moving() || self.motion.gliding()) && t.animate;
        if want && !self.anim_running {
            self.anim_running = true;
            unsafe {
                SetTimer(self.hwnd, ANIM_TIMER, ANIM_FRAME_MS, None);
            }
        } else if !want {
            self.stop_anim();
        }
    }

    /// Turn a block into text fields holding `texts` (the first one focused, all selected).
    pub fn start_edit(&mut self, slot: u8, texts: &[String], max_len: usize) {
        let fields = texts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut f = crate::textfield::TextField::new(t, max_len);
                f.all = i == 0 && f.all;
                f
            })
            .collect();
        self.edit = Some(PingEdit { slot, fields, focus: 0, error: false, since: Instant::now() });
        self.relayout();
        unsafe {
            SetTimer(self.hwnd, EDIT_TIMER, CARET_MS as u32, None);
        }
    }

    pub fn end_edit(&mut self) {
        if self.edit.take().is_some() {
            self.relayout();
            unsafe {
                let _ = KillTimer(self.hwnd, EDIT_TIMER);
            }
        }
    }

    /// Note a key on the field: the caret shows, a refusal is forgotten.
    pub fn edit_touched(&mut self) {
        if let Some(e) = self.edit.as_mut() {
            e.since = Instant::now();
            e.error = false;
        }
    }

    /// The bands can be drawn straight into the window: no tooltip over them, fully in view.
    fn anim_in_view(&self, a: &battery_view::FlowAnim) -> bool {
        self.tip.is_none() && (self.max_scroll == 0 || (a.y >= self.scroll && a.y + a.h <= self.scroll + self.view_h))
    }

    fn stop_anim(&mut self) {
        if self.anim_running {
            self.anim_running = false;
            unsafe {
                let _ = KillTimer(self.hwnd, ANIM_TIMER);
            }
        }
    }

    /// One animation frame: put back the pixels under the flow bands, draw the bands at the new
    /// phase and present. The rest of the popup is not touched.
    pub fn animate(&mut self) {
        let Some(a) = self.anim.as_ref() else {
            self.stop_anim();
            return;
        };
        if !self.visible {
            self.stop_anim();
            return;
        }
        // a tooltip may cover the bands, or they are scrolled partly out: keep the frame still
        if !self.anim_in_view(&a.clone()) {
            return;
        }
        let t0 = Instant::now();
        let a = a.clone();
        let (cx, cy) = self.anim_at;
        self.canvas.put_region(cx, cy, a.w, a.h, &self.anim_bg_canvas);
        battery_view::paint_smooth(&mut self.canvas, &a, &mut self.motion, ((cx - a.x) as f32, (cy - a.y) as f32));
        let t1 = Instant::now();
        // only the bands' rect goes to the screen, not the whole popup
        unsafe {
            if self.native == crate::backdrop::Mode::Layered {
                let _ = self.canvas.present_region(self.hwnd, self.win_pos.0, self.win_pos.1, cx, cy, a.w, a.h);
            } else {
                let hdc = GetDC(self.hwnd);
                self.canvas.paint_dc_region(hdc, cx, cy, a.w, a.h);
                ReleaseDC(self.hwnd, hdc);
            }
        }
        // frame cost, logged every 5 s while animating
        let st = &mut self.anim_stats;
        st.0 += 1;
        st.1 += (t1 - t0).as_secs_f64();
        st.2 += t1.elapsed().as_secs_f64();
        if st.3.elapsed().as_secs() >= 5 {
            let secs = st.3.elapsed().as_secs_f64();
            let rs = self.render_stats;
            crate::log!("anim: {:.0} fps, paint {:.2} ms, present {:.2} ms per frame, area {}x{}; {} redraws of {:.1} ms", st.0 as f64 / secs, st.1 * 1000.0 / st.0 as f64, st.2 * 1000.0 / st.0 as f64, a.w, a.h, rs.0, rs.1 * 1000.0 / rs.0.max(1) as f64);
            self.render_stats = (0, 0.0);
            *st = (0, 0.0, 0.0, Instant::now());
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
    expanded: &'a std::collections::HashSet<String>,
    proc_filter: (bool, bool),
    /// the memory breakdown as one bar
    mem_bar: bool,
    /// the GPU process list by video memory
    gpu_by_vram: bool,
    /// mouse position in content coords, when over the popup
    mouse: Option<(i32, i32)>,
    /// text just copied and the flash progress 0..1
    copied: Option<(String, f32)>,
    /// animated flow bands drawn by this render (battery popup)
    anim: Option<battery_view::FlowAnim>,
    /// content pixels under the bands before they were drawn
    anim_bg: Vec<u32>,
    /// animation clock, seconds
    phase: f32,
    /// process rows that fit on the screen; None = as configured
    max_rows: Option<usize>,
    /// the ping target or DNS field, while it is being edited
    ping_edit: Option<EditView>,
    /// rows of the "More" folds moved from their default place
    flipped: &'a std::collections::HashSet<String>,
    /// the "More" fold of the popup being drawn is open: every `frow` shows, a click on a label
    /// moves the row between the folded and the open view
    fold_open: bool,
    /// fold rows collected for `flush_fold`
    fold: Vec<FRow>,
    /// the user's order of the fold rows
    order: &'a [String],
    /// the fold row being dragged and the mouse y (content coords)
    drag: Option<(String, i32)>,
    /// fold rows drawn: key, y, height (rows sharing a key as one)
    frows: Vec<(String, i32, i32)>,
    /// history charts drawn, for the values under the mouse
    charts: Vec<(i32, i32, i32, i32)>,
    /// the area of the chart drawn last (`probe` adds its values under the mouse)
    last_chart: (i32, i32, i32, i32),
    /// how long ago a chart's sample was: (seconds since the newest, seconds per sample)
    ages: (f32, f32),
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
            self.tips.push(Tip { ax: None, x, y, w: max_w.max(1), h: th, font, text: text.to_string() });
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

    /// Title with a list of small lines under it instead of one subtitle: (left, right, left
    /// colour, click action). `right` goes on the title line.
    fn title_list(&mut self, title: &str, right: &str, lines: &[(String, String, Color, Option<Action>)]) {
        let f = self.font(15.0, FW_SEMIBOLD);
        let (_, h) = self.c.draw_text(title, 0, self.y, f, self.t.text);
        if !right.is_empty() {
            // bottom-aligned with the title, in the subtitle font
            let fs = self.font(9.5, FW_NORMAL);
            let (rw, rh) = self.c.measure_text(right, fs);
            self.c.draw_text(right, self.w - rw, self.y + h - rh - self.t.s(2.0), fs, self.t.dim);
        }
        self.y += h;
        let f = self.font(9.5, FW_NORMAL);
        for (left, value, color, action) in lines {
            let (vw, lh) = self.c.measure_text(value, f);
            let lh = if value.is_empty() { self.c.measure_text(left, f).1 } else { lh };
            let start = self.y;
            self.clipped(left, 0, self.y, self.w - vw - self.t.s(8.0), f, *color);
            self.c.draw_text(value, self.w - vw, self.y, f, *color);
            self.y += lh + self.t.s(1.0);
            if let Some(a) = action {
                self.clickable(start, a.clone());
            }
        }
        self.space(9.0);
    }

    /// Process section header: the title, and for the local machine the SYS / USER filter
    /// buttons on the right (filled when on).
    fn proc_section(&mut self, name: &str, local: bool) {
        self.proc_section_sorted(name, local, None);
    }

    /// Process section header: the title; with `by_vram` the GPU / VRAM order buttons after it
    /// (the chosen one filled); for the local machine the SYS / USER filter buttons on the right.
    fn proc_section_sorted(&mut self, name: &str, local: bool, by_vram: Option<bool>) {
        if !local && by_vram.is_none() {
            self.section(name);
            self.space(4.0);
            return;
        }
        self.space(8.0);
        let f = self.font(9.5, FW_SEMIBOLD);
        let (title_w, h) = self.c.draw_text(&name.to_uppercase(), 0, self.y, f, self.t.dim);
        let fb = self.font(8.5, FW_SEMIBOLD);
        let (pad_x, gap) = (self.t.s(6.0), self.t.s(4.0));
        let bh = h + self.t.s(2.0);
        let by = self.y - self.t.s(1.0);
        let r = self.t.s(2.5) as f32;
        let button = |ui: &mut Self, x: i32, label: &str, on: bool, action: Action| -> i32 {
            let (tw, th) = ui.c.measure_text(label, fb);
            let bw = tw + pad_x * 2;
            if on {
                ui.c.fill_round_rect(x, by, bw, bh, r, ui.t.text.alpha(0.16));
            } else {
                ui.c.stroke_round_rect_r(x, by, bw, bh, r, ui.t.dim.alpha(0.5));
            }
            let col = if on { ui.t.text } else { ui.t.dim.alpha(0.7) };
            ui.c.draw_text(label, x + pad_x, by + (bh - th) / 2, fb, col);
            ui.hits.push(Hit { x, y: by, w: bw, h: bh, action });
            bw
        };
        if let Some(vram) = by_vram {
            let mut x = title_w + self.t.s(10.0);
            x += button(self, x, "GPU", !vram, Action::GpuSort(false)) + gap;
            button(self, x, "VRAM", vram, Action::GpuSort(true));
        }
        if local {
            let mut x = self.w;
            for (label, on, system) in [("USER", self.proc_filter.1, false), ("SYS", self.proc_filter.0, true)] {
                let bw = self.c.measure_text(label, fb).0 + pad_x * 2;
                x -= bw;
                button(self, x, label, on, Action::ProcFilter(system));
                x -= gap;
            }
        }
        self.y += h + self.t.s(8.0);
    }

    /// Section header with two small switches on the right: the breakdown as a pie (a circle
    /// icon) or as one bar (a bar icon), the chosen one filled.
    fn breakdown_section(&mut self, name: &str) {
        self.space(8.0);
        let f = self.font(9.5, FW_SEMIBOLD);
        let (_, h) = self.c.draw_text(&name.to_uppercase(), 0, self.y, f, self.t.dim);
        let bh = h + self.t.s(2.0);
        let bw = bh + self.t.s(6.0);
        let by = self.y - self.t.s(1.0);
        let r = self.t.s(2.5) as f32;
        let mut x = self.w;
        for bar in [true, false] {
            x -= bw;
            let on = self.mem_bar == bar;
            if on {
                self.c.fill_round_rect(x, by, bw, bh, r, self.t.text.alpha(0.16));
            } else {
                self.c.stroke_round_rect_r(x, by, bw, bh, r, self.t.dim.alpha(0.5));
            }
            let col = if on { self.t.text } else { self.t.dim.alpha(0.7) };
            let (cx, cy) = (x as f32 + bw as f32 / 2.0, by as f32 + bh as f32 / 2.0);
            if bar {
                let (iw, ih) = (self.t.s(11.0), self.t.s(4.0));
                self.c.fill_round_rect(cx as i32 - iw / 2, cy as i32 - ih / 2, iw, ih, ih as f32 / 2.0, col);
            } else {
                self.c.fill_circle(cx, cy, self.t.s(4.0) as f32, col);
            }
            self.hits.push(Hit { x, y: by, w: bw, h: bh, action: Action::MemBar(bar) });
            x -= self.t.s(4.0);
        }
        self.y += h + self.t.s(6.0);
    }

    /// The parts of a whole as one rounded bar across the popup; hovering a part makes it
    /// brighter and names it with its size and share.
    fn stacked_bar(&mut self, slices: &[(String, u64, Color)]) {
        let total: u64 = slices.iter().map(|s| s.1).sum();
        if total == 0 {
            return;
        }
        let h = self.t.s(12.0);
        let y = self.y + self.t.s(2.0);
        let r = h as f32 / 2.0;
        let font = self.font(9.5, FW_NORMAL);
        let mut x = 0i32;
        let mut sum = 0u64;
        let n = slices.len();
        for (i, (label, v, col)) in slices.iter().enumerate() {
            sum += *v;
            let end = if i + 1 == n { self.w } else { (self.w as f64 * sum as f64 / total as f64).round() as i32 };
            if end <= x {
                continue;
            }
            let hovered = self.mouse.map_or(false, |(mx, my)| mx >= x && mx < end && my >= y - self.t.s(3.0) && my < y + h + self.t.s(3.0));
            // brighter: lighter and, for a see-through part, more solid
            let c = if hovered { lighten(*col).alpha(((col.a as f32 / 255.0) * 1.5 + 0.2).min(1.0)) } else { *col };
            self.c.fill_round_rect_cols(0, y, self.w, h, r, x, end, c);
            let pct = (*v as f64 / total as f64 * 100.0).round() as i32;
            self.tips.push(Tip { ax: None, x, y: y - self.t.s(3.0), w: end - x, h: h + self.t.s(6.0), font, text: format!("{label}: {} ({pct}%)", format_bytes(*v)) });
            x = end;
        }
        self.y = y + h + self.t.s(8.0);
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

    /// A row of a popup with a "More" fold, drawn by `flush_fold`. Shown with the fold closed when
    /// it is on (`default_on`, flipped by a click on its label with the fold open).
    fn frow(&mut self, key: &'static str, default_on: bool, label: &str, value: &str, color: Color) {
        self.frow_x(key, default_on, label, value, color, Extra::None);
    }

    /// `frow` with something that goes with the row: a block a click opens, a tooltip.
    fn frow_x(&mut self, key: &'static str, default_on: bool, label: &str, value: &str, color: Color, extra: Extra) {
        let on = default_on != self.flipped.contains(key);
        self.fold.push(FRow { key, on, label: label.to_string(), value: value.to_string(), color, extra });
    }

    /// Draw the collected fold rows in the user's order. With the fold closed only the on rows;
    /// with it open all of them, the label of an on row in full text colour and of the others
    /// dim, a click on the label switching it. Any row can be dragged up or down: the dragged one
    /// is shaded and a line shows where it goes.
    fn flush_fold(&mut self) {
        let rows = std::mem::take(&mut self.fold);
        let keys: Vec<&str> = rows.iter().map(|r| r.key).collect();
        let place = arrange(&keys, self.order);
        let mut rows: Vec<Option<FRow>> = rows.into_iter().map(Some).collect();
        let dragged = self.drag.as_ref().map(|d| d.0.clone());
        let f = self.font(11.0, FW_NORMAL);
        let pad = self.t.s(3.0);
        for i in place {
            let Some(r) = rows[i].take() else { continue };
            if !self.fold_open && !r.on {
                continue;
            }
            let start = self.y;
            if self.fold_open {
                // the label is the switch: a faint box under the mouse
                let (lw, lh) = self.c.measure_text(&r.label, f);
                let lw = lw.min(self.w / 2) + pad;
                if dragged.is_none() && self.mouse.map_or(false, |(mx, my)| mx >= 0 && mx < lw && my >= start && my < start + lh) {
                    self.c.fill_round_rect(-pad, start, lw + pad, lh, pad as f32, self.t.text.alpha(0.06));
                }
                self.row_lv(&r.label, &r.value, if r.on { self.t.text } else { self.t.dim }, r.color);
                self.hits.push(Hit { x: 0, y: start, w: lw, h: lh, action: Action::ToggleRow(r.key.to_string()) });
            } else {
                self.row_colored(&r.label, &r.value, r.color);
            }
            match r.extra {
                Extra::None => {}
                Extra::Expand(key, text) => {
                    if self.expanded.contains(key) && !text.is_empty() {
                        self.wrapped(&text, self.t.dim);
                    }
                    self.clickable(start, Action::Expand(key.to_string()));
                }
                Extra::Tip(text) => self.hover_tip(start, text),
            }
            if dragged.as_deref() == Some(r.key) {
                self.c.fill_round_rect(-pad, start, self.w + pad * 2, self.y - start, pad as f32, self.t.accent.alpha(0.18));
            }
            // rows sharing a key (one per fan) are one block
            match self.frows.last_mut() {
                Some(last) if last.0 == r.key && last.1 + last.2 == start => last.2 = self.y - start,
                _ => self.frows.push((r.key.to_string(), start, self.y - start)),
            }
        }
        if let Some((key, my)) = self.drag.clone() {
            if self.frows.iter().any(|r| r.0 == key) {
                let (_, ly) = drop_slot(&self.frows, &key, my);
                let ly = ly - self.t.s(1.5);
                self.c.fill_round_rect(-pad, ly - self.t.s(1.0), self.w + pad * 2, self.t.s(2.0).max(2), self.t.s(1.0) as f32, self.t.accent);
            }
        }
        self.fold_open = false;
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
                self.tips.push(Tip { ax: None, x, y: self.y, w, h, font: ft, text: tip });
            }
            x += w;
            if x >= self.w {
                break;
            }
        }
        self.y += h + self.t.s(3.0);
    }

    /// Dim text word-wrapped to the popup width, indented like a sub-row.
    fn wrapped(&mut self, text: &str, color: Color) {
        let f = self.font(10.0, FW_NORMAL);
        let x0 = self.t.s(12.0);
        let max_w = self.w - x0;
        let h = self.c.measure_text("X", f).1;
        let mut line = String::new();
        for word in text.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if line.is_empty() || self.c.measure_text(&candidate, f).0 <= max_w {
                line = candidate;
            } else {
                self.c.draw_text(&line, x0, self.y, f, color);
                self.y += h + self.t.s(1.0);
                line = word.to_string();
            }
        }
        if !line.is_empty() {
            self.c.draw_text(&line, x0, self.y, f, color);
            self.y += h + self.t.s(1.0);
        }
        self.space(3.0);
    }

    /// The "More" / "Less" line (with a chevron) under a list whose rarely needed rows fold away; `key` is
    /// the expand key (kept in the config, so the choice survives).
    fn more_toggle(&mut self, key: &str, open: bool) {
        let f = self.font(9.5, FW_NORMAL);
        let text = if open { t("popup.less") } else { t("popup.more") };
        let start = self.y;
        // right-aligned: the text, then a drawn chevron (the font's triangles are too small)
        let (cw, ch) = (self.t.s(7.0) as f32, self.t.s(3.5) as f32);
        let (tw, h) = self.c.measure_text(&text, f);
        let tx = self.w - tw - self.t.s(5.0) - cw as i32;
        self.c.draw_text(&text, tx, self.y, f, self.t.dim);
        let cx = (self.w - cw as i32) as f32;
        let cy = self.y as f32 + h as f32 / 2.0 + 1.0;
        let (top, tip) = if open { (cy + ch / 2.0, cy - ch / 2.0) } else { (cy - ch / 2.0, cy + ch / 2.0) };
        let width = self.t.s(1.2) as f32;
        self.c.draw_line(cx, top, cx + cw / 2.0, tip, width, self.t.dim);
        self.c.draw_line(cx + cw / 2.0, tip, cx + cw, top, width, self.t.dim);
        self.y += h + self.t.s(3.0);
        self.clickable(start, Action::Expand(key.to_string()));
    }

    /// Hovering the rows drawn since `from_y` shows `text` as a tooltip.
    fn hover_tip(&mut self, from_y: i32, text: String) {
        let font = self.font(9.5, FW_NORMAL);
        self.tips.push(Tip { ax: None, x: 0, y: from_y, w: self.w, h: self.y - from_y, font, text });
    }

    /// Make the rows drawn since `from_y` react to a click.
    /// A tooltip for any content rect.
    fn tips_for_rect(&mut self, x: i32, y: i32, w: i32, h: i32, text: String) {
        let font = self.font(9.5, FW_NORMAL);
        self.tips.push(Tip { ax: None, x, y, w, h, font, text });
    }

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
        // the card language of the popups: soft fill, faint outline, quiet grid
        let r = self.t.s(6.0) as f32;
        let (fill, stroke) = self.t.style.card(self.t.light);
        if self.t.style == crate::style::Style::Classic {
            self.c.fill_round_rect(0, self.y, self.w, h, r, self.t.dim.alpha(0.06));
        } else {
            self.c.fill_round_rect(0, self.y, self.w, h, r, fill);
        }
        self.c.stroke_round_rect_r(0, self.y, self.w, h, r, stroke.unwrap_or(self.t.dim.alpha(0.2)));
        for q in [0.25f32, 0.5, 0.75] {
            let gy = self.y + (h as f32 * q).round() as i32;
            self.c.fill_rect(2, gy, self.w - 4, 1, self.t.dim.alpha(0.08));
        }
        let rect = (1, self.y + 1, self.w - 2, h - 2);
        self.y += h + self.t.s(6.0);
        self.last_chart = rect;
        rect
    }

    /// Single 0..1 series.
    fn chart(&mut self, values: &[f32], h_dip: f32, color: Color) {
        let (x, y, w, h) = self.chart_box(h_dip);
        let lw = self.t.s(1.5) as f32;
        self.c.area_chart(x, y, w, h, values, color, self.t.style.chart_fill(), lw);
    }

    /// The values of the chart drawn last at the mouse: a dot where each of its `lines` (0..1, as
    /// drawn, in its colour) crosses the mouse's column and a box with `text`, which gets the
    /// index of that moment in a series of a given length (None when that series has none there).
    fn probe(&mut self, lines: &[(&[f32], Color)], text: impl Fn(&dyn Fn(usize) -> Option<usize>) -> String) {
        let (x, y, w, h) = self.last_chart;
        self.charts.push((x, y, w, h));
        let Some((mx, my)) = self.mouse else { return };
        if mx < x || mx >= x + w || my < y || my >= y + h {
            return;
        }
        let dx = mx - x;
        let label = text(&|n| series_index(n, w, dx));
        if label.is_empty() {
            return;
        }
        // how long ago that moment was, from the longest of the lines: "−01:22 · Load: 34%"
        let n = lines.iter().map(|l| l.0.len()).max().unwrap_or(0);
        let label = match series_index(n, w, dx) {
            Some(i) => format!("\u{2212}{} \u{00B7} {label}", ago(self.ages.0 + (n - 1 - i) as f32 * self.ages.1)),
            None => label,
        };
        let (r, ring) = (self.t.s(1.5) as f32, self.t.s(0.6) as f32);
        for (values, color) in lines {
            let Some(i) = series_index(values.len(), w, dx) else { continue };
            let cy = (y + h) as f32 - values[i].clamp(0.0, 1.0) * h as f32;
            let cx = mx as f32 + 0.5;
            // a ring in the text colour keeps the dot visible on its own fill
            self.c.fill_circle(cx, cy, r + ring, self.t.text.alpha(0.85));
            self.c.fill_circle(cx, cy, r, *color);
        }
        let font = self.font(9.5, FW_NORMAL);
        self.tips.push(Tip { ax: Some(mx), x, y, w, h, font, text: label });
    }

    /// A 0..1 series filled, and a second 0..1 one over it as a bare line in its own colour
    /// (GPU load and the video memory in use).
    fn chart_with_line(&mut self, values: &[f32], line: &[f32], h_dip: f32, color: Color, line_color: Color) {
        let (x, y, w, h) = self.chart_box(h_dip);
        self.c.line_chart(x, y, w, h, values, color, color.alpha(0.3));
        if !line.is_empty() {
            self.c.line_chart(x, y, w, h, line, line_color, line_color.alpha(0.0));
        }
    }

    /// Two series sharing an automatic scale (bytes/s etc).
    fn chart2(&mut self, a: &[f32], b: &[f32], h_dip: f32, ca: Color, cb: Color, floor: f32) -> f32 {
        let max = a.iter().chain(b.iter()).cloned().fold(floor, f32::max);
        let na: Vec<f32> = a.iter().map(|v| v / max).collect();
        let nb: Vec<f32> = b.iter().map(|v| v / max).collect();
        let (x, y, w, h) = self.chart_box(h_dip);
        let lw = self.t.s(1.5) as f32;
        let fill = self.t.style.chart_fill() * 0.8;
        self.c.area_chart(x, y, w, h, &na, ca, fill, lw);
        self.c.area_chart(x, y, w, h, &nb, cb, fill, lw);
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
            self.tips.push(Tip { ax: None, x: bx, y: y + pad, w: bw, h: max_h as i32, font: f, text: format!("{name}: {}", pct(*v)) });
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
        let sw = self.t.s(8.0);
        // centred on the pie
        let rows_h: i32 = slices.iter().map(|(label, ..)| self.c.measure_text(label, lf).1 + self.t.s(4.0)).sum::<i32>() - self.t.s(4.0);
        let mut ly = self.y + self.t.s(2.0) + ((d - rows_h) / 2).max(0);
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
        // fewer rows when the popup would not fit on the screen
        let n = self.max_rows.map_or(n, |m| n.min(m)).max(1);
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
                    self.tips.push(Tip { ax: None, x: 0, y: iy, w: box_w, h: icon, font: f, text: t("popup.kill_process") });
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
                ProcMode::Gpu if p.vram > 0 => format!("{} · {}", pct(p.gpu), format_bytes(p.vram)),
                ProcMode::Gpu => pct(p.gpu),
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
    /// GPU load and GPU memory
    Gpu,
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
            format!("{} {}{}", g.round(), unit("gbit"), unit("per_s"))
        } else {
            format!("{g:.1} {}{}", unit("gbit"), unit("per_s"))
        }
    } else if bps >= 1_000_000 {
        format!("{} {}{}", bps / 1_000_000, unit("mbit"), unit("per_s"))
    } else {
        format!("{} kbit/s", bps / 1000)
    }
}

/// A temperature in the configured unit. The sensors and the remote protocol always speak
/// Celsius; the conversion happens here, at display time.
fn fmt_temp(c: f32, fahrenheit: bool) -> String {
    if fahrenheit {
        format!("{:.0} \u{00B0}F", c * 9.0 / 5.0 + 32.0)
    } else {
        format!("{c:.0} \u{00B0}C")
    }
}

/// A temperature difference (a guidance range, not a reading) in the configured unit.
/// Differences scale by 9/5 and must not take the +32 offset.
fn fmt_delta(lo: f32, hi: f32, fahrenheit: bool) -> String {
    if fahrenheit {
        format!("{:.0}-{:.0} \u{00B0}F", lo * 9.0 / 5.0, hi * 9.0 / 5.0)
    } else {
        format!("{lo:.0}-{hi:.0} \u{00B0}C")
    }
}

fn pct(v: f32) -> String {
    format!("{}%", (v * 100.0).round() as i32)
}

// ---------------------------------------------------------------- modules

/// "3.9 GHz" or "800 MHz".
fn clock_text(mhz: u32) -> String {
    if mhz >= 1000 {
        format!("{:.1} {}", mhz as f64 / 1000.0, unit("ghz"))
    } else {
        format!("{mhz} {}", unit("mhz"))
    }
}

/// "12 345" with thin grouping, for counts and rates.
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

/// "3 d 04:12" / "04:12" (hours:minutes).
fn uptime_text(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 {
        format!("{} {h:02}:{m:02}", tf("popup.days", &[("n", &d.to_string())]))
    } else {
        format!("{h:02}:{m:02}")
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
    // the name on the left, the temperature (where known) on the right
    let temp = m.cpu_temp.map(|c| fmt_temp(c, ui.t.temp_f)).unwrap_or_default();
    if m.cpu_socket_names.len() > 1 && !m.offline {
        // several physical processors: each one under the title with its own load
        let lines: Vec<(String, String, Color, Option<Action>)> = m
            .cpu_socket_names
            .iter()
            .enumerate()
            .map(|(p, name)| {
                let loads: Vec<f32> = m.cpu_cores.iter().zip(&m.cpu_packages).filter(|(_, &k)| k as usize == p).map(|(v, _)| *v).collect();
                let load = if loads.is_empty() { String::new() } else { pct(loads.iter().sum::<f32>() / loads.len() as f32) };
                (name.clone(), load, ui.t.text, None)
            })
            .collect();
        ui.title_list(&head(m, "popup.cpus"), &temp, &lines);
    } else if m.offline || m.cpu_name.is_empty() {
        ui.title_r(&head(m, "popup.cpu"), &sub(m, &m.cpu_name), &temp);
    } else {
        // the model in full text colour with the temperature on its line, as the GPU popup does
        ui.title_list(&head(m, "popup.cpu"), "", &[(m.cpu_name.clone(), temp, ui.t.text, None)]);
    }
    let hist: Vec<f32> = m.cpu_history.iter().cloned().collect();
    let cpu_color = ui.t.chart_color(Kind::Cpu, 0.0);
    ui.chart(&hist, 70.0, cpu_color);
    ui.probe(&[(&hist, cpu_color)], |at| at(hist.len()).map(|i| format!("{}: {}", t("popup.load"), pct(hist[i]))).unwrap_or_default());
    // the rows stay in place; which of them show with "More" closed is the user's choice (a click
    // on a label with it open), the diagnostics and fixed facts fold away by default
    let more = ui.expanded.contains("more:cpu");
    ui.fold_open = more;
    let text = ui.t.text;
    ui.frow("cpu.load", true, &t("popup.load"), &pct(m.cpu_total), ui.t.usage(m.cpu_total, text));
    // kernel / programs in one line: who is busy
    ui.frow("cpu.system_user", true, &format!("{} / {}", t("popup.system"), t("popup.user")), &format!("{} / {}", pct(m.cpu_system), pct(m.cpu_user)), text);
    let x = &m.sys;
    if m.cpu_hybrid() {
        // load and current clock per core class
        let (pn, en, pl, el) = m.cpu_pe();
        let top = m.cpu_classes.iter().cloned().max().unwrap_or(0);
        let low = m.cpu_classes.iter().cloned().min().unwrap_or(0);
        ui.frow("cpu.p_cores", true, &tf("popup.p_cores", &[("n", &pn.to_string())]), &with_clock(pct(pl), m.cpu_clock(Some(top))), text);
        ui.frow("cpu.e_cores", true, &tf("popup.e_cores", &[("n", &en.to_string())]), &with_clock(pct(el), m.cpu_clock(Some(low))), text);
    } else if let Some(mhz) = m.cpu_clock(None) {
        ui.frow("cpu.clock", true, &t("popup.clock"), &clock_text(mhz), text);
    }
    // held back by heat, power or firmware limits (shown only while it bites)
    if let Some(v) = x.perf_limit.filter(|v| *v < 0.995) {
        ui.frow("cpu.perf_limit", true, &t("popup.perf_limit"), &pct(v), RED);
    }
    if x.processes > 0 {
        ui.frow("cpu.procs_threads", true, &t("popup.procs_threads"), &format!("{} / {}", grouped(x.processes as u64), grouped(x.threads as u64)), text);
    }
    if m.load_avg.len() == 3 {
        ui.frow("cpu.load_avg", true, &t("popup.load_avg"), &format!("{:.2} / {:.2} / {:.2}", m.load_avg[0], m.load_avg[1], m.load_avg[2]), text);
    }
    // machine fans of an agent (single-board computers): "Fan  40% · 1490 RPM"
    for (i, f) in m.fans.iter().enumerate() {
        let label = if m.fans.len() > 1 { format!("{} {}", t("popup.fan"), i + 1) } else { t("popup.fan") };
        let value = match (f.duty, f.rpm) {
            (Some(d), Some(r)) => format!("{} · {r} {}", pct(d), unit("rpm")),
            (Some(d), None) => pct(d),
            (None, Some(r)) => format!("{r} {}", unit("rpm")),
            _ => continue,
        };
        ui.frow("cpu.fan", true, &label, &value, text);
    }
    // what an agent's machine draws: "Power  12.4 W" (a Mac), "Power VDD_IN  7.2 W" (a Jetson)
    for p in &m.power {
        let label = if m.power.len() > 1 { format!("{} {}", t("popup.power"), p.id) } else { t("popup.power") };
        ui.frow("cpu.power", true, &label, &format!("{:.1} {}", p.w, unit("w")), text);
    }
    ui.frow("cpu.idle", false, &t("popup.idle"), &pct((1.0 - m.cpu_total).max(0.0)), text);
    if x.processes > 0 {
        ui.frow("cpu.handles", false, &t("popup.handles"), &grouped(x.handles as u64), text);
    }
    if x.uptime_secs > 0 {
        ui.frow("cpu.uptime", false, &t("popup.uptime"), &uptime_text(x.uptime_secs), text);
    }
    if let Some(v) = x.ctx_switches {
        ui.frow("cpu.ctx_switches", false, &t("popup.ctx_switches"), &format!("{}{}", grouped(v.round() as u64), unit("per_s")), text);
    }
    if let Some(v) = x.interrupts {
        let mut value = format!("{}{}", grouped(v.round() as u64), unit("per_s"));
        if let (Some(dpc), Some(irq)) = (x.dpc_time, x.interrupt_time) {
            value.push_str(&format!(" · DPC {:.1}% · ISR {:.1}%", dpc * 100.0, irq * 100.0));
        }
        ui.frow("cpu.interrupts", false, &t("popup.interrupts"), &value, text);
    }
    if m.cpu_sockets > 1 {
        ui.frow("cpu.sockets", false, &t("popup.sockets"), &m.cpu_sockets.to_string(), text);
    }
    let c = &m.sys.cpu;
    if c.cache.iter().any(|v| *v > 0) {
        // only the levels the CPU has: "Cache L1 / L2" on a Core 2 without L3
        let levels: Vec<usize> = (0..3).filter(|i| c.cache[*i] > 0).collect();
        let mut label = t("popup.cache");
        if let Some(i) = label.find(" L1") {
            label.truncate(i);
            label.push(' ');
            label.push_str(&levels.iter().map(|i| format!("L{}", i + 1)).collect::<Vec<_>>().join(" / "));
        }
        ui.frow("cpu.cache", false, &label, &levels.iter().map(|i| format_bytes(c.cache[*i])).collect::<Vec<_>>().join(" / "), text);
    }
    match (c.base_mhz, c.max_mhz) {
        (Some(b), Some(x)) if x > b => {
            ui.frow("cpu.base_clock", false, &t("popup.base_max_clock"), &format!("{} / {}", clock_text(b), clock_text(x)), text);
        }
        (Some(b), _) => {
            ui.frow("cpu.base_clock", false, &t("popup.base_clock"), &clock_text(b), text);
        }
        _ => {}
    }
    if !c.features.is_empty() {
        // a click on the row opens the full list of extensions the CPU reports
        ui.frow_x("cpu.instructions", false, &t("popup.instructions"), &c.features, text, Extra::Expand("cpu_features", c.all_features.clone()));
    }
    // under a hypervisor (Hyper-V, VBS) Windows reports the firmware switch as off
    if c.hypervisor {
        ui.frow("cpu.virtualization", false, &t("popup.virtualization"), &t("popup.hypervisor"), text);
    } else if let Some(v) = c.virtualization {
        ui.frow("cpu.virtualization", false, &t("popup.virtualization"), &t(if v { "popup.enabled" } else { "popup.disabled" }), text);
    }
    ui.flush_fold();
    ui.more_toggle("more:cpu", more);
    let sockets = if m.cpu_sockets > 1 { tf("popup.sockets_suffix", &[("n", &m.cpu_sockets.to_string())]) } else { String::new() };
    if m.cpu_hybrid() {
        let (pn, en, _, _) = m.cpu_pe();
        ui.section(&tf("popup.cores_pe", &[("p", &pn.to_string()), ("e", &en.to_string()), ("sockets", &sockets)]));
    } else {
        ui.section(&tf("popup.cores", &[("n", &m.cpu_cores.len().to_string()), ("sockets", &sockets)]));
    }
    let breaks = m.cpu_breaks();
    ui.cores(&m.cpu_cores, &m.cpu_classes, &breaks, &m.cpu_smt, &m.cpu_ids);
    // a remote machine's system: "Ubuntu 24.04.3 LTS", the kernel, the architecture, the agent
    let os = &m.os;
    if !os.release.is_empty() || !os.kernel.is_empty() {
        ui.section(&t("popup.os_section"));
        for (key, v) in [("popup.os_release", &os.release), ("popup.kernel", &os.kernel), ("popup.arch", &os.arch), ("popup.agent", &os.agent)] {
            if !v.is_empty() {
                ui.row(&t(key), v);
            }
        }
    }
    ui.proc_section(&t("popup.top_processes"), m.host.is_empty());
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
    let mem_color = ui.t.chart_color(Kind::Mem, 0.0);
    ui.chart(&hist, 70.0, mem_color);
    let total = m.mem.total;
    ui.probe(&[(&hist, mem_color)], |at| at(hist.len()).map(|i| format!("{}: {} \u{00B7} {}", t("popup.usage"), pct(hist[i]), format_bytes((hist[i].clamp(0.0, 1.0) as f64 * total as f64) as u64))).unwrap_or_default());
    // the rows stay in place; which of them show with "More" closed is the user's choice
    let more = ui.expanded.contains("more:mem");
    ui.fold_open = more;
    let text = ui.t.text;
    ui.frow("mem.usage", true, &t("popup.usage"), &pct(m.mem.pct), ui.t.usage(m.mem.pct, text));
    ui.frow("mem.used", true, &t("popup.used"), &format_bytes(m.mem.used), text);
    ui.frow("mem.free", true, &t("popup.free"), &format_bytes(m.mem.total.saturating_sub(m.mem.used)), text);
    ui.frow("mem.committed", true, &t("popup.committed"), &format!("{} / {}", format_bytes(m.mem.commit_used), format_bytes(m.mem.commit_total)), text);
    if m.sys.pagefile_total > 0 {
        ui.frow("mem.pagefile", false, &t("popup.pagefile"), &format!("{} / {}", format_bytes(m.sys.pagefile_used), format_bytes(m.sys.pagefile_total)), text);
    }
    if m.sys.hw_reserved >= 1024 * 1024 {
        ui.frow("mem.hw_reserved", false, &t("popup.hw_reserved"), &format_bytes(m.sys.hw_reserved), text);
    }
    ui.flush_fold();
    ui.more_toggle("more:mem", more);
    if m.mem_processes > 0 {
        // where the physical memory goes: process private sets, kernel pools, the rest of "used"
        // (driver-locked pages such as VM memory, shared/mapped pages, hardware reserved) and
        // what is still available (the standby file cache lives in there)
        let kernel = m.mem_kernel_paged + m.mem_kernel_nonpaged;
        let other = m.mem.used.saturating_sub(m.mem_processes).saturating_sub(kernel);
        let available = m.mem.total.saturating_sub(m.mem.used);
        ui.breakdown_section(&t("popup.where_it_goes"));
        let parts = [
            // red, not the accent: a dark system accent vanishes on the dark popup
            (t("popup.processes_private"), m.mem_processes, Color::rgb(0xFF, 0x45, 0x3A)),
            (t("popup.kernel_pools"), kernel, Color::rgb(0xAF, 0x52, 0xDE)),
            (t("popup.other_memory"), other, Color::rgb(0xFF, 0x95, 0x00)),
            (t("popup.available_memory"), available, ui.t.dim.alpha(0.45)),
        ];
        // the pie with its legend, or just one bar whose parts name themselves on hover
        if ui.mem_bar {
            ui.stacked_bar(&parts);
        } else {
            ui.pie(&parts);
        }
        ui.row(&t("popup.file_cache"), &format_bytes(m.mem_cache));
    }
    ui.proc_section(&t("popup.top_processes"), m.host.is_empty());
    ui.processes(&m.top_mem, ProcMode::Mem, m.top_n, m.host.is_empty());
}

fn draw_gpu(ui: &mut Ui, m: &MetricsData) {
    let temp = m.gpu_perf.temp.map(|c| fmt_temp(c, ui.t.temp_f)).unwrap_or_default();
    if m.gpu_adapters.is_empty() || m.offline {
        ui.title_r(&head(m, "popup.gpu"), &sub(m, &m.gpu_name), &temp);
    } else {
        // every adapter under the title, the temperature on the right (the sensors are read for
        // the shown one only, an iGPU has none worth showing); the shown one is bright, a click
        // switches the popup and the widget to another
        let primary = m.gpu_primary.as_ref().map(|p| p.luid);
        let several = m.gpu_adapters.len() > 1 && m.host.is_empty();
        let lines: Vec<(String, String, Color, Option<Action>)> = m
            .gpu_adapters
            .iter()
            .map(|a| {
                let shown = Some(a.luid) == primary;
                let right = if shown && a.dedicated >= 512 * 1024 * 1024 { temp.clone() } else { String::new() };
                let color = if shown { ui.t.text } else { ui.t.dim };
                let action = (several && !shown).then(|| Action::SelectGpu(a.name.clone()));
                // the card's own memory next to its name: "AMD Radeon RX 9070 XT · 16 GB"
                let name = match vram_size(a.dedicated) {
                    Some(size) => format!("{} \u{00B7} {size}", a.name),
                    None => a.name.clone(),
                };
                (name, right, color, action)
            })
            .collect();
        let key = if m.gpu_adapters.len() > 1 { "popup.gpus" } else { "popup.gpu" };
        ui.title_list(&head(m, key), "", &lines);
    }
    // the load filled, the video memory in use as a line over it in the memory colour
    let hist: Vec<f32> = m.gpu_history.iter().cloned().collect();
    let vram: Vec<f32> = m.vram_history.iter().cloned().collect();
    let load_color = ui.t.chart_color(Kind::Gpu, 0.0);
    // the memory module's colour, or orange where it is the same as the GPU's (both the accent)
    let mem_color = ui.t.chart_color(Kind::Mem, 0.0);
    let vram_color = if mem_color == load_color { Color::rgb(0xFF, 0x95, 0x00) } else { mem_color };
    ui.chart_with_line(&hist, &vram, 70.0, load_color, vram_color);
    // the video memory in bytes from the shown adapter's size (an agent sends its total)
    let vram_size = m.gpu_primary.as_ref().map(|a| a.dedicated).filter(|d| *d > 0).unwrap_or(m.vram_total);
    ui.probe(&[(&hist, load_color), (&vram, vram_color)], |at| {
        let mut parts = Vec::new();
        if let Some(i) = at(hist.len()) {
            parts.push(format!("{}: {}", t("popup.load"), pct(hist[i])));
        }
        if let Some(i) = at(vram.len()) {
            let used = (vram[i].clamp(0.0, 1.0) as f64 * vram_size as f64) as u64;
            parts.push(format!("{}: {} ({})", t("popup.video_memory"), format_bytes(used), pct(vram[i])));
        }
        parts.join(" \u{00B7} ")
    });
    // the rows stay in place; which of them show with "More" closed is the user's choice
    let more = ui.expanded.contains("more:gpu");
    ui.fold_open = more;
    let text = ui.t.text;
    match m.gpu {
        Some(v) => ui.frow("gpu.load", true, &t("popup.load"), &pct(v), ui.t.usage(v, text)),
        None => ui.frow("gpu.load", true, &t("popup.load"), &t("popup.na"), text),
    }
    // memory of the primary adapter in use: "1.2 / 5.9 GB · shared 0.4 GB"
    let primary = m.gpu_primary.as_ref().and_then(|p| m.gpu_adapters.iter().find(|a| a.luid == p.luid));
    let adapter_mem = primary.map(adapter_mem).unwrap_or_default();
    if !adapter_mem.is_empty() {
        ui.frow("gpu.video_memory", true, &t("popup.video_memory"), &adapter_mem, text);
    }
    // whatever the driver reports (D3DKMT perf data). Clocks and power keep their rows from the
    // start, with a dash until the first reading, so the popup does not grow under the pointer;
    // a local reading keeps its last value until the next one (see Perf::or)
    let p = m.gpu_perf;
    let fixed = m.host.is_empty() && !m.gpu_adapters.is_empty();
    const DASH: &str = "\u{2014}";
    match p.core_mhz {
        Some(mhz) => ui.frow("gpu.clock", true, &t("popup.clock"), &clock_now_max(mhz, p.core_max_mhz), text),
        None if fixed => ui.frow("gpu.clock", true, &t("popup.clock"), DASH, text),
        None => {}
    }
    match p.mem_mhz {
        Some(mhz) => ui.frow("gpu.mem_clock", true, &t("popup.mem_clock"), &clock_now_max(mhz, p.mem_max_mhz), text),
        None if fixed => ui.frow("gpu.mem_clock", true, &t("popup.mem_clock"), DASH, text),
        None => {}
    }
    let power = match (p.power_w, p.power_limit_w, p.power) {
        (Some(w), Some(l), _) => Some(format!("{} / {l:.0} {}", watts(w), unit("w"))),
        (Some(w), None, _) => Some(format!("{} {}", watts(w), unit("w"))),
        (None, _, Some(v)) => Some(tf("popup.power_budget", &[("pct", &pct(v))])),
        _ if fixed => Some(DASH.to_string()),
        _ => None,
    };
    if let Some(v) = power {
        ui.frow("gpu.power", true, &t("popup.power"), &v, text);
    }
    if let Some(h) = p.hotspot {
        // the hottest point of the die; the tooltip says what the gap to the core reading means
        // the guidance ranges follow the selected unit: they are differences, not readings
        let tip = tf("popup.hotspot_tip", &[("gap", &fmt_delta(5.0, 15.0, ui.t.temp_f)), ("big", &fmt_delta(20.0, 30.0, ui.t.temp_f))]);
        ui.frow_x("gpu.hotspot", true, &t("popup.hotspot"), &fmt_temp(h, ui.t.temp_f), text, Extra::Tip(tip));
    }
    let fan = match (p.fan_pct, p.fan_rpm) {
        (Some(f), Some(r)) => Some(format!("{f}% · {r} {}", unit("rpm"))),
        (Some(f), None) => Some(format!("{f}%")),
        (None, Some(r)) => Some(format!("{r} {}", unit("rpm"))),
        _ => None,
    };
    if let Some(v) = fan {
        ui.frow("gpu.fan", false, &t("popup.fan"), &v, text);
    }
    // "PCIe 3.0 x16 (max 3.0 x16) · 1.2 GB/s"
    let mut link = String::new();
    if let (Some(g), Some(w)) = (p.pcie_gen, p.pcie_width) {
        link = format!("{}.0 x{w}", g);
        if let (Some(mg), Some(mw)) = (p.pcie_max_gen, p.pcie_max_width) {
            if (mg, mw) != (g, w) {
                link.push_str(&format!(" ({})", tf("popup.max", &[("v", &format!("{mg}.0 x{mw}"))])));
            }
        }
    }
    if let Some(bw) = p.pcie_bw {
        if !link.is_empty() {
            link.push_str(" · ");
        }
        link.push_str(&format_bytes_rate(bw));
    }
    if !link.is_empty() {
        ui.frow("gpu.pcie", false, "PCIe", &link, text);
    }
    if let Some(bw) = p.mem_bw {
        ui.frow("gpu.mem_bandwidth", false, &t("popup.mem_bandwidth"), &format_bytes_rate(bw), text);
    }
    if let Some(a) = &m.gpu_primary {
        // SoC GPUs of remote machines have no PCI ids
        if a.vendor_id != 0 {
            let vendor = a.vendor();
            let ids = format!("{:04X}:{:04X}", a.vendor_id, a.device_id);
            ui.frow("gpu.vendor_device", false, &t("popup.vendor_device"), &if vendor.is_empty() { ids } else { format!("{vendor} {ids}") }, text);
        }
        if !a.driver.is_empty() {
            ui.frow("gpu.driver", false, &t("popup.driver"), &a.driver, text);
        }
    }
    ui.flush_fold();
    ui.more_toggle("more:gpu", more);
    // DXGI usage is this process's own; the adapter counters above replace it locally
    if adapter_mem.is_empty() {
        if m.vram_budget > 0 {
            let frac = m.vram_used as f32 / m.vram_budget as f32;
            ui.section(&t("popup.video_memory"));
            ui.bar_row(&t("popup.used"), frac, &format_bytes(m.vram_used), ui.t.accent);
            ui.row(&t("popup.budget"), &format_bytes(m.vram_budget));
        }
        if m.vram_total > 0 {
            ui.row(&t("popup.dedicated"), &format_bytes(m.vram_total));
        }
    }
    // per-process GPU use: where the GPU counters exist (Windows 10 1709+), or what an agent
    // sends (Linux DRM fdinfo, NVML)
    let local = m.host.is_empty();
    if (local && m.gpu_available) || (!local && !m.top_gpu.is_empty()) {
        // ordered by GPU load or by video memory (an agent sends its top by load: re-sorted here)
        ui.proc_section_sorted(&t("popup.top_processes"), local, Some(ui.gpu_by_vram));
        let mut list = m.top_gpu.clone();
        if !local {
            crate::metrics::processes::sort_gpu(&mut list, ui.gpu_by_vram);
        }
        ui.processes(&list, ProcMode::Gpu, m.top_n, local);
    }
}

/// A card's own video memory as its box says it: whole gigabytes ("16 GB" for the 15.8 GiB
/// Windows reports); None for an integrated GPU without memory of its own.
fn vram_size(bytes: u64) -> Option<String> {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let gib = bytes as f64 / GIB;
    if gib >= 1.0 {
        Some(format!("{} {}", gib.round() as u64, unit("gb")))
    } else if bytes >= 512 * 1024 * 1024 {
        Some(format_bytes(bytes))
    } else {
        None
    }
}

/// Adapter memory in use where the counters have it: "1.2 / 5.9 GB · shared 0.4 GB"; an iGPU's
/// small dedicated carve-out stays empty, only its shared use is shown then. "" when unknown.
fn adapter_mem(a: &crate::metrics::gpu::AdapterInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (a.dedicated > 0, a.dedicated_used) {
        (true, Some(0)) if a.shared_used.map_or(false, |u| u >= 1024 * 1024) => {}
        (true, Some(u)) => parts.push(format!("{} / {}", format_bytes(u), format_bytes(a.dedicated))),
        _ => {}
    }
    if let Some(u) = a.shared_used.filter(|u| *u >= 1024 * 1024 || a.dedicated == 0) {
        parts.push(format!("{} {}", t("popup.shared"), format_bytes(u)));
    }
    parts.join(" \u{00B7} ")
}

/// Watts without a fraction above 10 W, one decimal below, two below 1 W.
fn watts(w: f32) -> String {
    if w < 10.0 {
        format!("{w:.1}")
    } else {
        format!("{w:.0}")
    }
}

/// "1100 / 1366 MHz": the current clock and the card's maximum where known (the GPU drops to its
/// lowest step at idle, so the current one alone says little).
fn clock_now_max(now: u32, max: Option<u32>) -> String {
    match max.filter(|m| *m > 0) {
        Some(m) => format!("{now} / {m} {}", unit("mhz")),
        None => format!("{now} {}", unit("mhz")),
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
    let scaled = |v: &[f32]| -> Vec<f32> { v.iter().map(|x| x / max).collect() };
    let (nrx, ntx) = (scaled(&rx), scaled(&tx));
    ui.probe(&[(&nrx, DOWN), (&ntx, UP)], |at| {
        let mut parts = Vec::new();
        if let Some(i) = at(rx.len()) {
            parts.push(format!("{down}: {}", rate(rx[i] as f64)));
        }
        if let Some(i) = at(tx.len()) {
            parts.push(format!("{up}: {}", rate(tx[i] as f64)));
        }
        parts.join(" \u{00B7} ")
    });
    ui.legend(&[(&down, ui.on_off(DOWN, show_rx)), (&up, ui.on_off(UP, show_tx)), (&tf("popup.scale", &[("rate", &rate(max as f64))]), ui.t.dim.alpha(0.0))]);
    let start = ui.y;
    ui.row_lv(&down, &rate(m.net_rx), ui.on_off(ui.t.dim, show_rx), ui.on_off(DOWN, show_rx));
    ui.clickable(start, Action::ToggleNetSeries(true));
    let start = ui.y;
    ui.row_lv(&up, &rate(m.net_tx), ui.on_off(ui.t.dim, show_tx), ui.on_off(UP, show_tx));
    ui.clickable(start, Action::ToggleNetSeries(false));
    // remote machines send no ping readings and have no DNS to change from here
    let local = m.host.is_empty();
    ping_block(ui, 0, &m.net_ping_host, m.net_latency, m.net_jitter, local);
    ping_block(ui, 1, &m.net_ping_host2, m.net_latency2, m.net_jitter2, local);
    if local {
        dns_block(ui, m);
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

/// One ping target (slot 0 or 1) while the popup is open: its readings, a clickable block that
/// turns into a text field, or, without a target, the way to set one.
fn ping_block(ui: &mut Ui, slot: u8, host: &str, latency: Option<f32>, jitter: Option<f32>, local: bool) {
    if let Some(e) = ui.ping_edit.clone().filter(|e| local && e.slot == slot) {
        ping_field(ui, &e);
        return;
    }
    if !host.is_empty() {
        // ICMP probe to the target while the popup is open
        let ms = |v: Option<f32>| v.map(|v| format!("{v:.1} {}", unit("ms"))).unwrap_or_else(|| t("popup.na"));
        let start = ui.y;
        ui.section(&tf("popup.ping", &[("host", host)]));
        ui.row(&t("popup.latency"), &ms(latency));
        ui.row(&t("popup.jitter"), &ms(jitter));
        if local {
            // the whole block becomes a text field on a click
            ui.clickable(start, Action::EditPingHost(slot));
            ui.hover_tip(start, t("power.ping_tip"));
        }
    } else if local && slot == 0 {
        // ping switched off: the block stays, so it can be switched on again from here
        let start = ui.y;
        ui.section(&t("power.ping_off_title"));
        ui.row_lv(&t("power.ping_off"), "", ui.t.dim, ui.t.dim);
        ui.clickable(start, Action::EditPingHost(slot));
        ui.hover_tip(start, t("power.ping_tip"));
    } else if local {
        // no second target: one quiet line to add it
        ui.space(4.0);
        let start = ui.y;
        let f = ui.font(10.0, FW_NORMAL);
        let (_, h) = ui.c.draw_text(&t("power.ping_add"), 0, ui.y, f, ui.t.accent);
        ui.y += h + ui.t.s(2.0);
        ui.clickable(start, Action::EditPingHost(slot));
    }
}

/// "1.1.1.1 · 1.0.0.1 (+2 IPv6)": the IPv4 servers, the IPv6 ones counted.
fn dns_servers_text(servers: &[std::net::IpAddr]) -> String {
    let v4: Vec<String> = servers.iter().filter(|s| s.is_ipv4()).map(|s| s.to_string()).collect();
    let v6 = servers.iter().filter(|s| s.is_ipv6()).count();
    let mut out = if v4.is_empty() { servers.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" \u{00B7} ") } else { v4.join(" \u{00B7} ") };
    if !v4.is_empty() && v6 > 0 {
        out.push_str(&format!(" (+{v6} IPv6)"));
    }
    out
}

/// The response time of `ip` as a short label: measured ms (green for the fastest), "…" while
/// it is still to come, "–" without an answer. None when it is not measured.
fn dns_speed(m: &MetricsData, ip: Option<std::net::IpAddr>, dim: Color) -> Option<(String, Color)> {
    let ip = ip?;
    let fastest = m.dns_speed.iter().filter_map(|(_, ms)| *ms).fold(f32::INFINITY, f32::min);
    match m.dns_speed.iter().find(|(s, _)| *s == ip) {
        Some((_, Some(ms))) => {
            let text = if *ms < 10.0 { format!("{ms:.1} {}", unit("ms")) } else { format!("{ms:.0} {}", unit("ms")) };
            let done = !m.dns_speed_running && m.dns_intercepted != Some(true);
            Some((text, if done && *ms <= fastest { GREEN } else { dim }))
        }
        Some((_, None)) => Some(("\u{2013}".into(), dim)),
        None if m.dns_speed_running => Some(("\u{2026}".into(), dim)),
        None => None,
    }
}

/// One choice of the DNS list: a radio dot, the name, the response time, the servers dimmed on
/// the right.
fn dns_choice(ui: &mut Ui, chosen: bool, name: &str, detail: &str, speed: Option<(String, Color)>, action: Action) {
    let f = ui.font(11.0, FW_NORMAL);
    let fd = ui.font(9.5, FW_NORMAL);
    let start = ui.y;
    let h = ui.c.measure_text("Xg", f).1;
    let r = ui.t.s(5.0) as f32;
    let cy = ui.y as f32 + h as f32 / 2.0;
    let cx = r + ui.t.s(1.0) as f32;
    if chosen {
        ui.c.fill_circle(cx, cy, r, ui.t.accent);
        ui.c.fill_circle(cx, cy, r * 0.4, if ui.t.light { Color::rgb(255, 255, 255) } else { Color::rgb(0x20, 0x20, 0x20) });
    } else {
        ui.c.fill_circle(cx, cy, r, ui.t.dim.alpha(0.55));
        ui.c.fill_circle(cx, cy, r - ui.t.s(1.2) as f32, if ui.t.light { Color::rgb(0xF6, 0xF6, 0xF6) } else { Color::rgb(0x26, 0x26, 0x26) });
    }
    let x = ui.t.s(18.0);
    // the response time sits in a fixed column after the names; the servers take what is left
    let nw = ui.c.measure_text(name, f).0;
    let sx = x + ui.t.s(78.0).max(nw + ui.t.s(8.0));
    let sw = ui.t.s(44.0);
    let (dw, dh) = ui.c.measure_text(detail, fd);
    let dw = dw.min(ui.w - sx - sw - ui.t.s(6.0)).max(0);
    ui.clipped(name, x, ui.y, sx - x - ui.t.s(4.0), f, if chosen { ui.t.text } else { ui.t.text.alpha(0.85) });
    if let Some((s, col)) = speed {
        let fs = ui.font(9.5, if col == GREEN { FW_SEMIBOLD } else { FW_NORMAL });
        let ssh = ui.c.measure_text(&s, fs).1;
        ui.c.draw_text(&s, sx, ui.y + (h - ssh) / 2, fs, col);
    }
    ui.clipped(detail, ui.w - dw, ui.y + (h - dh) / 2, dw, fd, ui.t.dim);
    ui.y += h + ui.t.s(5.0);
    ui.clickable(start, action);
}

/// DNS of the active interface: what is set now; a click opens the list (Automatic, the
/// presets, Custom). A change runs elevated (one UAC prompt).
fn dns_block(ui: &mut Ui, m: &MetricsData) {
    use crate::metrics::dns::{Choice, PRESETS};
    let Some(d) = m.dns.as_ref() else { return };
    if let Some(e) = ui.ping_edit.clone().filter(|e| e.slot == 2) {
        ping_field(ui, &e);
        return;
    }
    let choice = d.choice();
    let name = match choice {
        Choice::Automatic => t("power.dns_auto"),
        Choice::Preset(i) => PRESETS[i].name.to_string(),
        Choice::Custom => t("power.dns_custom"),
    };
    let open = ui.expanded.contains("dns");
    let start = ui.y;
    ui.section(&tf("power.dns_title", &[("iface", &d.alias)]));
    ui.row(&name, &dns_servers_text(&d.servers));
    if m.dns_busy {
        let f = ui.font(9.5, FW_NORMAL);
        let (_, h) = ui.c.draw_text_clipped(&t("power.dns_busy"), 0, ui.y, ui.w, f, ui.t.accent);
        ui.y += h + ui.t.s(3.0);
    }
    let caught = m.dns_intercepted == Some(true);
    let warn = if ui.t.light { ORANGE } else { YELLOW };
    if caught {
        let f = ui.font(9.5, FW_NORMAL);
        let (_, h) = ui.c.draw_text_clipped(&format!("\u{26A0} {}", t("power.dns_intercepted")), 0, ui.y, ui.w, f, warn);
        ui.y += h + ui.t.s(3.0);
    }
    toggle_link(ui, "dns", open, &t("power.dns_change"), &t("power.dns_close"));
    ui.clickable(start, Action::Expand("dns".into()));
    if !open {
        ui.hover_tip(start, t("power.dns_tip"));
        return;
    }
    ui.space(3.0);
    let dim = ui.t.dim;
    // the automatic servers are only known while they are the ones in use
    let auto_ip = if choice == Choice::Automatic { d.servers.first().cloned() } else { None };
    dns_choice(ui, choice == Choice::Automatic, &t("power.dns_auto"), &t("power.dns_auto_detail"), dns_speed(m, auto_ip, dim), Action::Dns(-1));
    for (i, p) in PRESETS.iter().enumerate() {
        let detail = p.servers.iter().filter(|s| !s.contains(':')).cloned().collect::<Vec<_>>().join(" \u{00B7} ");
        let ip = p.servers.first().and_then(|s| s.parse().ok());
        dns_choice(ui, choice == Choice::Preset(i), p.name, &detail, dns_speed(m, ip, dim), Action::Dns(i as i8));
    }
    // Custom applies the saved servers like a preset; without any it opens the editor
    if m.dns_custom.is_empty() {
        dns_choice(ui, choice == Choice::Custom, &t("power.dns_custom"), &t("power.dns_custom_set"), None, Action::EditPingHost(2));
    } else {
        let ip = crate::metrics::dns::parse_servers(&m.dns_custom).and_then(|l| l.first().cloned());
        dns_choice(ui, choice == Choice::Custom, &t("power.dns_custom"), &m.dns_custom.replace(", ", " \u{00B7} "), dns_speed(m, ip, dim), Action::DnsCustom);
        let f = ui.font(9.5, FW_NORMAL);
        let start = ui.y;
        let x = ui.t.s(18.0);
        let (_, h) = ui.c.draw_text(&t("power.dns_edit"), x, ui.y, f, ui.t.accent);
        ui.y += h + ui.t.s(4.0);
        ui.clickable(start, Action::EditPingHost(2));
    }
    let f = ui.font(9.0, FW_NORMAL);
    let col = ui.t.dim.alpha(0.8);
    if caught {
        battery_view::wrap(ui, &t("power.dns_intercepted_note"), f, warn);
        ui.y += ui.t.s(2.0);
    }
    battery_view::wrap(ui, &t("power.dns_speed_note"), f, col);
    ui.y += ui.t.s(2.0);
    battery_view::wrap(ui, &t("power.dns_uac_note"), f, col);
}

/// A right-aligned accent link with a chevron that opens / closes the block `key`.
fn toggle_link(ui: &mut Ui, key: &str, open: bool, closed_text: &str, open_text: &str) {
    let f = ui.font(9.5, FW_NORMAL);
    let text = if open { open_text } else { closed_text };
    let start = ui.y;
    let (cw, ch) = (ui.t.s(7.0) as f32, ui.t.s(3.5) as f32);
    let (tw, h) = ui.c.measure_text(text, f);
    let tx = ui.w - tw - ui.t.s(5.0) - cw as i32;
    ui.c.draw_text(text, tx, ui.y, f, ui.t.accent);
    let cx = (ui.w - cw as i32) as f32;
    let cy = ui.y as f32 + h as f32 / 2.0 + 1.0;
    let (top, tip) = if open { (cy + ch / 2.0, cy - ch / 2.0) } else { (cy - ch / 2.0, cy + ch / 2.0) };
    let width = ui.t.s(1.2) as f32;
    ui.c.draw_line(cx, top, cx + cw / 2.0, tip, width, ui.t.accent);
    ui.c.draw_line(cx + cw / 2.0, tip, cx + cw, top, width, ui.t.accent);
    ui.y += h + ui.t.s(3.0);
    ui.clickable(start, Action::Expand(key.to_string()));
}

/// A text field in the network popup (ping targets, custom DNS servers), in the look of a
/// Windows 11 text box: card fill, accent underline (red after a refused entry), selection,
/// caret, and a hint line.
fn ping_field(ui: &mut Ui, e: &EditView) {
    let dns = e.slot == 2;
    let title = match e.slot {
        0 => t("power.ping_off_title"),
        1 => format!("{} 2", t("power.ping_off_title")),
        _ => t("power.dns_custom_title"),
    };
    ui.section(&title);
    for (i, fv) in e.fields.iter().enumerate() {
        let focused = i == e.focus;
        if dns {
            // which server this is, above the field
            let fc = ui.font(9.5, FW_NORMAL);
            let (_, ch) = ui.c.draw_text(&t(if i == 0 { "power.dns_primary" } else { "power.dns_secondary" }), 0, ui.y, fc, ui.t.dim);
            ui.y += ch + ui.t.s(3.0);
        }
        let h = ui.t.s(32.0);
        let r = ui.t.s(6.0) as f32;
        let top = ui.y;
        let (fill, stroke) = ui.t.style.card(ui.t.light);
        ui.c.fill_round_rect(0, top, ui.w, h, r, fill);
        if focused {
            // a touch stronger than a card: it has the keyboard
            ui.c.fill_round_rect(0, top, ui.w, h, r, ui.t.text.alpha(0.04));
        }
        ui.c.stroke_round_rect_r(0, top, ui.w, h, r, stroke.unwrap_or(ui.t.dim.alpha(0.22)));
        let lh = ui.t.s(if focused { 2.0 } else { 1.0 });
        let line = if e.error && focused { RED } else if focused { ui.t.accent } else { ui.t.dim.alpha(0.5) };
        ui.c.fill_round_rect(ui.t.s(1.0), top + h - lh, ui.w - ui.t.s(2.0), lh, lh as f32 / 2.0, line);
        let f = ui.font(11.5, FW_NORMAL);
        let pad = ui.t.s(10.0);
        let th = ui.c.measure_text("Xg", f).1;
        let ty = top + (h - th) / 2 - ui.t.s(1.0);
        let max_w = ui.w - pad * 2;
        if fv.text.is_empty() {
            let ph = match (dns, i) {
                (false, _) => t("power.ping_placeholder"),
                (true, 0) => t("power.dns_placeholder"),
                (true, _) => t("power.dns_placeholder2"),
            };
            ui.c.draw_text_clipped(&ph, pad, ty, max_w, f, ui.t.dim.alpha(0.6));
        } else {
            let tw = ui.c.measure_text(&fv.text, f).0.min(max_w);
            if fv.all && focused {
                ui.c.fill_round_rect(pad - ui.t.s(2.0), ty, tw + ui.t.s(4.0), th, ui.t.s(2.0) as f32, ui.t.accent.alpha(0.38));
            }
            ui.c.draw_text_clipped(&fv.text, pad, ty, max_w, f, ui.t.text);
        }
        if focused && e.caret_on && !fv.all {
            let cx = pad + ui.c.measure_text(&fv.before, f).0.min(max_w);
            ui.c.fill_rect(cx, ty + ui.t.s(1.0), ui.t.s(1.0).max(1), th - ui.t.s(2.0), ui.t.text);
        }
        // a click on a field puts the keyboard there and keeps the edit going
        ui.y = top + h;
        ui.clickable(top, Action::EditFocus(i as u8));
        ui.y = top + h + ui.t.s(5.0);
    }
    let fh = ui.font(9.0, FW_NORMAL);
    let (hint, col) = match (e.error, dns) {
        (true, false) => (t("power.ping_invalid"), RED),
        (true, true) => (t("power.dns_invalid"), RED),
        (false, false) => (t("power.ping_hint"), ui.t.dim),
        (false, true) => (t("power.dns_hint"), ui.t.dim),
    };
    battery_view::wrap(ui, &hint, fh, col);
    ui.y += ui.t.s(4.0);
}

fn draw_disk(ui: &mut Ui, m: &MetricsData) {
    let line = match m.disks.len() {
        0 => String::new(),
        1 => m.disks[0].model.clone(),
        n => tf("popup.physical_disks_n", &[("n", &n.to_string())]),
    };
    let key = if m.disks.len() > 1 { "popup.disks" } else { "popup.disk" };
    ui.title(&head(m, key), &sub(m, &line));
    let r: Vec<f32> = m.disk_history.iter().map(|v| v.0).collect();
    let w: Vec<f32> = m.disk_history.iter().map(|v| v.1).collect();
    let max = ui.chart2(&r, &w, 70.0, READ, WRITE, 1024.0 * 1024.0);
    let (read, write) = (t("popup.read"), t("popup.write"));
    let scaled = |v: &[f32]| -> Vec<f32> { v.iter().map(|x| x / max).collect() };
    let (nr, nw) = (scaled(&r), scaled(&w));
    ui.probe(&[(&nr, READ), (&nw, WRITE)], |at| {
        let mut parts = Vec::new();
        if let Some(i) = at(r.len()) {
            parts.push(format!("{read}: {}", format_bytes_rate(r[i] as f64)));
        }
        if let Some(i) = at(w.len()) {
            parts.push(format!("{write}: {}", format_bytes_rate(w[i] as f64)));
        }
        parts.join(" \u{00B7} ")
    });
    ui.legend(&[(&read, READ), (&write, WRITE), (&tf("popup.scale", &[("rate", &format_bytes_rate(max as f64))]), ui.t.dim.alpha(0.0))]);
    ui.row_colored(&read, &format_bytes_rate(m.disk_read), READ);
    ui.row_colored(&write, &format_bytes_rate(m.disk_write), WRITE);
    if !m.disks.is_empty() {
        ui.section(&t("popup.physical_disks"));
        for d in &m.disks {
            // "NVMe · 1 TB · 38 °C"; rates, activity, SSD / TRIM, serial and firmware open with a click
            // on the name
            let mut parts: Vec<String> = Vec::new();
            if !d.bus.is_empty() {
                parts.push(d.bus.to_string());
            }
            if d.size > 0 {
                parts.push(format_bytes(d.size));
            }
            if let Some(c) = d.temp {
                parts.push(fmt_temp(c, ui.t.temp_f));
            }
            let desc = parts.join(" · ");
            let start = ui.y;
            let (lc, vc) = (ui.on_off(ui.t.text, d.enabled), ui.on_off(ui.t.dim, d.enabled));
            let key = format!("disk:{}", d.model);
            let open = ui.expanded.contains(&key);
            ui.row_lv(&d.model, &desc, lc, vc);
            // the name itself opens the details (the rest of the block switches the disk)
            let f = ui.font(11.0, FW_NORMAL);
            let name_w = ui.c.measure_text(&d.model, f).0.min(ui.w / 2);
            ui.hits.push(Hit { x: 0, y: start, w: name_w + ui.t.s(4.0), h: ui.y - start, action: Action::Expand(key) });
            if open {
                // the disk's own rates and activity
                let rw = format!("{} / {}", format_bytes_rate(d.read_bps), format_bytes_rate(d.write_bps));
                ui.row_lv(&format!("    {}", t("popup.read_write")), &rw, vc, ui.on_off(ui.t.text, d.enabled));
                if let Some(a) = d.active {
                    // "12% · 0.4 ms · 150 IOPS"
                    let mut text = format!("{} · {:.1} {} · {} IOPS", pct(a), d.latency_ms.unwrap_or(0.0), unit("ms"), grouped(d.iops.unwrap_or(0.0).round() as u64));
                    if let Some(q) = d.queue.filter(|q| *q >= 1.0) {
                        text.push_str(&format!(" · {}", tf("popup.queue", &[("n", &format!("{q:.0}"))])));
                    }
                    ui.row_lv(&format!("    {}", t("popup.activity")), &text, vc, ui.on_off(ui.t.text, d.enabled));
                }
                // SSD / HDD and TRIM, serial (copies on click), firmware
                let mut segs: Vec<(String, Option<String>)> = Vec::new();
                match d.ssd {
                    Some(true) => segs.push((if d.trim == Some(true) { "SSD (TRIM)".to_string() } else { "SSD".to_string() }, None)),
                    Some(false) => segs.push(("HDD".to_string(), None)),
                    None => {}
                }
                if !d.serial.is_empty() {
                    segs.push((format!("S/N {}", d.serial), Some(d.serial.clone())));
                }
                if !d.firmware.is_empty() {
                    segs.push((format!("FW {}", d.firmware), None));
                }
                if !segs.is_empty() {
                    ui.info_row(&segs, " · ", ui.on_off(ui.t.dim, d.enabled));
                }
            }
            ui.clickable(start, Action::ToggleDisk(d.model.clone()));
        }
    }
    ui.proc_section(&t("popup.top_processes"), m.host.is_empty());
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

#[cfg(test)]
mod tests {
    use super::{ago, arrange, drop_slot, fmt_delta, fmt_temp, held, merge_order, series_index};

    #[test]
    fn sample_ago() {
        assert_eq!(ago(0.2), "00:00");
        assert_eq!(ago(82.4), "01:22");
        assert_eq!(ago(3909.0), "1:05:09");
    }
    use std::collections::VecDeque;

    #[test]
    fn held_chart_window() {
        let snap: VecDeque<u32> = (1..=5).collect();
        // three samples came while held; the live history dropped its oldest ones (full)
        let live: VecDeque<u32> = (4..=8).collect();
        // all three behind: the held picture; none behind: the live one; in between it scrolls
        assert_eq!(held(&snap, &live, 3, 3), snap);
        assert_eq!(held(&snap, &live, 3, 0), live);
        assert_eq!(held(&snap, &live, 3, 1), (3..=7).collect::<VecDeque<u32>>());
        // nothing held: live as it is
        assert_eq!(held(&VecDeque::new(), &live, 3, 3), live);
    }

    #[test]
    fn chart_probe_index() {
        // 3 samples stretched over 101 px: the ends and the middle
        assert_eq!(series_index(3, 101, 0), Some(0));
        assert_eq!(series_index(3, 101, 50), Some(1));
        assert_eq!(series_index(3, 101, 100), Some(2));
        // 300 samples in 100 px: the newest 100, one per column
        assert_eq!(series_index(300, 100, 0), Some(200));
        assert_eq!(series_index(300, 100, 99), Some(299));
        assert_eq!(series_index(0, 100, 5), None);
        assert_eq!(series_index(1, 100, 70), Some(0));
    }

    fn v(k: &[&str]) -> Vec<String> {
        k.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fold_order() {
        let keys = ["load", "clock", "fan", "fan", "idle", "uptime"];
        // nothing stored: the default order
        assert_eq!(arrange(&keys, &[]), vec![0, 1, 2, 3, 4, 5]);
        // uptime dragged to the top; the rest keep their default order after their anchors
        assert_eq!(arrange(&keys, &v(&["uptime", "load", "clock"])), vec![5, 0, 1, 2, 3, 4]);
        // the fans move together, the unlisted rows after them stay with them
        assert_eq!(arrange(&keys, &v(&["fan", "load"])), vec![2, 3, 4, 5, 0, 1]);
        // a row the stored order does not know follows the row before it by default
        assert_eq!(arrange(&["a", "new", "b"], &v(&["b", "a"])), vec![2, 0, 1]);
        // keys of rows not shown now and of the other popup keep their places
        assert_eq!(merge_order(&v(&["gpu.a", "x", "limit", "y"]), &v(&["y", "x"])), v(&["gpu.a", "y", "x", "limit"]));
        assert_eq!(merge_order(&[], &v(&["b", "a"])), v(&["b", "a"]));
        // rows at y 0, 10, 20 (height 10): dragging "a" below the middle of "c" puts it last
        let rows = vec![("a".to_string(), 0, 10), ("b".to_string(), 10, 10), ("c".to_string(), 20, 10)];
        assert_eq!(drop_slot(&rows, "a", 27), (2, 30));
        assert_eq!(drop_slot(&rows, "c", 3), (0, 0));
        assert_eq!(drop_slot(&rows, "c", 12), (1, 10));
    }

    #[test]
    fn fahrenheit_conversion_boundaries() {
        assert_eq!(fmt_temp(0.0, true), "32 \u{00B0}F");
        assert_eq!(fmt_temp(100.0, true), "212 \u{00B0}F");
        assert_eq!(fmt_temp(-40.0, true), "-40 \u{00B0}F"); // the scales meet here
        assert_eq!(fmt_temp(37.4, true), "99 \u{00B0}F"); // rounds like the Celsius display
        assert_eq!(fmt_temp(37.6, true), "100 \u{00B0}F");
    }

    #[test]
    fn celsius_display_keeps_the_unit_letter() {
        assert_eq!(fmt_temp(38.4, false), "38 \u{00B0}C");
    }

    #[test]
    fn guidance_differences_scale_without_the_offset() {
        // differences take 9/5 only: an offset would show 32-41 here, not 0-9
        assert_eq!(fmt_delta(0.0, 5.0, true), "0-9 \u{00B0}F");
        assert_eq!(fmt_delta(5.0, 15.0, true), "9-27 \u{00B0}F");
        assert_eq!(fmt_delta(20.0, 30.0, true), "36-54 \u{00B0}F");
        assert_eq!(fmt_delta(5.0, 15.0, false), "5-15 \u{00B0}C");
    }
}
