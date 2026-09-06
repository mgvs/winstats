//! About window: a small centred layered window with the logo, name, version, copyright and links.

use crate::canvas::{Canvas, Color, FW_NORMAL, FW_SEMIBOLD, FontSpec};
use crate::util::{pcwstr, wide};
use crate::widgets::Theme;
use windows::Win32::Foundation::{HINSTANCE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

pub const NAME: &str = "winstats";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const AUTHOR: &str = "Oleksandr Zhabotynskyi";
pub const REPO_URL: &str = "https://github.com/mgvs/winstats";
pub const LICENSE_URL: &str = "https://github.com/mgvs/winstats/blob/main/LICENSE";

const W_DIP: f32 = 320.0;
const H_DIP: f32 = 250.0;

pub struct About {
    pub hwnd: HWND,
    canvas: Canvas,
    pub visible: bool,
    /// clickable areas in window coords: (rect, url); an empty url is the close button
    links: Vec<(RECT, &'static str)>,
}

impl About {
    pub fn new(hinstance: HINSTANCE, class: &str) -> Option<Self> {
        let cls = wide(class);
        let title = wide("About winstats");
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
        Some(Self { hwnd, canvas: Canvas::new(), visible: false, links: Vec::new() })
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }

    /// True when (x, y) in window coords is over a link (not the close button).
    pub fn is_link_at(&self, x: i32, y: i32) -> bool {
        self.links.iter().any(|(r, u)| !u.is_empty() && x >= r.left && x < r.right && y >= r.top && y < r.bottom)
    }

    /// Click at window coords: open a link or close.
    pub fn click(&mut self, x: i32, y: i32) {
        let hit = self.links.iter().find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom).map(|(_, u)| *u);
        match hit {
            Some("") => self.hide(),
            Some(url) => {
                let verb = wide("open");
                let u = wide(url);
                unsafe {
                    ShellExecuteW(None, pcwstr(&verb), pcwstr(&u), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                }
            }
            None => {}
        }
    }

    pub fn show(&mut self, t: &Theme, light: bool) {
        let w = t.s(W_DIP);
        let h = t.s(H_DIP);
        self.canvas.resize(w, h);
        self.canvas.clear();
        self.links.clear();

        let bg = if light { Color::rgba(0xF9, 0xF9, 0xF9, 250) } else { Color::rgba(0x20, 0x20, 0x20, 250) };
        let r = t.s(10.0) as f32;
        self.canvas.fill_round_rect(0, 0, w, h, r, bg);
        self.canvas.stroke_round_rect_r(0, 0, w, h, r, t.dim.alpha(0.35));

        // close button
        let cf = FontSpec { px: t.s(13.0), weight: FW_NORMAL };
        let (cw, ch) = self.canvas.measure_text("\u{2715}", cf);
        let cx = w - t.s(14.0) - cw;
        let cy = t.s(10.0);
        self.canvas.draw_text("\u{2715}", cx, cy, cf, t.dim);
        self.links.push((RECT { left: cx - t.s(8.0), top: cy - t.s(8.0), right: cx + cw + t.s(8.0), bottom: cy + ch + t.s(8.0) }, ""));

        // logo: rounded square in the accent colour with white core bars
        let logo = t.s(64.0);
        let lx = (w - logo) / 2;
        let ly = t.s(26.0);
        self.canvas.fill_round_rect(lx, ly, logo, logo, t.s(14.0) as f32, t.accent);
        // centred group of bars with chart-like, uneven levels
        let bars = [0.45f32, 0.8, 0.35, 0.95, 0.6, 0.25, 0.7];
        let pad = t.s(12.0);
        let gap = t.s(2.0);
        let n = bars.len() as i32;
        let bw = (logo - pad * 2 - gap * (n - 1)) / n;
        let total = bw * n + gap * (n - 1);
        let x0 = lx + (logo - total) / 2;
        let max_h = (logo - pad * 2) as f32;
        for (i, v) in bars.iter().enumerate() {
            let bx = x0 + i as i32 * (bw + gap);
            self.canvas.fill_rect(bx, ly + pad, bw, max_h as i32, Color::rgba(255, 255, 255, 70));
            self.canvas.fill_bar(bx, ly + logo - pad, bw, v * max_h, Color::rgb(255, 255, 255));
        }

        let mut y = ly + logo + t.s(16.0);
        let centred = |c: &mut Canvas, text: &str, f: FontSpec, col: Color, y: &mut i32| -> RECT {
            let (tw, th) = c.measure_text(text, f);
            let x = (w - tw) / 2;
            c.draw_text(text, x, *y, f, col);
            let rc = RECT { left: x, top: *y, right: x + tw, bottom: *y + th };
            *y += th;
            rc
        };

        centred(&mut self.canvas, NAME, FontSpec { px: t.s(20.0), weight: FW_SEMIBOLD }, t.text, &mut y);
        y += t.s(6.0);
        centred(&mut self.canvas, &crate::i18n::tf("about.version", &[("v", VERSION)]), FontSpec { px: t.s(11.0), weight: FW_NORMAL }, t.text, &mut y);
        y += t.s(4.0);
        centred(&mut self.canvas, &format!("\u{00A9} {AUTHOR} {}", year()), FontSpec { px: t.s(11.0), weight: FW_NORMAL }, t.dim, &mut y);
        y += t.s(14.0);
        let lf = FontSpec { px: t.s(11.0), weight: FW_NORMAL };
        let rc = centred(&mut self.canvas, "github.com/mgvs/winstats", lf, t.accent, &mut y);
        self.links.push((pad_rect(rc, t.s(6.0)), REPO_URL));
        y += t.s(4.0);
        let rc = centred(&mut self.canvas, &crate::i18n::t("about.license"), lf, t.accent, &mut y);
        self.links.push((pad_rect(rc, t.s(6.0)), LICENSE_URL));

        // centre on the primary monitor
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mon = unsafe {
            let hm = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTOPRIMARY);
            let _ = GetMonitorInfoW(hm, &mut mi);
            mi.rcWork
        };
        let x = mon.left + (mon.right - mon.left - w) / 2;
        let yy = mon.top + (mon.bottom - mon.top - h) / 2;
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, x, yy, w, h, SWP_SHOWWINDOW);
            if let Err(e) = self.canvas.present_at(self.hwnd, x, yy) {
                crate::log!("about UpdateLayeredWindow failed: {e}");
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.visible = true;
    }
}

fn pad_rect(r: RECT, p: i32) -> RECT {
    RECT { left: r.left - p, top: r.top - p, right: r.right + p, bottom: r.bottom + p }
}

fn year() -> u16 {
    unsafe { GetLocalTime().wYear }
}
