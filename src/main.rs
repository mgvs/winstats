#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod canvas;
mod config;
mod i18n;
mod metrics;
mod about;
mod popup;
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
use windows::Win32::Graphics::Gdi::ScreenToClient;
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
    wm_taskbar_created: u32,
    ticks: u64,
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
    widgets::normalize(&mut cfg.widgets);
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
            wm_taskbar_created,
            ticks: 0,
        })
    });

    log!("winstats started (build {}, win11={}, floating={})", util::windows_build(), util::is_windows11(), floating);
    with_app(|app| app.tick());
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
            modes,
        }
    }

    fn tick(&mut self) {
        self.ticks += 1;
        if !self.ensure_bar() {
            return;
        }
        self.metrics.update(self.popup.visible);
        self.render();
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

        let widths: Vec<i32> = self.widgets.iter().map(|w| w.width(&mut self.canvas, &theme, &self.metrics)).collect();
        let visible: Vec<(usize, i32)> = widths.iter().cloned().enumerate().filter(|(_, w)| *w > 0).collect();
        let total = edge * 2 + visible.iter().map(|(_, w)| *w).sum::<i32>() + gap * (visible.len().saturating_sub(1)) as i32;

        self.canvas.resize(total.max(1), h.max(1));
        self.canvas.clear();
        // alpha 0 pixels are transparent to hit-testing on layered windows: keep the strip clickable
        self.canvas.fill_rect(0, 0, total, h, Color::rgba(0, 0, 0, 1));
        let y = (h - theme.inner_h) / 2;
        let mut x = edge;
        self.layout.clear();
        for (i, w) in &visible {
            self.widgets[*i].draw(&mut self.canvas, x, y, &theme, &self.metrics);
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
            let light = self.is_light();
            self.popup.render(&theme, light, &self.metrics, false);
        }
    }

    /// Left click on the strip at bar-client x.
    fn click(&mut self, x: i32) {
        let Some((_, kind, x0, x1)) = self.layout.iter().find(|(_, _, a, b)| x >= *a && x < *b).cloned() else { return };
        let Some(tb) = self.taskbar.as_ref() else { return };
        let mut bar_rc = RECT::default();
        let mut tray_rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.bar, &mut bar_rc);
            let _ = GetWindowRect(tb.tray, &mut tray_rc);
        }
        let anchor = RECT { left: bar_rc.left + x0, top: bar_rc.top, right: bar_rc.left + x1, bottom: bar_rc.bottom };
        if self.popup.toggle(kind, anchor, tray_rc) {
            self.metrics.begin_details();
            let theme = self.theme();
            let light = self.is_light();
            self.popup.render(&theme, light, &self.metrics, true);
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
        if !self.floating || self.drag.is_some() {
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
        widgets::normalize(&mut self.cfg.widgets);
        i18n::set_language(&self.cfg.language);
        self.widgets = widgets::build(&self.cfg.widgets);
        self.metrics = Metrics::new(&self.cfg.disk, self.cfg.top_processes as usize, &self.cfg.net_disabled, &self.cfg.disks_disabled);
        self.metrics.set_group_pe(self.cfg.group_pe_cores);
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
                    let theme = app.theme();
                    let light = app.is_light();
                    app.popup.render(&theme, light, &app.metrics, false);
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
/// language: MENU_LANG_BASE = system language, then one entry per locale
const MENU_LANG_BASE: usize = 600;
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
unsafe fn color_submenu(cfg: &Config, mi: usize, kind: Kind) -> Option<HMENU> {
    unsafe {
        let sub = CreatePopupMenu().ok()?;
        let current = cfg.colors.get(module_key(kind)).map(String::as_str).unwrap_or("accent");
        let base = MENU_COLOR_BASE + mi * MENU_COLOR_STRIDE;
        for (i, (name, title)) in COLOR_MODES.iter().enumerate() {
            menu_item(sub, base + i, &t(title), current == *name);
        }
        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
        for (i, (name, title, _)) in widgets::PALETTE.iter().enumerate() {
            menu_item(sub, base + COLOR_MODES.len() + i, &t(title), current == *name);
        }
        Some(sub)
    }
}
const INTERVALS: &[(u32, &str)] = &[(500, "0.5 s"), (1000, "1 s"), (2000, "2 s"), (5000, "5 s")];

unsafe fn menu_item(menu: HMENU, id: usize, text: &str, checked: bool) {
    let t = wide(text);
    let flags = if checked { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = unsafe { AppendMenuW(menu, flags, id, pcwstr(&t)) };
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
unsafe fn build_menu(cfg: &Config, hybrid: bool, gpu_ok: bool, battery_ok: bool, update: Option<&str>) -> Option<HMENU> {
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
                if let Some(cs) = color_submenu(cfg, mi, *kind) {
                    menu_submenu(sub, cs, &t("menu.colour"));
                }
            }
            if *kind == Kind::Cpu && hybrid {
                menu_item(sub, MENU_SPLIT_PE, &t("menu.show_pe_splitter"), cfg.split_pe_cores);
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
        menu_item(menu, MENU_LABELS, &t("menu.labels"), cfg.labels);

        let accent = CreatePopupMenu().ok()?;
        for (i, (name, title, _)) in widgets::PALETTE.iter().enumerate() {
            menu_item(accent, MENU_ACCENT_BASE + i, &t(title), cfg.accent == *name);
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

fn show_menu() {
    let Some((hidden, cfg, hybrid, gpu_ok, battery_ok, update)) = with_app(|app| {
        (app.hidden, app.cfg.clone(), app.metrics.cpu_hybrid(), app.metrics.gpu_available, app.metrics.battery_available, app.update_available.clone())
    }) else {
        return;
    };
    let cmd = unsafe {
        let Some(menu) = build_menu(&cfg, hybrid, gpu_ok, battery_ok, update.as_deref()) else { return };
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hidden);
        let r = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hidden, None);
        let _ = PostMessageW(hidden, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        r.0 as usize
    };
    match cmd {
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
        MENU_CHECK_UPDATE => update::check_async(hidden, true),
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

impl App {
    /// Apply a settings change from the context menu, persist it and redraw.
    /// A click on a toggling row in the popup: flip the item in the config, save, re-render.
    fn popup_action(&mut self, action: popup::Action) {
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
        let theme = self.theme();
        let light = self.is_light();
        self.popup.render(&theme, light, &self.metrics, false);
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
