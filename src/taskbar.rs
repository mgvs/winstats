//! Locating the Explorer taskbar and hosting our window inside it:
//! SetParent(our_hwnd, Shell_TrayWnd), then keep it positioned next to the notification area.

use crate::util::{pcwstr, wide};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, GetMonitorInfoW, MapWindowPoints, MonitorFromWindow, DISPLAY_DEVICEW,
    MONITORINFOEXW, MONITOR_DEFAULTTONULL,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

pub struct Taskbar {
    /// true: the strip is a top-level always-on-top window over the taskbar (Windows 7);
    /// false: it is a layered child of Shell_TrayWnd (Windows 8 and later)
    pub floating: bool,
    pub tray: HWND,
    pub notify: Option<HWND>,
    secondary: bool,
    /// Windows 10 only: the task-list host we shrink to make room.
    pub rebar: Option<HWND>,
    pub rebar_original: Option<RECT>,
}

fn find_child(parent: HWND, class: &str) -> Option<HWND> {
    let c = wide(class);
    unsafe { FindWindowExW(parent, None, pcwstr(&c), PCWSTR::null()).ok() }
}

#[derive(Clone)]
pub struct DisplayTaskbar {
    pub id: String,
    pub label: String,
    pub tray: HWND,
}

fn wide_string(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|&c| c == 0).unwrap_or(value.len())])
}

fn describe_display(tray: HWND) -> Option<DisplayTaskbar> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let mut monitor = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
    unsafe {
        let handle = MonitorFromWindow(tray, MONITOR_DEFAULTTONULL);
        if !GetMonitorInfoW(handle, &mut info.monitorInfo).as_bool() {
            return None;
        }
        let _ = EnumDisplayDevicesW(PCWSTR(info.szDevice.as_ptr()), 0, &mut monitor, EDD_GET_DEVICE_INTERFACE_NAME);
    }
    let source = wide_string(&info.szDevice);
    let device_id = wide_string(&monitor.DeviceID);
    let name = wide_string(&monitor.DeviceString);
    let rect = info.monitorInfo.rcMonitor;
    let label = format!("{} · {} × {} ({}, {})",
        if name.is_empty() { &source } else { &name },
        rect.right - rect.left, rect.bottom - rect.top, rect.left, rect.top);
    Some(DisplayTaskbar {
        id: if device_id.is_empty() { source } else { device_id },
        label,
        tray,
    })
}

fn primary() -> Option<HWND> {
    let cls = wide("Shell_TrayWnd");
    unsafe { FindWindowExW(None, None, pcwstr(&cls), PCWSTR::null()).ok() }
}

fn secondary_taskbars() -> impl Iterator<Item = HWND> {
    let cls = wide("Shell_SecondaryTrayWnd");
    let mut previous = None;
    std::iter::from_fn(move || {
        let tray = unsafe { FindWindowExW(None, previous.as_ref(), pcwstr(&cls), PCWSTR::null()).ok()? };
        previous = Some(tray);
        Some(tray)
    })
}

pub fn displays() -> Vec<DisplayTaskbar> {
    primary().into_iter().chain(secondary_taskbars()).filter_map(describe_display).collect()
}

pub fn target(monitor: &str) -> Option<HWND> {
    if !monitor.is_empty() {
        if let Some(display) = displays().into_iter().find(|display| display.id == monitor) {
            return Some(display.tray);
        }
    }
    primary()
}

pub fn find(floating: bool, tray: HWND) -> Taskbar {
    let notify = find_child(tray, "TrayNotifyWnd");
    let rebar = find_child(tray, "ReBarWindow32");
    let rebar_original = rebar.and_then(|r| child_rect(tray, r));
    let secondary = Some(tray) != primary();
    Taskbar { floating, tray, notify, secondary, rebar, rebar_original }
}

/// Rect of `child` expressed in `parent` client coordinates.
pub fn child_rect(parent: HWND, child: HWND) -> Option<RECT> {
    unsafe {
        let mut rc = RECT::default();
        GetWindowRect(child, &mut rc).ok()?;
        let mut pts = [POINT { x: rc.left, y: rc.top }, POINT { x: rc.right, y: rc.bottom }];
        MapWindowPoints(None, parent, &mut pts);
        Some(RECT { left: pts[0].x, top: pts[0].y, right: pts[1].x, bottom: pts[1].y })
    }
}

impl Taskbar {
    pub fn alive(&self) -> bool {
        unsafe { IsWindow(self.tray).as_bool() }
    }

