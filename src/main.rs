#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod canvas;
mod config;
mod i18n;
mod menu;
mod metrics;
mod about;
mod popup;
mod remote;
mod share;
mod taskbar;
mod update;
mod util;
mod widgets;

use canvas::{BLACK, Canvas, Color, WHITE};
use config::Config;
use metrics::Metrics;
use std::cell::RefCell;
use about::About;
use popup::Popup;
use taskbar::Taskbar;
use util::{pcwstr, wide};
use i18n::t;
use widgets::{ColorMode, Kind, Theme, Widget};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

const TIMER_ID: usize = 1;
/// floating mode: re-assert the strip above the taskbar (Explorer raises the taskbar over other
/// topmost windows whenever it is activated, e.g. when the Start menu opens)
const TIMER_ZORDER: usize = 2;
/// floating mode: short burst of z-order checks (every 16 ms) right after a shell event, because
/// Explorer raises the taskbar a few milliseconds *after* the events it emits
const TIMER_ZBURST: usize = 3;
const ZBURST_TICKS: u32 = 14;
/// one-shot: quiet update check a while after start
const TIMER_UPDATE: usize = 4;
const MENU_EXIT: usize = 1;
const MENU_CONFIG: usize = 2;
const MENU_RELOAD: usize = 3;
const MENU_TASKMGR: usize = 4;
const MENU_ABOUT: usize = 5;
const MENU_CHECK_UPDATE: usize = 6;
const MENU_GET_UPDATE: usize = 7;

struct App {
    cfg: Config,
    hidden: HWND,
    bar: HWND,
    taskbar: Option<Taskbar>,
    metrics: Metrics,
    canvas: Canvas,
    widgets: Vec<Box<dyn Widget>>,
    /// (widget index, kind, x0, x1) of each visible widget in bar client coords, for hit-testing
    layout: Vec<(usize, Kind, i32, i32)>,
    /// mouse-drag reordering state
    drag: Option<Drag>,
    /// floating mode: the strip is a top-level window over the taskbar instead of a child
    floating: bool,
    /// floating mode: remaining burst checks
    zburst: u32,
    popup: Popup,
    about: About,
    /// newer release found by the update check, shown in the menu
    update_available: Option<String>,
    remotes: remote::Remotes,
    /// this machine served to other winstats (Remote > Share this machine)
    share: Option<share::Share>,
    /// agents listed in the last Remote submenu, in id order
    menu_remote: Vec<RemoteEntry>,
    /// last logged layout (widget kinds and widths), to log only changes
    layout_sig: String,
    /// floating mode: the strip is hidden while a full-screen game or presentation runs
    fullscreen_hidden: bool,
    wm_taskbar_created: u32,
    ticks: u64,
}

/// One line of the Remote submenu: a configured machine, or one only heard on the network.
#[derive(Clone, Debug)]
struct RemoteEntry {
    /// config key when configured
    key: Option<String>,
    id: String,
    name: String,
    os: String,
    arch: String,
    address: String,
    enabled: bool,
    /// "online", "offline (...)", or "found" for an unconfigured one
    state: String,
    /// cpu / mem from the beacon, when heard
    load: Option<(f32, f32)>,
    /// modules the agent reports
    has: Vec<String>,
    /// cores of two classes (P / E): grouping applies
    hybrid: bool,
    /// the core chart has dividers (P / E, clusters or sockets): the splitter option applies
    splittable: bool,
}

fn remote_keys(cfg: &Config) -> Vec<String> {
    cfg.remotes.keys().cloned().collect()
}

/// Theme for a remote machine's widgets: its own colour modes and its label.
fn remote_theme(cfg: &Config, base: &Theme, key: &str) -> Theme {
    let mut t = base.clone();
    if let Some(rc) = cfg.remotes.get(key) {
        for (i, k) in widgets::MODULES.iter().enumerate() {
            t.modes[i] = match rc.colors.get(module_key(*k)) {
                Some(v) => ColorMode::parse(v),
                None => ColorMode::Accent,
            };
        }
        t.label = Some(if rc.label.is_empty() { key.to_string() } else { rc.label.clone() });
        t.labels = rc.labels;
        t.split_pe = rc.split_pe;
    } else {
        t.label = Some(key.to_string());
    }
    t
}

/// A full-screen Direct3D game or a presentation is in front: the shell says so, or the
/// foreground window (not ours, not the desktop) covers its whole monitor, taskbar included.
fn fullscreen_active(own: HWND) -> bool {
    unsafe {
        if let Ok(s) = SHQueryUserNotificationState() {
            if s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE {
                return true;
            }
        }
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == own {
            return false;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(fg, &mut cls) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]);
        if matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "WinStatsPopup" | "WinStatsAbout") {
            return false;
        }
        let mut rc = RECT::default();
        if GetWindowRect(fg, &mut rc).is_err() {
            return false;
        }
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let hm = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if !GetMonitorInfoW(hm, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        rc.left <= m.left && rc.top <= m.top && rc.right >= m.right && rc.bottom >= m.bottom
    }
}

/// The machine's theme, or all grey when it has stopped answering.
fn agent_theme(cfg: &Config, base: &Theme, key: &str, stale: bool) -> Theme {
    let mut t = remote_theme(cfg, base, key);
    if stale {
        let grey = base.dim.alpha(0.6);
        t.modes = [ColorMode::Fixed(grey); 6];
        t.accent = grey;
        t.text = grey;
    }
    t
}

/// A widget being dragged along the strip.
struct Drag {
    /// index into `widgets` / `cfg.widgets`
    widget: usize,
    start_x: i32,
    cur_x: i32,
    /// true once the pointer moved far enough to count as a drag rather than a click
    active: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn main() {
    let first_run = !Config::path().exists();
    let mut cfg = Config::load();
    let keys = remote_keys(&cfg);
    widgets::normalize(&mut cfg.widgets, &keys);
    i18n::set_language(&cfg.language);
    let floating = match cfg.mode.as_str() {
        "floating" => true,
        "embedded" => false,
        _ => util::is_windows7(),
    };
    unsafe {
        // per-monitor DPI where Windows offers it (10 1607+), system DPI awareness otherwise
        if floating || !util::set_per_monitor_dpi() {
            let _ = SetProcessDPIAware();
        }
        // single instance
        let name = wide("Local\\winstats-single-instance");
        let _mutex = CreateMutexW(None, false, pcwstr(&name));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            log!("already running");
            return;
        }
    }

    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None).expect("module handle").into() };

    unsafe {
        register_class(hinstance, "WinStatsHidden", Some(hidden_wndproc));
        register_class(hinstance, "WinStatsBar", Some(bar_wndproc));
        register_class(hinstance, "WinStatsPopup", Some(popup_wndproc));
        register_class(hinstance, "WinStatsAbout", Some(about_wndproc));
    }

    let hidden = unsafe {
        let cls = wide("WinStatsHidden");
        let title = wide("winstats");
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            pcwstr(&cls),
            pcwstr(&title),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        )
        .expect("hidden window")
    };

    let wm_taskbar_created = unsafe {
        let s = wide("TaskbarCreated");
        let m = RegisterWindowMessageW(pcwstr(&s));
        // let the broadcast through even if we run elevated
        let _ = ChangeWindowMessageFilterEx(hidden, m, MSGFLT_ALLOW, None);
        m
    };

    let mut metrics = Metrics::new(&cfg.disk, cfg.top_processes as usize, &cfg.net_disabled, &cfg.disks_disabled);
    metrics.set_group_pe(cfg.group_pe_cores);
    if first_run {
        // do not start with widgets this machine cannot feed
        let before = cfg.widgets.len();
        if !metrics.gpu_available {
            cfg.widgets.retain(|w| widgets::CATALOG.iter().all(|(k, n, _)| *k != Kind::Gpu || *n != w));
        }
        if !metrics.battery_available {
            cfg.widgets.retain(|w| widgets::CATALOG.iter().all(|(k, n, _)| *k != Kind::Battery || *n != w));
        }
        if cfg.widgets.len() != before {
            cfg.save();
        }
    }
    let widgets = widgets::build(&cfg.widgets);
    let popup = Popup::new(hinstance, "WinStatsPopup").expect("popup window");
    let about = About::new(hinstance, "WinStatsAbout").expect("about window");
    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            cfg: cfg.clone(),
            hidden,
            bar: HWND::default(),
            taskbar: None,
            metrics,
            canvas: Canvas::new(),
            widgets,
            layout: Vec::new(),
            drag: None,
            floating,
            zburst: 0,
            popup,
            about,
            update_available: None,
            remotes: remote::Remotes::new(),
            share: None,
            menu_remote: Vec::new(),
            layout_sig: String::new(),
            fullscreen_hidden: false,
            wm_taskbar_created,
            ticks: 0,
        })
    });

    log!("winstats started (build {}, win11={}, floating={})", util::windows_build(), util::is_windows11(), floating);
    with_app(|app| {
        let on = app.cfg.discovery;
        app.remotes.set_discovery(on);
        app.apply_share();
        app.tick();
    });
    unsafe {
        SetTimer(hidden, TIMER_ID, cfg.update_ms.max(250), None);
        SetTimer(hidden, TIMER_UPDATE, 20_000, None);
        if floating {
            SetTimer(hidden, TIMER_ZORDER, 150, None);
            // react immediately when the taskbar is raised (foreground change or any top-level
            // z-order change); the timer above is only a fallback
            use windows::Win32::UI::Accessibility::SetWinEventHook;
            // system events 1..=9 cover foreground, menu popups and mouse capture; the object
            // events cover show / reorder / focus / location changes
            let _ = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_CAPTUREEND, None, Some(shell_event_proc), 0, 0, WINEVENT_OUTOFCONTEXT);
            let _ = SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_LOCATIONCHANGE, None, Some(shell_event_proc), 0, 0, WINEVENT_OUTOFCONTEXT);
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().take() {
            app.shutdown();
        }
    });
}

