//! Locating the Explorer taskbar and hosting our window inside it:
//! SetParent(our_hwnd, Shell_TrayWnd), then keep it positioned next to the notification area.

use crate::util::{pcwstr, wide};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::MapWindowPoints;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

pub struct Taskbar {
    /// true: the strip is a top-level always-on-top window over the taskbar (Windows 7);
    /// false: it is a layered child of Shell_TrayWnd (Windows 8 and later)
    pub floating: bool,
    pub tray: HWND,
    pub notify: Option<HWND>,
    /// Windows 10 only: the task-list host we shrink to make room.
    pub rebar: Option<HWND>,
    pub rebar_original: Option<RECT>,
}

fn find_child(parent: HWND, class: &str) -> Option<HWND> {
    let c = wide(class);
    unsafe { FindWindowExW(parent, None, pcwstr(&c), PCWSTR::null()).ok() }
}

pub fn find(floating: bool) -> Option<Taskbar> {
    let cls = wide("Shell_TrayWnd");
    let tray = unsafe { FindWindowExW(None, None, pcwstr(&cls), PCWSTR::null()).ok()? };
    let notify = find_child(tray, "TrayNotifyWnd");
    let rebar = find_child(tray, "ReBarWindow32");
    let rebar_original = rebar.and_then(|r| child_rect(tray, r));
    Some(Taskbar { floating, tray, notify, rebar, rebar_original })
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

    /// Position the strip. Returns the rect used (taskbar client coords).
    pub fn place(&self, hwnd: HWND, width: i32, position: &str, offset_px: i32) -> RECT {
        let rc = self.client();
        let h = rc.bottom - rc.top;
        let notify = self.notify.and_then(|n| child_rect(self.tray, n));
        let x = match position {
            "left" => rc.left + offset_px,
            _ => {
                let anchor = notify.map(|n| n.left).unwrap_or(rc.right);
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

    /// Undo the Windows 10 task-list shrink.
    pub fn restore(&self) {
        if let (Some(rebar), Some(orig)) = (self.rebar, self.rebar_original) {
            unsafe {
                let _ = MoveWindow(rebar, orig.left, orig.top, orig.right - orig.left, orig.bottom - orig.top, true);
            }
        }
    }
}