    /// DPI of the taskbar's monitor (Windows 10+), else the session-wide DPI from the screen DC.
    pub fn dpi(&self) -> u32 {
        if let Some(d) = crate::util::dpi_for_window(self.tray) {
            return d;
        }
        use windows::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, LOGPIXELSX, ReleaseDC};
        unsafe {
            let hdc = GetDC(None);
            let d = GetDeviceCaps(hdc, LOGPIXELSX);
            ReleaseDC(None, hdc);
            if d <= 0 { 96 } else { d as u32 }
        }
    }

    pub fn client(&self) -> RECT {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.tray, &mut rc);
        }
        rc
    }

    /// True for a taskbar docked at the left or right edge (taller than wide).
    pub fn vertical(&self) -> bool {
        let rc = self.client();
        rc.bottom - rc.top > rc.right - rc.left
    }

    /// Size across the taskbar: its height when horizontal, its width when vertical.
    pub fn thickness(&self) -> i32 {
        let rc = self.client();
        if self.vertical() {
            rc.right - rc.left
        } else {
            rc.bottom - rc.top
        }
    }

    /// True when the taskbar sits above `hwnd` in the z-order (the floating strip is covered).
    pub fn covers(&self, hwnd: HWND) -> bool {
        let mut h = unsafe { GetWindow(hwnd, GW_HWNDPREV) };
        let mut guard = 0;
        while let Ok(w) = h {
            if w == self.tray {
                return true;
            }
            guard += 1;
            if guard > 512 {
                break;
            }
            h = unsafe { GetWindow(w, GW_HWNDPREV) };
        }
        false
    }

    /// Reparent `hwnd` (created as a layered popup) into the taskbar. In floating mode
    /// (Windows 7 cannot host a layered child) the window stays top-level and `place` keeps it
    /// over the taskbar; a WinEvent hook in main.rs re-raises it when Explorer lifts the taskbar.
    pub fn attach(&self, hwnd: HWND) -> bool {
        if self.floating {
            return true;
        }
        unsafe {
            // `as _`: isize on 64-bit, i32 on 32-bit where the crate maps this to SetWindowLongW
            SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS).0 as _);
            match SetParent(hwnd, self.tray) {
                Ok(_) => true,
                Err(e) => {
                    crate::log!("SetParent failed: {e}");
                    false
                }
            }
        }
    }

    /// Position the strip; `len` is its size along the taskbar. Returns the rect used (taskbar
    /// client coords).
    pub fn place(&self, hwnd: HWND, len: i32, position: &str, offset_px: i32) -> RECT {
        let rc = self.client();
        let notify = self.notify.and_then(|n| child_rect(self.tray, n));
        if self.vertical() {
            return self.place_vertical(hwnd, len, position, offset_px, rc, notify);
        }
        let h = rc.bottom - rc.top;
        let width = len;
        let x = match position {
            "left" => rc.left + offset_px,
            _ => {
                let anchor = notify.map(|n| n.left).unwrap_or_else(|| {
                    // Explorer can draw the secondary clock inside a full-width XAML host,
                    // with no separate notification-area HWND to measure.
                    let clock_space = if self.secondary { (144 * self.dpi() as i32 + 48) / 96 } else { 0 };
                    rc.right - clock_space
                });
                anchor - width - offset_px
            }
        };
        let x = x.max(rc.left);
        unsafe {
            if let (Some(rebar), Some(orig)) = (self.rebar, self.rebar_original) {
                // Windows 10: shrink the task list so it does not run underneath us.
                if let Some(cur) = child_rect(self.tray, rebar) {
                    let desired_right = if position == "left" { orig.right } else { x - 2 };
                    let desired_left = if position == "left" { (x + width + 2).max(orig.left) } else { orig.left };
                    if cur.right != desired_right || cur.left != desired_left {
                        let _ = MoveWindow(rebar, desired_left, cur.top, desired_right - desired_left, cur.bottom - cur.top, true);
                    }
                }
            }
            if self.floating {
                // floating strip: taskbar client coords -> screen, kept above the taskbar
                let mut pt = POINT { x, y: rc.top };
                let _ = windows::Win32::Graphics::Gdi::ClientToScreen(self.tray, &mut pt);
                let _ = SetWindowPos(hwnd, HWND_TOPMOST, pt.x, pt.y, width, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            } else {
                let _ = SetWindowPos(hwnd, HWND_TOP, x, rc.top, width, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
        }
        RECT { left: x, top: rc.top, right: x + width, bottom: rc.top + h }
    }

    /// Vertical taskbar: the strip is a column. "left" is the top end, "right" the end next to
    /// the notification area, like the horizontal layout turned by 90 degrees.
    fn place_vertical(&self, hwnd: HWND, len: i32, position: &str, offset_px: i32, rc: RECT, notify: Option<RECT>) -> RECT {
        let w = rc.right - rc.left;
        let y = match position {
            "left" => rc.top + offset_px,
            _ => notify.map(|n| n.top).unwrap_or(rc.bottom) - len - offset_px,
        };
        let y = y.max(rc.top);
        unsafe {
            if let (Some(rebar), Some(orig)) = (self.rebar, self.rebar_original) {
                if let Some(cur) = child_rect(self.tray, rebar) {
                    let desired_bottom = if position == "left" { orig.bottom } else { y - 2 };
                    let desired_top = if position == "left" { (y + len + 2).max(orig.top) } else { orig.top };
                    if cur.bottom != desired_bottom || cur.top != desired_top {
                        let _ = MoveWindow(rebar, cur.left, desired_top, cur.right - cur.left, desired_bottom - desired_top, true);
                    }
                }
            }
            if self.floating {
                let mut pt = POINT { x: rc.left, y };
                let _ = windows::Win32::Graphics::Gdi::ClientToScreen(self.tray, &mut pt);
                let _ = SetWindowPos(hwnd, HWND_TOPMOST, pt.x, pt.y, w, len, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            } else {
                let _ = SetWindowPos(hwnd, HWND_TOP, rc.left, y, w, len, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
        }
        RECT { left: rc.left, top: y, right: rc.right, bottom: y + len }
    }

    /// Undo the Windows 10 task-list shrink.
    pub fn restore(&self) {
        if let (Some(rebar), Some(orig)) = (self.rebar, self.rebar_original) {
            unsafe {
                let _ = MoveWindow(rebar, orig.left, orig.top, orig.right - orig.left, orig.bottom - orig.top, true);
            }
        }
    }
}