unsafe fn register_class(hinstance: HINSTANCE, name: &str, proc: WNDPROC) {
    let cls = wide(name);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: proc,
        hInstance: hinstance,
        lpszClassName: pcwstr(&cls),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        ..Default::default()
    };
    // keep the class name alive for the process lifetime
    std::mem::forget(cls);
    unsafe { RegisterClassExW(&wc) };
}

impl App {
    fn ensure_bar(&mut self) -> bool {
        if self.taskbar.as_ref().map(|t| t.alive()).unwrap_or(false) == false {
            self.taskbar = taskbar::find(self.floating);
            match &self.taskbar {
                Some(t) => log!(
                    "taskbar found: tray={:?} notify={:?} rebar={:?} client={:?} dpi={}",
                    t.tray,
                    t.notify,
                    t.rebar,
                    t.client(),
                    t.dpi()
                ),
                None => {
                    return false;
                }
            }
        }
        let alive = unsafe { IsWindow(self.bar).as_bool() };
        // floating: the strip is a top-level window over the taskbar, there is no parent to check
        let parent_ok = alive && (self.floating || unsafe { GetParent(self.bar).ok() } == self.taskbar.as_ref().map(|t| t.tray));
        if !alive || !parent_ok {
            if alive {
                unsafe {
                    let _ = DestroyWindow(self.bar);
                }
            }
            self.canvas = Canvas::new();
            let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap().into() };
            let cls = wide("WinStatsBar");
            let title = wide("winstats bar");
            let bar = unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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
            };
            let Ok(bar) = bar else {
                log!("CreateWindowExW(bar) failed: {:?}", bar.err());
                return false;
            };
            self.bar = bar;
            let tb = self.taskbar.as_ref().unwrap();
            if !tb.attach(bar) {
                return false;
            }
            log!("bar window {:?} attached to taskbar", bar);
        }
        true
    }

    fn is_light(&self) -> bool {
        match self.cfg.theme.as_str() {
            "light" => true,
            "dark" => false,
            _ => util::system_light_theme(),
        }
    }

    fn theme(&self) -> Theme {
        let tb = self.taskbar.as_ref().unwrap();
        let light = self.is_light();
        let scale = tb.dpi() as f32 / 96.0;
        let h = tb.client().bottom - tb.client().top;
        let (text, dim) = if light {
            (Color::rgb(0x1B, 0x1B, 0x1B), Color::rgba(0x1B, 0x1B, 0x1B, 160))
        } else {
            (WHITE, Color::rgba(255, 255, 255, 150))
        };
        let _ = BLACK;
        let mut modes = [ColorMode::Accent; 6];
        for (i, k) in widgets::MODULES.iter().enumerate() {
            modes[i] = match self.cfg.colors.get(module_key(*k)) {
                Some(v) => ColorMode::parse(v),
                None if self.cfg.utilization_colors && *k == Kind::Cpu => ColorMode::Utilization,
                None => ColorMode::Accent,
            };
        }
        Theme {
            text,
            dim,
            accent: self.cfg.accent_color(),
            light,
            scale,
            inner_h: ((h as f32) * 0.55).round() as i32,
            labels: self.cfg.labels,
            font_scale: if self.cfg.font_scale > 0.5 { self.cfg.font_scale } else { 1.0 },
            split_pe: self.cfg.split_pe_cores,
            chart_w: self.cfg.chart_width.clamp(20, 200) as f32,
            bar_w: widgets::bar_width_dip(&self.cfg.bar_width),
            modes,
            label: None,
        }
    }

    /// Start or stop serving this machine to follow `cfg.share`.
    fn apply_share(&mut self) {
        if self.cfg.share {
            if self.cfg.share_id.is_empty() {
                self.cfg.share_id = share::new_id();
                self.cfg.save();
            }
            if self.share.is_none() {
                self.share = share::Share::start(&self.cfg, &self.metrics);
            }
        } else {
            self.share = None;
        }
        self.metrics.want_per_disk = self.share.is_some();
    }

    /// Draw the popup from whichever machine it shows.
    fn render_popup(&mut self, first: bool) {
        let light = self.is_light();
        let theme = self.theme();
        match self.popup.source.clone() {
            None => self.popup.render(&theme, light, &self.metrics, first),
            Some(key) => {
                let t = remote_theme(&self.cfg, &theme, &key);
                if let Some(a) = self.remotes.agents.get(&key) {
                    self.popup.render(&t, light, &a.data, first);
                }
            }
        }
    }

    fn tick(&mut self) {
        self.ticks += 1;
        if !self.ensure_bar() {
            return;
        }
        // floating strip over a full-screen Direct3D game would flicker through it: hide it
        // until the game is gone (the child strip on Windows 10/11 is covered anyway)
        if self.floating {
            let fs = fullscreen_active(self.bar);
            if fs != self.fullscreen_hidden {
                self.fullscreen_hidden = fs;
                unsafe {
                    let _ = ShowWindow(self.bar, if fs { SW_HIDE } else { SW_SHOWNA });
                }
                if fs {
                    self.popup.hide("fullscreen");
                    log!("full-screen application in front: strip hidden");
                } else {
                    log!("full-screen application gone: strip back");
                    self.raise_bar();
                }
            }
        }
        let share_details = self.share.as_ref().map_or(false, |s| s.details_wanted());
        self.metrics.update((self.popup.visible && self.popup.source.is_none()) || share_details);
        if let Some(s) = self.share.as_mut() {
            s.publish(&self.cfg, &self.metrics);
        }
        if self.remotes.poll(&mut self.cfg) {
            self.cfg.save();
        }
        if self.popup.visible {
            if let Some(key) = self.popup.source.clone() {
                self.remotes.request_details(&key, self.cfg.top_processes as usize);
            }
        }
        if !self.fullscreen_hidden {
            self.render();
        }
    }

    /// Layout, draw and present the strip (and the popup) from the current metrics.
    fn render(&mut self) {
        if self.taskbar.is_none() {
            return;
        }
        let theme = self.theme();
        let tb = self.taskbar.as_ref().unwrap();
        let client = tb.client();
        let h = client.bottom - client.top;
        let gap = theme.s(6.0);
        let edge = theme.s(6.0);

        let mut widths: Vec<i32> = Vec::with_capacity(self.widgets.len());
        for w in &self.widgets {
            widths.push(match w.source() {
                None => w.width(&mut self.canvas, &theme, &self.metrics),
                // a remote machine that stopped answering fades to grey, then takes no room;
                // its place in the order is kept
                Some(key) => match self.remotes.agents.get(key) {
                    Some(a) if a.visible => w.width(&mut self.canvas, &agent_theme(&self.cfg, &theme, key, a.stale), &a.data),
                    _ => 0,
                },
            });
        }
        let visible: Vec<(usize, i32)> = widths.iter().cloned().enumerate().filter(|(_, w)| *w > 0).collect();
        let total = edge * 2 + visible.iter().map(|(_, w)| *w).sum::<i32>() + gap * (visible.len().saturating_sub(1)) as i32;

        self.canvas.resize(total.max(1), h.max(1));
        self.canvas.clear();
        // alpha 0 pixels are transparent to hit-testing on layered windows: keep the strip clickable
        self.canvas.fill_rect(0, 0, total, h, Color::rgba(0, 0, 0, 1));
        let y = (h - theme.inner_h) / 2;
        let mut x = edge;
        self.layout.clear();
        // one line per layout change so tests and bug reports can find the widgets
        let sig: Vec<String> = visible.iter().map(|(i, w)| format!("{}{:?}@{}", self.widgets[*i].source().map(|s| format!("{s}/")).unwrap_or_default(), self.widgets[*i].kind(), w)).collect();
        let sig = sig.join(" ");
        if sig != self.layout_sig {
            self.layout_sig = sig.clone();
            log!("layout: {sig}");
        }
        for (i, w) in &visible {
            match self.widgets[*i].source() {
                None => self.widgets[*i].draw(&mut self.canvas, x, y, &theme, &self.metrics),
                Some(key) => {
                    if let Some(a) = self.remotes.agents.get(key) {
                        self.widgets[*i].draw(&mut self.canvas, x, y, &agent_theme(&self.cfg, &theme, key, a.stale), &a.data);
                    }
                }
            }
            self.layout.push((*i, self.widgets[*i].kind(), x - gap / 2, x + w + gap / 2));
            x += w + gap;
        }
        if let Some(d) = self.drag.as_ref().filter(|d| d.active) {
            // dim the dragged widget and mark the insertion slot nearest to the pointer
            if let Some((_, _, x0, x1)) = self.layout.iter().find(|(i, _, _, _)| *i == d.widget) {
                let bg = if self.is_light() { Color::rgba(255, 255, 255, 140) } else { Color::rgba(0, 0, 0, 140) };
                self.canvas.fill_rect(*x0 + gap / 2, y, x1 - x0 - gap, theme.inner_h, bg);
            }
            let slot_x = self.slot_x(self.drop_slot(d.cur_x), gap, edge, total);
            self.canvas.fill_rect(slot_x - 1, y - theme.s(2.0), theme.s(2.0), theme.inner_h + theme.s(4.0), theme.accent);
        }

        let offset = theme.s(self.cfg.offset as f32);
        let rc = tb.place(self.bar, total, &self.cfg.position, offset);
        if self.ticks == 1 {
            log!("placed bar at {:?}", rc);
        }
        if let Err(e) = self.canvas.present(self.bar) {
            if self.ticks % 30 == 1 {
                log!("UpdateLayeredWindow failed: {e}");
            }
        }
        if self.popup.visible {
            self.render_popup(false);
        }
    }

    /// Left click on the strip at bar-client x.
    fn click(&mut self, x: i32) {
        let Some((wi, kind, x0, x1)) = self.layout.iter().find(|(_, _, a, b)| x >= *a && x < *b).cloned() else { return };
        let source = self.widgets.get(wi).and_then(|w| w.source().map(|s| s.to_string()));
        let Some(tb) = self.taskbar.as_ref() else { return };
        let mut bar_rc = RECT::default();
        let mut tray_rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.bar, &mut bar_rc);
            let _ = GetWindowRect(tb.tray, &mut tray_rc);
        }
        let anchor = RECT { left: bar_rc.left + x0, top: bar_rc.top, right: bar_rc.left + x1, bottom: bar_rc.bottom };
        if self.popup.toggle(kind, source.clone(), anchor, tray_rc) {
            match source {
                None => self.metrics.begin_details(),
                Some(key) => self.remotes.request_details(&key, self.cfg.top_processes as usize),
            }
            self.render_popup(true);
        }
    }

    /// Insertion slot (0..=visible count) whose boundary is nearest to bar-client x.
    fn drop_slot(&self, x: i32) -> usize {
        let mut best = 0usize;
        let mut best_d = i32::MAX;
        for (k, (_, _, x0, _)) in self.layout.iter().enumerate() {
            let d = (x - x0).abs();
            if d < best_d {
                best_d = d;
                best = k;
            }
        }
        if let Some((_, _, _, x1)) = self.layout.last() {
            if (x - x1).abs() < best_d {
                best = self.layout.len();
            }
        }
        best
    }

    /// x of a slot boundary in bar-client coords.
    fn slot_x(&self, slot: usize, _gap: i32, _edge: i32, total: i32) -> i32 {
        match self.layout.get(slot) {
            // x0 already sits in the middle of the gap before the widget
            Some((_, _, x0, _)) => (*x0).max(1),
            None => self.layout.last().map(|(_, _, _, x1)| *x1).unwrap_or(total).min(total - 2),
        }
    }

    fn drag_start(&mut self, x: i32) {
        let Some((i, _, _, _)) = self.layout.iter().find(|(_, _, a, b)| x >= *a && x < *b).cloned() else { return };
        self.drag = Some(Drag { widget: i, start_x: x, cur_x: x, active: false });
        unsafe {
            SetCapture(self.bar);
        }
    }

    fn drag_move(&mut self, x: i32) {
        let threshold = self.theme().s(6.0);
        let Some(d) = self.drag.as_mut() else { return };
        d.cur_x = x;
        if !d.active && (x - d.start_x).abs() >= threshold {
            d.active = true;
            self.popup.hide("drag");
        }
        if d.active {
            self.render();
        }
    }

    /// Button released: finish a drag (reorder) or treat it as a click.
    fn drag_end(&mut self, x: i32) {
        unsafe {
            let _ = ReleaseCapture();
        }
        let Some(d) = self.drag.take() else { return };
        if !d.active {
            self.click(x);
            return;
        }
        let slot = self.drop_slot(x);
        let from = d.widget;
        let to = match self.layout.get(slot) {
            Some((i, _, _, _)) => *i,
            None => self.widgets.len(),
        };
        if to != from && to != from + 1 && from < self.cfg.widgets.len() {
            let name = self.cfg.widgets.remove(from);
            let insert_at = if to > from { to - 1 } else { to };
            self.cfg.widgets.insert(insert_at.min(self.cfg.widgets.len()), name);
            self.cfg.save();
            self.widgets = widgets::build(&self.cfg.widgets);
        }
        self.render();
    }

    /// floating mode: a shell event happened; check the z-order now and keep checking every 16 ms
    /// for a short while, since the taskbar is raised slightly after the event.
    fn shell_event(&mut self) {
        self.raise_bar();
        if self.zburst == 0 {
            unsafe {
                SetTimer(self.hidden, TIMER_ZBURST, 16, None);
            }
        }
        self.zburst = ZBURST_TICKS;
    }

    /// floating mode: put the strip back on top, but only when the taskbar actually covers it
    /// (an unconditional SetWindowPos would generate reorder events and feed the hook forever).
    fn raise_bar(&mut self) {
        if !self.floating || self.drag.is_some() || self.fullscreen_hidden {
            return;
        }
        let Some(tb) = self.taskbar.as_ref() else { return };
        unsafe {
            if IsWindow(self.bar).as_bool() && tb.covers(self.bar) {
                let _ = SetWindowPos(self.bar, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    fn reload(&mut self) {
        self.cfg = Config::load();
        let keys = remote_keys(&self.cfg);
        widgets::normalize(&mut self.cfg.widgets, &keys);
        let on = self.cfg.discovery;
        self.remotes.set_discovery(on);
        self.share = None;
        i18n::set_language(&self.cfg.language);
        self.widgets = widgets::build(&self.cfg.widgets);
        self.metrics = Metrics::new(&self.cfg.disk, self.cfg.top_processes as usize, &self.cfg.net_disabled, &self.cfg.disks_disabled);
        self.metrics.set_group_pe(self.cfg.group_pe_cores);
        self.apply_share();
        unsafe {
            SetTimer(self.hidden, TIMER_ID, self.cfg.update_ms.max(250), None);
        }
        log!("config reloaded");
    }

    fn shutdown(self) {
        unsafe {
            let _ = DestroyWindow(self.popup.hwnd);
            let _ = DestroyWindow(self.about.hwnd);
            if IsWindow(self.bar).as_bool() {
                let _ = DestroyWindow(self.bar);
            }
        }
        if let Some(tb) = &self.taskbar {
            tb.restore();
        }
    }
}

unsafe extern "system" fn shell_event_proc(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // object events for child objects (buttons, list items, the caret...) are noise
    if event >= EVENT_OBJECT_SHOW && id_object != OBJID_WINDOW.0 {
        return;
    }
    with_app(|app| app.shell_event());
}

unsafe extern "system" fn hidden_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            if wparam.0 == TIMER_UPDATE {
                unsafe {
                    let _ = KillTimer(hwnd, TIMER_UPDATE);
                }
                update::check_async(hwnd, false);
                return LRESULT(0);
            }
            if wparam.0 == TIMER_ZORDER {
                with_app(|app| app.raise_bar());
                return LRESULT(0);
            }
            if wparam.0 == TIMER_ZBURST {
                with_app(|app| {
                    app.raise_bar();
                    app.zburst = app.zburst.saturating_sub(1);
                    if app.zburst == 0 {
                        unsafe {
                            let _ = KillTimer(app.hidden, TIMER_ZBURST);
                        }
                    }
                });
                return LRESULT(0);
            }
            with_app(|app| app.tick());
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        update::WM_APP_UPDATE => {
            update_result(hwnd, wparam.0 == 1);
            LRESULT(0)
        }
        _ => {
            let created = with_app(|app| app.wm_taskbar_created).unwrap_or(0);
            if created != 0 && msg == created {
                log!("TaskbarCreated: explorer restarted, re-attaching");
                with_app(|app| {
                    app.taskbar = None;
                    app.bar = HWND::default();
                    app.tick();
                });
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

unsafe extern "system" fn bar_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_RBUTTONUP => {
            show_menu();
            LRESULT(0)
        }
        // Do not let Explorer activate the taskbar (it would eat the click while our popup is foreground).
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            with_app(|app| app.drag_start(x));
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if wparam.0 & 0x0001 != 0 {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                with_app(|app| app.drag_move(x));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            with_app(|app| app.drag_end(x));
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            with_app(|app| {
                app.drag = None;
                app.render();
            });
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            open_taskmgr();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ACTIVATE => {
            if wparam.0 & 0xFFFF == 0 {
                with_app(|app| app.about.hide());
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 == 0x1B {
                with_app(|app| app.about.hide());
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            with_app(|app| app.about.click(x, y));
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // hand over links, arrow elsewhere
            let mut pt = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
            }
            let link = with_app(|app| app.about.is_link_at(pt.x, pt.y)).unwrap_or(false);
            unsafe {
                let _ = SetCursor(LoadCursorW(None, if link { IDC_HAND } else { IDC_ARROW }).unwrap_or_default());
            }
            LRESULT(1)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn popup_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            with_app(|app| {
                if let Some(action) = app.popup.click(x, y) {
                    app.popup_action(action);
                }
            });
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // hand over the rows that react to a click
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(hwnd, &mut pt);
            let clickable = with_app(|app| app.popup.visible && app.popup.click_target(pt.x, pt.y)).unwrap_or(false);
            if clickable {
                if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(c);
                }
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            with_app(|app| {
                if app.popup.visible && app.popup.mouse_move(x, y) {
                    app.render_popup(false);
                }
            });
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if wparam.0 & 0xFFFF == 0 {
                let other = HWND(lparam.0 as *mut _);
                let mut cls = [0u16; 64];
                let n = unsafe { GetClassNameW(other, &mut cls) } as usize;
                let reason = format!("deactivated by {:?} '{}'", other, String::from_utf16_lossy(&cls[..n]));
                with_app(|app| app.popup.hide(&reason));
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 == 0x1B {
                with_app(|app| app.popup.hide("escape"));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn open_taskmgr() {
    let verb = wide("open");
    let file = wide("taskmgr.exe");
    unsafe {
        ShellExecuteW(None, pcwstr(&verb), pcwstr(&file), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

const MENU_WIDGET_BASE: usize = 100;
const MENU_LABELS: usize = 200;
const MENU_AUTOSTART: usize = 202;
const MENU_GROUP_PE: usize = 203;
const MENU_SPLIT_PE: usize = 204;
/// per-module move: MENU_MOVE_BASE + module_index * 2 + (0 = left, 1 = right)
const MENU_MOVE_BASE: usize = 520;
const MENU_POS_RIGHT: usize = 210;
const MENU_POS_LEFT: usize = 211;
const MENU_THEME_AUTO: usize = 220;
const MENU_THEME_DARK: usize = 221;
const MENU_THEME_LIGHT: usize = 222;
const MENU_INTERVAL_BASE: usize = 230;
/// per-module colour: MENU_COLOR_BASE + module_index * 20 + entry
const MENU_COLOR_BASE: usize = 300;
const MENU_COLOR_STRIDE: usize = 20;
/// global accent: MENU_ACCENT_BASE + palette index
const MENU_ACCENT_BASE: usize = 450;
const MENU_FONT_BASE: usize = 470;
const MENU_CHART_BASE: usize = 480;
const MENU_TOP_BASE: usize = 490;
const MENU_BAR_BASE: usize = 500;
/// language: MENU_LANG_BASE = system language, then one entry per locale
const MENU_LANG_BASE: usize = 600;
/// Remote submenu: the discovery toggle, then one block of ids per listed agent
const MENU_REMOTE_DISCOVERY: usize = 700;
const MENU_REMOTE_SHARE: usize = 701;
const MENU_REMOTE_BASE: usize = 2000;
const MENU_REMOTE_STRIDE: usize = 200;
/// items inside an agent's block
const RM_CONNECT: usize = 0;
const RM_FORGET: usize = 1;
const RM_LEFT: usize = 2;
const RM_RIGHT: usize = 3;
const RM_WIDGET: usize = 10;
const RM_COLOR: usize = 40;
const RM_COLOR_STRIDE: usize = 16;
const RM_SPLIT_PE: usize = 4;
const RM_GROUP_PE: usize = 5;
const RM_LABELS: usize = 6;
const TOP_COUNTS: &[u32] = &[5, 10, 15, 20];
const CHART_WIDTHS: &[(u32, &str)] = &[(34, "menu.small"), (48, "menu.medium"), (64, "menu.large"), (96, "menu.extra_large")];
const FONT_SIZES: &[(f32, &str)] = &[(0.85, "menu.small"), (1.0, "menu.normal"), (1.15, "menu.large"), (1.3, "menu.extra_large")];
/// colour submenu entries before the palette
const COLOR_MODES: &[(&str, &str)] = &[("accent", "menu.accent"), ("utilization", "menu.by_utilisation"), ("mono", "menu.monochrome")];

fn module_key(k: Kind) -> &'static str {
    match k {
        Kind::Cpu => "cpu",
        Kind::Gpu => "gpu",
        Kind::Mem => "mem",
        Kind::Disk => "disk",
        Kind::Net => "net",
        Kind::Battery => "battery",
    }
}

/// Colour submenu for one module: modes first, then the palette.
unsafe fn color_submenu(cfg: &Config, mi: usize, kind: Kind, dpi: u32) -> Option<HMENU> {
    unsafe {
        let sub = CreatePopupMenu().ok()?;
        let current = cfg.colors.get(module_key(kind)).map(String::as_str).unwrap_or("accent");
        let base = MENU_COLOR_BASE + mi * MENU_COLOR_STRIDE;
        color_entries(sub, base, current, cfg, dpi);
        Some(sub)
    }
}

/// Body of a colour submenu: the modes (the accent one showing the current accent colour),
/// a separator, then the palette with a swatch per entry.
unsafe fn color_entries(sub: HMENU, base: usize, current: &str, cfg: &Config, dpi: u32) {
    unsafe {
        for (i, (name, title)) in COLOR_MODES.iter().enumerate() {
            let color = if *name == "accent" { palette_color(&cfg.accent, widgets::BLUE) } else { None };
            menu_color_item(sub, base + i, &t(title), current == *name, color, dpi);
        }
        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
        for (i, (name, title, c)) in widgets::PALETTE.iter().enumerate() {
            menu_color_item(sub, base + COLOR_MODES.len() + i, &t(title), current == *name, palette_color(name, *c), dpi);
        }
    }
}
const INTERVALS: &[(u32, &str)] = &[(500, "0.5 s"), (1000, "1 s"), (2000, "2 s"), (5000, "5 s")];

/// Remote submenu: discovery toggle, then one submenu per machine (configured or heard).
unsafe fn remote_menu(cfg: &Config, entries: &[RemoteEntry], dpi: u32) -> Option<HMENU> {
    unsafe {
        let rm = CreatePopupMenu().ok()?;
        menu_item(rm, MENU_REMOTE_DISCOVERY, &t("menu.discovery"), cfg.discovery);
        menu_item(rm, MENU_REMOTE_SHARE, &t("menu.share"), cfg.share);
        let _ = AppendMenuW(rm, MF_SEPARATOR, 0, PCWSTR::null());
        if entries.is_empty() {
            let txt = wide(&t(if cfg.discovery { "menu.no_agents" } else { "menu.discovery_off" }));
            let _ = AppendMenuW(rm, MF_STRING | MF_GRAYED, 0, pcwstr(&txt));
            return Some(rm);
        }
        for (ei, e) in entries.iter().enumerate() {
            let base = MENU_REMOTE_BASE + ei * MENU_REMOTE_STRIDE;
            let sub = CreatePopupMenu().ok()?;
            match &e.key {
                None => menu_item(sub, base + RM_CONNECT, &t("menu.connect"), false),
                Some(key) => {
                    menu_item(sub, base + RM_CONNECT, &t("menu.connected"), e.enabled);
                    let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                    let rc = cfg.remotes.get(key);
                    for (mi, kind) in widgets::MODULES.iter().enumerate() {
                        let feed = match kind {
                            Kind::Gpu => e.has.iter().any(|h| h == "gpu"),
                            Kind::Battery => e.has.iter().any(|h| h == "battery"),
                            _ => true,
                        };
                        if !feed {
                            continue;
                        }
                        let ms = CreatePopupMenu().ok()?;
                        for (i, (k, name, title)) in widgets::CATALOG.iter().enumerate() {
                            if k != kind {
                                continue;
                            }
                            let full = format!("{key}/{name}");
                            menu_item(ms, base + RM_WIDGET + i, &t(title), cfg.widgets.iter().any(|w| *w == full));
                        }
                        if *kind != Kind::Battery {
                            let _ = AppendMenuW(ms, MF_SEPARATOR, 0, PCWSTR::null());
                            let cs = CreatePopupMenu().ok()?;
                            let current = rc.and_then(|r| r.colors.get(module_key(*kind))).map(String::as_str).unwrap_or("accent");
                            let cb = base + RM_COLOR + mi * RM_COLOR_STRIDE;
                            color_entries(cs, cb, current, cfg, dpi);
                            menu_submenu(ms, cs, &t("menu.colour"));
                        }
                        if *kind == Kind::Cpu && e.splittable {
                            // one divider setting for P / E boundaries, clusters and sockets
                            menu_item(ms, base + RM_SPLIT_PE, &t(if e.hybrid { "menu.show_pe_splitter" } else { "menu.show_splitter" }), rc.map_or(true, |r| r.split_pe));
                        }
                        if *kind == Kind::Cpu && e.hybrid {
                            menu_item(ms, base + RM_GROUP_PE, &t("menu.group_pe_cores"), rc.map_or(false, |r| r.group_pe));
                        }
                        let on = cfg.widgets.iter().any(|w| widgets::split_name(w).0 == key && widgets::CATALOG.iter().any(|(k2, n, _)| k2 == kind && widgets::split_name(w).1 == *n));
                        let title = if on { format!("{}  \u{2022}", kind.title()) } else { kind.title() };
                        menu_submenu(sub, ms, &title);
                    }
                    let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                    menu_item(sub, base + RM_LABELS, &t("menu.labels"), rc.map_or(true, |r| r.labels));
                    menu_item(sub, base + RM_LEFT, &t("menu.move_left"), false);
                    menu_item(sub, base + RM_RIGHT, &t("menu.move_right"), false);
                    let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                    menu_item(sub, base + RM_FORGET, &t("menu.forget"), false);
                }
            }
            let mut title = e.name.clone();
            if !e.os.is_empty() {
                title.push_str(&format!("  {} {}", e.os, e.arch));
            }
            if let Some((c, m)) = e.load {
                title.push_str(&format!("  {:.0}% \u{00B7} {:.0}%", c * 100.0, m * 100.0));
            }
            if !e.state.is_empty() {
                title.push_str(&format!("  ({})", e.state));
            }
            menu_submenu(rm, sub, &title);
        }
        Some(rm)
    }
}

unsafe fn menu_item(menu: HMENU, id: usize, text: &str, checked: bool) {
    let t = wide(text);
    let flags = if checked { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = unsafe { AppendMenuW(menu, flags, id, pcwstr(&t)) };
}

/// Checkable entry with a colour square in the check column instead of a tick.
unsafe fn menu_color_item(menu: HMENU, id: usize, text: &str, checked: bool, color: Option<Color>, dpi: u32) {
    unsafe {
        menu_item(menu, id, text, checked);
        if let Some(bmp) = color.and_then(|c| menu::swatch(c, dpi)) {
            let pos = GetMenuItemCount(menu) - 1;
            if pos >= 0 {
                menu::set_item_bitmap(menu, pos as u32, bmp);
            }
        }
    }
}

/// Palette entry -> the colour it stands for on this machine ("system" follows the accent).
fn palette_color(name: &str, fallback: Color) -> Option<Color> {
    Some(widgets::named_color(name).unwrap_or(fallback))
}

unsafe fn menu_submenu(menu: HMENU, sub: HMENU, text: &str) {
    unsafe { menu_submenu_ex(menu, sub, text, true) }
}

/// Submenu entry; `enabled = false` greys it out (module the machine cannot provide).
unsafe fn menu_submenu_ex(menu: HMENU, sub: HMENU, text: &str, enabled: bool) {
    let t = wide(text);
    let flags = if enabled { MF_POPUP } else { MF_POPUP | MF_GRAYED };
    let _ = unsafe { AppendMenuW(menu, flags, sub.0 as usize, pcwstr(&t)) };
}

/// Settings menu: one submenu per module with a checkable entry per widget type.
unsafe fn build_menu(st: &MenuState) -> Option<HMENU> {
    let MenuState { cfg, hybrid, splittable, gpu_ok, battery_ok, update, remotes, dpi, .. } = st;
    let (hybrid, splittable, gpu_ok, battery_ok, dpi) = (*hybrid, *splittable, *gpu_ok, *battery_ok, *dpi);
    let update = update.as_deref();
    let remotes: &[RemoteEntry] = remotes;
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (mi, kind) in widgets::MODULES.iter().enumerate() {
            let sub = CreatePopupMenu().ok()?;
            for (i, (k, name, title)) in widgets::CATALOG.iter().enumerate() {
                if k != kind {
                    continue;
                }
                let on = cfg.widgets.iter().any(|w| w == name);
                menu_item(sub, MENU_WIDGET_BASE + i, &t(title), on);
            }
            if *kind != Kind::Battery {
                let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
                if let Some(cs) = color_submenu(cfg, mi, *kind, dpi) {
                    menu_submenu(sub, cs, &t("menu.colour"));
                }
            }
            if *kind == Kind::Cpu && splittable {
                menu_item(sub, MENU_SPLIT_PE, &t(if hybrid { "menu.show_pe_splitter" } else { "menu.show_splitter" }), cfg.split_pe_cores);
            }
            if *kind == Kind::Cpu && hybrid {
                menu_item(sub, MENU_GROUP_PE, &t("menu.group_pe_cores"), cfg.group_pe_cores);
            }
            let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
            menu_item(sub, MENU_MOVE_BASE + mi * 2, &t("menu.move_left"), false);
            menu_item(sub, MENU_MOVE_BASE + mi * 2 + 1, &t("menu.move_right"), false);
            let enabled = widgets::CATALOG.iter().any(|(k, n, _)| k == kind && cfg.widgets.iter().any(|w| w == n));
            let available = match kind {
                Kind::Gpu => gpu_ok,
                Kind::Battery => battery_ok,
                _ => true,
            };
            let title = if !available {
                format!("{}  {}", kind.title(), t("menu.na"))
            } else if enabled {
                format!("{}  \u{2022}", kind.title())
            } else {
                kind.title().to_string()
            };
            menu_submenu_ex(menu, sub, &title, available);
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if let Some(rm) = remote_menu(cfg, remotes, dpi) {
            let any = !cfg.remotes.is_empty();
            menu_submenu(menu, rm, &if any { format!("{}  \u{2022}", t("menu.remote")) } else { t("menu.remote") });
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        menu_item(menu, MENU_LABELS, &t("menu.labels"), cfg.labels);

        let accent = CreatePopupMenu().ok()?;
        for (i, (name, title, c)) in widgets::PALETTE.iter().enumerate() {
            menu_color_item(accent, MENU_ACCENT_BASE + i, &t(title), cfg.accent == *name, palette_color(name, *c), dpi);
        }
        menu_submenu(menu, accent, &t("menu.accent_colour"));

        let font = CreatePopupMenu().ok()?;
        for (i, (scale, title)) in FONT_SIZES.iter().enumerate() {
            menu_item(font, MENU_FONT_BASE + i, &t(title), (cfg.font_scale - scale).abs() < 0.01);
        }
        menu_submenu(menu, font, &t("menu.font_size"));

        let chart = CreatePopupMenu().ok()?;
        for (i, (w, title)) in CHART_WIDTHS.iter().enumerate() {
            menu_item(chart, MENU_CHART_BASE + i, &t(title), cfg.chart_width == *w);
        }
        menu_submenu(menu, chart, &t("menu.chart_width"));

        let bars = CreatePopupMenu().ok()?;
        for (i, (name, _, title)) in widgets::BAR_WIDTHS.iter().enumerate() {
            menu_item(bars, MENU_BAR_BASE + i, &t(title), cfg.bar_width == *name);
        }
        menu_submenu(menu, bars, &t("menu.bar_width"));

        let top = CreatePopupMenu().ok()?;
        for (i, n) in TOP_COUNTS.iter().enumerate() {
            menu_item(top, MENU_TOP_BASE + i, &n.to_string(), cfg.top_processes == *n);
        }
        menu_submenu(menu, top, &t("menu.top_processes"));

        let pos = CreatePopupMenu().ok()?;
        menu_item(pos, MENU_POS_RIGHT, &t("menu.pos_right"), cfg.position != "left");
        menu_item(pos, MENU_POS_LEFT, &t("menu.pos_left"), cfg.position == "left");
        menu_submenu(menu, pos, &t("menu.position"));

        let interval = CreatePopupMenu().ok()?;
        for (i, (ms, title)) in INTERVALS.iter().enumerate() {
            menu_item(interval, MENU_INTERVAL_BASE + i, title, cfg.update_ms == *ms);
        }
        menu_submenu(menu, interval, &t("menu.update_interval"));

        let theme = CreatePopupMenu().ok()?;
        menu_item(theme, MENU_THEME_AUTO, &t("menu.theme_auto"), cfg.theme == "auto");
        menu_item(theme, MENU_THEME_DARK, &t("menu.theme_dark"), cfg.theme == "dark");
        menu_item(theme, MENU_THEME_LIGHT, &t("menu.theme_light"), cfg.theme == "light");
        menu_submenu(menu, theme, &t("menu.theme"));

        let lang = CreatePopupMenu().ok()?;
        menu_item(lang, MENU_LANG_BASE, &t("menu.lang_system"), cfg.language == "auto");
        let _ = AppendMenuW(lang, MF_SEPARATOR, 0, PCWSTR::null());
        for (i, (code, name, _)) in i18n::LOCALES.iter().enumerate() {
            menu_item(lang, MENU_LANG_BASE + 1 + i, name, cfg.language == *code);
        }
        menu_submenu(menu, lang, &t("menu.language"));

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        menu_item(menu, MENU_AUTOSTART, &t("menu.start_with_windows"), util::autostart_enabled());
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        menu_item(menu, MENU_TASKMGR, &t("menu.task_manager"), false);
        menu_item(menu, MENU_CONFIG, &t("menu.open_config"), false);
        menu_item(menu, MENU_RELOAD, &t("menu.reload_config"), false);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        menu_item(menu, MENU_ABOUT, &t("menu.about"), false);
        match update {
            Some(v) => menu_item(menu, MENU_GET_UPDATE, &i18n::tf("menu.update_available", &[("v", v)]), false),
            None => menu_item(menu, MENU_CHECK_UPDATE, &t("menu.check_updates"), false),
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        menu_item(menu, MENU_EXIT, &t("menu.exit"), false);
        Some(menu)
    }
}

/// Add `item` to the list when absent, remove it when present.
fn toggle(list: &mut Vec<String>, item: String) {
    if let Some(i) = list.iter().position(|s| *s == item) {
        list.remove(i);
    } else {
        list.push(item);
    }
}

fn open_url(url: &str) {
    let verb = wide("open");
    let u = wide(url);
    unsafe {
        ShellExecuteW(None, pcwstr(&verb), pcwstr(&u), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// A finished update check: remember a newer release for the menu; after a manual check also
/// tell the user what was found.
fn update_result(hidden: HWND, manual: bool) {
    let Some(result) = update::take_result() else { return };
    let current = about::VERSION;
    let newer = match &result {
        Ok(v) if update::is_newer(v, current) => Some(v.clone()),
        _ => None,
    };
    match &result {
        Ok(v) => log!("update check: latest {v}, running {current}"),
        Err(e) => log!("update check failed: {e}"),
    }
    with_app(|app| app.update_available = newer.clone());
    if !manual {
        return;
    }
    let title = wide(&t("update.title"));
    let (text, style) = match (&result, &newer) {
        (Ok(_), Some(v)) => (i18n::tf("update.available", &[("v", v), ("cur", current)]), MB_YESNO | MB_ICONQUESTION),
        (Ok(_), None) => (i18n::tf("update.latest", &[("v", current)]), MB_OK | MB_ICONINFORMATION),
        (Err(e), _) => (i18n::tf("update.failed", &[("err", e)]), MB_OK | MB_ICONERROR),
    };
    let text_w = wide(&text);
    let r = unsafe { MessageBoxW(hidden, pcwstr(&text_w), pcwstr(&title), style | MB_TOPMOST) };
    if newer.is_some() && r == IDYES {
        open_url(update::RELEASES_URL);
    }
}

/// Everything the menu builder needs, snapshotted from the app so no borrow is held while
/// the menu runs its modal loop.
struct MenuState {
    hidden: HWND,
    cfg: Config,
    hybrid: bool,
    splittable: bool,
    gpu_ok: bool,
    battery_ok: bool,
    update: Option<String>,
    remotes: Vec<RemoteEntry>,
    dpi: u32,
}

/// Snapshot for the menu. `fresh` re-reads the remote list; a refresh while the menu is open
/// keeps the list the open menu was built from so the ids stay valid.
fn menu_state(fresh: bool) -> Option<MenuState> {
    with_app(|app| {
        let remotes = if fresh { app.remote_entries() } else { app.menu_remote.clone() };
        app.menu_remote = remotes.clone();
        MenuState {
            hidden: app.hidden,
            cfg: app.cfg.clone(),
            hybrid: app.metrics.cpu_hybrid(),
            splittable: app.metrics.cpu_breaks().iter().any(|b| *b),
            gpu_ok: app.metrics.gpu_available,
            battery_ok: app.metrics.battery_available,
            update: app.update_available.clone(),
            remotes,
            dpi: app.taskbar.as_ref().map(|t| t.dpi()).or_else(|| util::dpi_for_window(app.hidden)).unwrap_or(96),
        }
    })
}

/// Commands that change a setting and keep the menu open; the rest close it.
fn menu_sticky(id: u32) -> bool {
    let id = id as usize;
    match id {
        MENU_EXIT | MENU_CONFIG | MENU_RELOAD | MENU_TASKMGR | MENU_ABOUT | MENU_CHECK_UPDATE | MENU_GET_UPDATE => false,
        c if c >= MENU_REMOTE_BASE => !matches!((c - MENU_REMOTE_BASE) % MENU_REMOTE_STRIDE, RM_CONNECT | RM_FORGET),
        _ => true,
    }
}

fn menu_rebuild() -> Option<HMENU> {
    let st = menu_state(false)?;
    unsafe { build_menu(&st) }
}

fn menu_command(cmd: u32) {
    match cmd as usize {
        0 => {}
        MENU_EXIT => unsafe { PostQuitMessage(0) },
        MENU_TASKMGR => open_taskmgr(),
        MENU_CONFIG => {
            let verb = wide("open");
            let path = wide(&Config::path().to_string_lossy());
            unsafe {
                ShellExecuteW(None, pcwstr(&verb), pcwstr(&path), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
            }
        }
        MENU_ABOUT => {
            with_app(|app| {
                app.popup.hide("about");
                let theme = app.theme();
                let light = app.is_light();
                app.about.show(&theme, light);
            });
        }
        MENU_RELOAD => {
            with_app(|app| app.reload());
        }
        MENU_CHECK_UPDATE => {
            if let Some(hidden) = with_app(|app| app.hidden) {
                update::check_async(hidden, true);
            }
        }
        MENU_GET_UPDATE => open_url(update::RELEASES_URL),
        MENU_AUTOSTART => {
            let enable = !util::autostart_enabled();
            if !util::set_autostart(enable) {
                log!("autostart change failed");
            }
        }
        other => {
            with_app(|app| app.apply_menu(other));
        }
    }
}

fn show_menu() {
    let Some(st) = menu_state(true) else { return };
    let cmd = unsafe {
        let Some(menu) = build_menu(&st) else {
            menu::free_bitmaps();
            return;
        };
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(st.hidden);
        let r = menu::track(st.hidden, menu, pt, menu_sticky, menu_command, menu_rebuild);
        let _ = PostMessageW(st.hidden, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        menu::free_bitmaps();
        r
    };
    menu_command(cmd);
}

impl App {
    /// Apply a settings change from the context menu, persist it and redraw.
    /// A click on a toggling row in the popup: flip the item in the config, save, re-render.
    fn popup_action(&mut self, action: popup::Action) {
        if let Some(key) = self.popup.source.clone() {
            self.remote_popup_action(&key, action);
            return;
        }
        // at least one interface / disk must stay on, otherwise the widget shows nothing
        match action {
            popup::Action::ToggleNet(alias) => {
                let last = self.metrics.net_ifaces.iter().filter(|i| i.enabled).count() <= 1
                    && self.metrics.net_ifaces.iter().any(|i| i.enabled && i.alias == alias);
                if last {
                    return;
                }
                toggle(&mut self.cfg.net_disabled, alias);
                self.metrics.set_net_disabled(&self.cfg.net_disabled);
            }
            popup::Action::ToggleDisk(model) => {
                let last = self.metrics.disks.iter().filter(|d| d.enabled).count() <= 1
                    && self.metrics.disks.iter().any(|d| d.enabled && d.model == model);
                if last {
                    return;
                }
                toggle(&mut self.cfg.disks_disabled, model);
                self.metrics.set_disks_disabled(&self.cfg.disks_disabled);
            }
        }
        self.cfg.save();
        self.render_popup(false);
    }

    /// The same toggles for a remote machine, kept in its `[remotes.<key>]` section.
    fn remote_popup_action(&mut self, key: &str, action: popup::Action) {
        let Some(a) = self.remotes.agents.get(key) else { return };
        let Some(rc) = self.cfg.remotes.get_mut(key) else { return };
        match action {
            popup::Action::ToggleNet(alias) => {
                let last = a.data.net_ifaces.iter().filter(|i| i.enabled).count() <= 1 && a.data.net_ifaces.iter().any(|i| i.enabled && i.alias == alias);
                if last {
                    return;
                }
                toggle(&mut rc.net_disabled, alias);
            }
            popup::Action::ToggleDisk(model) => {
                let last = a.data.disks.iter().filter(|d| d.enabled).count() <= 1 && a.data.disks.iter().any(|d| d.enabled && d.model == model);
                if last {
                    return;
                }
                toggle(&mut rc.disks_disabled, model);
            }
        }
        let (net, disks) = (rc.net_disabled.clone(), rc.disks_disabled.clone());
        self.remotes.set_disabled(key, &net, &disks);
        self.cfg.save();
        self.render_popup(false);
    }

    /// Lines of the Remote submenu: configured machines first, then the ones only heard.
    fn remote_entries(&self) -> Vec<RemoteEntry> {
        let seen = self.remotes.seen();
        let mut out = Vec::new();
        for (key, rc) in &self.cfg.remotes {
            let s = seen.iter().find(|s| s.beacon.id == rc.id);
            let agent = self.remotes.agents.get(key);
            let (os, arch) = match agent.and_then(|a| a.info.as_ref()) {
                Some(i) => (i.os.clone(), i.arch.clone()),
                None => s.map(|s| (s.beacon.os.clone(), s.beacon.arch.clone())).unwrap_or_default(),
            };
            out.push(RemoteEntry {
                key: Some(key.clone()),
                id: rc.id.clone(),
                name: if rc.name.is_empty() { key.clone() } else { rc.name.clone() },
                os,
                arch,
                address: rc.address.clone(),
                enabled: rc.enabled,
                state: if rc.enabled { self.remotes.state_text(key) } else { String::new() },
                load: s.map(|s| (s.beacon.cpu, s.beacon.mem)).or_else(|| agent.filter(|a| a.online).map(|a| (a.data.cpu_total, a.data.mem.pct))),
                has: agent.and_then(|a| a.info.as_ref()).map(|i| i.has.clone()).unwrap_or_else(|| vec!["cpu".into(), "mem".into(), "net".into(), "disk".into()]),
                hybrid: agent.map_or(false, |a| a.data.cpu_hybrid()),
                splittable: agent.map_or(false, |a| a.data.cpu_breaks().iter().any(|b| *b)),
            });
        }
        for s in &seen {
            // configured ones are listed above; our own share beacon is not a remote machine
            if self.cfg.remotes.values().any(|rc| rc.id == s.beacon.id) || (!self.cfg.share_id.is_empty() && s.beacon.id == self.cfg.share_id) {
                continue;
            }
            out.push(RemoteEntry {
                key: None,
                id: s.beacon.id.clone(),
                name: s.beacon.name.clone(),
                os: s.beacon.os.clone(),
                arch: s.beacon.arch.clone(),
                address: s.addr.to_string(),
                enabled: false,
                state: t("remote.found"),
                load: Some((s.beacon.cpu, s.beacon.mem)),
                has: Vec::new(),
                hybrid: false,
                splittable: false,
            });
        }
        out
    }

    /// A command from an agent's block of the Remote submenu.
    fn apply_remote(&mut self, entry: usize, item: usize) {
        let Some(e) = self.menu_remote.get(entry).cloned() else { return };
        match (e.key.clone(), item) {
            (None, RM_CONNECT) => {
                // first connection: a config entry named after the machine
                let key = remote::key_for(&e.name, &self.cfg.remotes);
                let label: String = e.name.chars().filter(|c| c.is_ascii_alphanumeric()).take(3).collect::<String>().to_uppercase();
                self.cfg.remotes.insert(
                    key.clone(),
                    config::RemoteCfg { id: e.id.clone(), name: e.name.clone(), address: e.address.clone(), enabled: true, label, ..Default::default() },
                );
                // start with the two charts every agent can feed
                widgets::toggle_widget(&mut self.cfg.widgets, &format!("{key}/cpu_line"));
                widgets::toggle_widget(&mut self.cfg.widgets, &format!("{key}/mem"));
            }
            (None, _) => return,
            (Some(key), RM_CONNECT) => {
                if let Some(rc) = self.cfg.remotes.get_mut(&key) {
                    rc.enabled = !rc.enabled;
                }
            }
            (Some(key), RM_FORGET) => {
                self.cfg.remotes.remove(&key);
                self.cfg.widgets.retain(|w| widgets::split_name(w).0 != key);
                if self.popup.source.as_deref() == Some(key.as_str()) {
                    self.popup.hide("forget");
                }
            }
            (Some(key), RM_LABELS) => {
                if let Some(rc) = self.cfg.remotes.get_mut(&key) {
                    rc.labels = !rc.labels;
                }
            }
            (Some(key), RM_SPLIT_PE) => {
                if let Some(rc) = self.cfg.remotes.get_mut(&key) {
                    rc.split_pe = !rc.split_pe;
                }
            }
            (Some(key), RM_GROUP_PE) => {
                if let Some(rc) = self.cfg.remotes.get_mut(&key) {
                    rc.group_pe = !rc.group_pe;
                    let on = rc.group_pe;
                    self.remotes.set_group_pe(&key, on);
                }
            }
            (Some(key), RM_LEFT) => widgets::move_source(&mut self.cfg.widgets, &key, -1),
            (Some(key), RM_RIGHT) => widgets::move_source(&mut self.cfg.widgets, &key, 1),
            (Some(key), i) if (RM_WIDGET..RM_WIDGET + widgets::CATALOG.len()).contains(&i) => {
                widgets::toggle_widget(&mut self.cfg.widgets, &format!("{key}/{}", widgets::CATALOG[i - RM_WIDGET].1));
            }
            (Some(key), i) if (RM_COLOR..RM_COLOR + widgets::MODULES.len() * RM_COLOR_STRIDE).contains(&i) => {
                let mi = (i - RM_COLOR) / RM_COLOR_STRIDE;
                let entry = (i - RM_COLOR) % RM_COLOR_STRIDE;
                let value = if entry < COLOR_MODES.len() {
                    COLOR_MODES[entry].0
                } else {
                    widgets::PALETTE.get(entry - COLOR_MODES.len()).map(|p| p.0).unwrap_or("accent")
                };
                if let Some(rc) = self.cfg.remotes.get_mut(&key) {
                    let mk = module_key(widgets::MODULES[mi]).to_string();
                    if value == "accent" {
                        rc.colors.remove(&mk);
                    } else {
                        rc.colors.insert(mk, value.to_string());
                    }
                }
            }
            _ => return,
        }
        let keys = remote_keys(&self.cfg);
        widgets::normalize(&mut self.cfg.widgets, &keys);
        self.cfg.save();
        self.widgets = widgets::build(&self.cfg.widgets);
        if self.remotes.poll(&mut self.cfg) {
            self.cfg.save();
        }
        self.tick();
    }

    fn apply_menu(&mut self, cmd: usize) {
        let mut interval_changed = false;
        match cmd {
            MENU_LABELS => self.cfg.labels = !self.cfg.labels,
            MENU_SPLIT_PE => self.cfg.split_pe_cores = !self.cfg.split_pe_cores,
            c if (MENU_MOVE_BASE..MENU_MOVE_BASE + widgets::MODULES.len() * 2).contains(&c) => {
                let mi = (c - MENU_MOVE_BASE) / 2;
                let dir = if (c - MENU_MOVE_BASE) % 2 == 0 { -1 } else { 1 };
                widgets::move_module(&mut self.cfg.widgets, widgets::MODULES[mi], dir);
            }
            MENU_GROUP_PE => {
                self.cfg.group_pe_cores = !self.cfg.group_pe_cores;
                self.metrics.set_group_pe(self.cfg.group_pe_cores);
            }
            c if (MENU_LANG_BASE..=MENU_LANG_BASE + i18n::LOCALES.len()).contains(&c) => {
                self.cfg.language = if c == MENU_LANG_BASE { "auto".to_string() } else { i18n::LOCALES[c - MENU_LANG_BASE - 1].0.to_string() };
                i18n::set_language(&self.cfg.language);
            }
            c if (MENU_TOP_BASE..MENU_TOP_BASE + TOP_COUNTS.len()).contains(&c) => {
                self.cfg.top_processes = TOP_COUNTS[c - MENU_TOP_BASE];
                self.metrics.set_top_n(self.cfg.top_processes as usize);
            }
            c if (MENU_CHART_BASE..MENU_CHART_BASE + CHART_WIDTHS.len()).contains(&c) => {
                self.cfg.chart_width = CHART_WIDTHS[c - MENU_CHART_BASE].0;
            }
            c if (MENU_BAR_BASE..MENU_BAR_BASE + widgets::BAR_WIDTHS.len()).contains(&c) => {
                self.cfg.bar_width = widgets::BAR_WIDTHS[c - MENU_BAR_BASE].0.to_string();
            }
            c if (MENU_FONT_BASE..MENU_FONT_BASE + FONT_SIZES.len()).contains(&c) => {
                self.cfg.font_scale = FONT_SIZES[c - MENU_FONT_BASE].0;
            }
            c if (MENU_ACCENT_BASE..MENU_ACCENT_BASE + widgets::PALETTE.len()).contains(&c) => {
                self.cfg.accent = widgets::PALETTE[c - MENU_ACCENT_BASE].0.to_string();
            }
            c if (MENU_COLOR_BASE..MENU_COLOR_BASE + widgets::MODULES.len() * MENU_COLOR_STRIDE).contains(&c) => {
                let mi = (c - MENU_COLOR_BASE) / MENU_COLOR_STRIDE;
                let entry = (c - MENU_COLOR_BASE) % MENU_COLOR_STRIDE;
                let value = if entry < COLOR_MODES.len() {
                    COLOR_MODES[entry].0
                } else {
                    widgets::PALETTE.get(entry - COLOR_MODES.len()).map(|p| p.0).unwrap_or("accent")
                };
                let key = module_key(widgets::MODULES[mi]).to_string();
                if value == "accent" {
                    self.cfg.colors.remove(&key);
                } else {
                    self.cfg.colors.insert(key, value.to_string());
                }
                self.cfg.utilization_colors = false;
            }
            MENU_POS_RIGHT => self.cfg.position = "right".into(),
            MENU_POS_LEFT => self.cfg.position = "left".into(),
            MENU_THEME_AUTO => self.cfg.theme = "auto".into(),
            MENU_THEME_DARK => self.cfg.theme = "dark".into(),
            MENU_THEME_LIGHT => self.cfg.theme = "light".into(),
            c if (MENU_INTERVAL_BASE..MENU_INTERVAL_BASE + INTERVALS.len()).contains(&c) => {
                self.cfg.update_ms = INTERVALS[c - MENU_INTERVAL_BASE].0;
                interval_changed = true;
            }
            c if (MENU_WIDGET_BASE..MENU_WIDGET_BASE + widgets::CATALOG.len()).contains(&c) => {
                widgets::toggle_widget(&mut self.cfg.widgets, widgets::CATALOG[c - MENU_WIDGET_BASE].1);
            }
            MENU_REMOTE_DISCOVERY => {
                self.cfg.discovery = !self.cfg.discovery;
                let on = self.cfg.discovery;
                self.remotes.set_discovery(on);
            }
            MENU_REMOTE_SHARE => {
                self.cfg.share = !self.cfg.share;
                self.apply_share();
            }
            c if c >= MENU_REMOTE_BASE => {
                self.apply_remote((c - MENU_REMOTE_BASE) / MENU_REMOTE_STRIDE, (c - MENU_REMOTE_BASE) % MENU_REMOTE_STRIDE);
                return;
            }
            _ => return,
        }
        self.cfg.save();
        self.widgets = widgets::build(&self.cfg.widgets);
        if interval_changed {
            unsafe {
                SetTimer(self.hidden, TIMER_ID, self.cfg.update_ms.max(250), None);
            }
        }
        if self.cfg.position == "left" {
            if let Some(tb) = &self.taskbar {
                tb.restore();
            }
        }
        self.tick();
    }
}
