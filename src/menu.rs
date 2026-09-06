//! Native context-menu helpers.
//!
//! * `swatch` renders a small rounded colour square that goes into the check column of a
//!   menu item (`hbmpItem`), so colour choices are visible without reading their names.
//! * `track` wraps `TrackPopupMenu` with a WH_MSGFILTER hook: a click on a "sticky" item
//!   (a toggle or a radio setting) is applied on the spot, the check marks and titles of the
//!   whole tree are refreshed and the click is swallowed, so the menu stays open. Anything
//!   else - a click outside, Escape, a non-sticky item - closes it as usual.

use crate::canvas::Color;
use std::cell::RefCell;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PWSTR;

/// Per-tracking state the hook needs. Function pointers rather than closures: the hook is a
/// plain `extern "system"` callback.
struct Track {
    owner: HWND,
    root: HMENU,
    sticky: fn(u32) -> bool,
    apply: fn(u32),
    rebuild: fn() -> Option<HMENU>,
    /// the button-down was swallowed, so swallow the matching button-up too
    swallow_up: bool,
}

thread_local! {
    static TRACK: RefCell<Option<Track>> = const { RefCell::new(None) };
    /// swatches created while a menu is up; released after it closes
    static BITMAPS: RefCell<Vec<HBITMAP>> = const { RefCell::new(Vec::new()) };
}

/// Height of the check-mark cell for `dpi`, the size a swatch should have.
fn check_size(dpi: u32) -> i32 {
    type ForDpi = unsafe extern "system" fn(i32, u32) -> i32;
    let px = match crate::util::user32_proc("GetSystemMetricsForDpi") {
        Some(p) => unsafe {
            let f: ForDpi = std::mem::transmute(p);
            f(SM_CYMENUCHECK.0, dpi)
        },
        None => unsafe { GetSystemMetrics(SM_CYMENUCHECK) },
    };
    px.max(10)
}

/// A rounded square of `c` with a slightly darker 1 px rim (keeps white visible on a light
/// menu). Premultiplied 32-bit DIB, as themed menus expect for `hbmpItem`.
pub fn swatch(c: Color, dpi: u32) -> Option<HBITMAP> {
    let s = check_size(dpi);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: s,
            biHeight: -s,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbm = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(hbm);
        }
        return None;
    }
    let px = unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, (s * s) as usize) };
    let rim = Color::rgb((c.r as u32 * 7 / 10) as u8, (c.g as u32 * 7 / 10) as u8, (c.b as u32 * 7 / 10) as u8);
    let half = s as f32 / 2.0;
    let radius = (s as f32 * 0.28).max(2.0);
    // signed distance to a rounded square centred on the bitmap, `inset` px inside the edge
    let inside = |x: f32, y: f32, inset: f32| -> bool {
        let h = half - inset - radius;
        let dx = (x - half).abs() - h;
        let dy = (y - half).abs() - h;
        let ox = dx.max(0.0);
        let oy = dy.max(0.0);
        (ox * ox + oy * oy).sqrt() + dx.max(dy).min(0.0) - radius < 0.0
    };
    const SS: i32 = 4;
    for y in 0..s {
        for x in 0..s {
            let (mut outer, mut inner) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if inside(fx, fy, 0.5) {
                        outer += 1;
                        if inside(fx, fy, 1.5) {
                            inner += 1;
                        }
                    }
                }
            }
            let total = (SS * SS) as u32;
            let a_fill = inner;
            let a_rim = outer - inner;
            let ch = |f: u8, r: u8| -> u32 { (f as u32 * a_fill + r as u32 * a_rim) / total };
            let a = outer * 255 / total;
            px[(y * s + x) as usize] = (a << 24) | (ch(c.r, rim.r) << 16) | (ch(c.g, rim.g) << 8) | ch(c.b, rim.b);
        }
    }
    BITMAPS.with(|b| b.borrow_mut().push(hbm));
    Some(hbm)
}

/// Put `bmp` into the check column of the item at `pos`.
pub unsafe fn set_item_bitmap(menu: HMENU, pos: u32, bmp: HBITMAP) {
    let info = MENUITEMINFOW { cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32, fMask: MIIM_BITMAP, hbmpItem: bmp, ..Default::default() };
    let _ = unsafe { SetMenuItemInfoW(menu, pos, TRUE, &info) };
}

/// Release every swatch made since the last call. Only after the menu is destroyed.
pub fn free_bitmaps() {
    BITMAPS.with(|b| {
        for hbm in b.borrow_mut().drain(..) {
            unsafe {
                let _ = DeleteObject(hbm);
            }
        }
    });
}

/// `TrackPopupMenu` with sticky items. `sticky(id)` says which commands keep the menu open,
/// `apply(id)` runs one, `rebuild()` returns a fresh tree reflecting the new state (its check
/// marks and titles are copied onto the open menu, then it is destroyed). Returns the command
/// chosen the ordinary way (Enter or a non-sticky click), 0 when the menu was dismissed.
pub unsafe fn track(owner: HWND, root: HMENU, pt: POINT, sticky: fn(u32) -> bool, apply: fn(u32), rebuild: fn() -> Option<HMENU>) -> u32 {
    TRACK.with(|t| *t.borrow_mut() = Some(Track { owner, root, sticky, apply, rebuild, swallow_up: false }));
    let hook = unsafe { SetWindowsHookExW(WH_MSGFILTER, Some(hook_proc), None, GetCurrentThreadId()) }.ok();
    let r = unsafe { TrackPopupMenu(root, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, owner, None) };
    if let Some(h) = hook {
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
    TRACK.with(|t| *t.borrow_mut() = None);
    r.0 as u32
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == MSGF_MENU as i32 && lparam.0 != 0 {
        let msg = unsafe { &*(lparam.0 as *const MSG) };
        if unsafe { sticky_click(msg) } {
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn is_menu_window(hwnd: HWND) -> bool {
    let mut cls = [0u16; 16];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) } as usize;
    n == 6 && cls[..n] == [b'#' as u16, b'3' as u16, b'2' as u16, b'7' as u16, b'6' as u16, b'8' as u16]
}

/// True when the message is a press on a sticky item that has just been handled here.
unsafe fn sticky_click(msg: &MSG) -> bool {
    match msg.message {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {}
        WM_LBUTTONUP | WM_RBUTTONUP => {
            return TRACK.with(|t| t.borrow_mut().as_mut().map_or(false, |t| std::mem::take(&mut t.swallow_up)));
        }
        _ => return false,
    }
    let Some((owner, root, sticky, apply, rebuild)) = TRACK.with(|t| t.borrow().as_ref().map(|t| (t.owner, t.root, t.sticky, t.apply, t.rebuild))) else {
        return false;
    };
    let pt = msg.pt;
    let hwnd = unsafe { WindowFromPoint(pt) };
    if hwnd.is_invalid() || !is_menu_window(hwnd) {
        return false;
    }
    let hmenu = HMENU(unsafe { SendMessageW(hwnd, MN_GETHMENU, WPARAM(0), LPARAM(0)) }.0 as *mut _);
    if hmenu.is_invalid() {
        return false;
    }
    let idx = unsafe { MenuItemFromPoint(owner, hmenu, pt) };
    if idx < 0 {
        return false;
    }
    let id = unsafe { GetMenuItemID(hmenu, idx) };
    if id == u32::MAX || id == 0 || !sticky(id) {
        return false;
    }
    let state = unsafe { GetMenuState(hmenu, idx as u32, MF_BYPOSITION) };
    if state == u32::MAX || state & (MF_GRAYED.0 | MF_DISABLED.0) != 0 {
        return false;
    }
    apply(id);
    if let Some(fresh) = rebuild() {
        unsafe {
            sync_tree(root, fresh);
            let _ = DestroyMenu(fresh);
            let _ = EnumThreadWindows(GetCurrentThreadId(), Some(redraw_menu_window), LPARAM(0));
        }
    }
    TRACK.with(|t| {
        if let Some(t) = t.borrow_mut().as_mut() {
            t.swallow_up = true;
        }
    });
    true
}

unsafe extern "system" fn redraw_menu_window(hwnd: HWND, _: LPARAM) -> BOOL {
    if is_menu_window(hwnd) {
        unsafe {
            let _ = RedrawWindow(hwnd, None, None, RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW);
        }
    }
    TRUE
}

/// Copy check state, title and swatch of every item from `src` onto the matching item of
/// `dst`, recursing into submenus. Items are matched by position and command id; where the
/// two trees differ the item is left alone.
unsafe fn sync_tree(dst: HMENU, src: HMENU) {
    let n = unsafe { GetMenuItemCount(dst).min(GetMenuItemCount(src)) };
    for i in 0..n {
        let (ds, ss) = unsafe { (GetSubMenu(dst, i), GetSubMenu(src, i)) };
        if ds.is_invalid() != ss.is_invalid() {
            continue;
        }
        if !ds.is_invalid() {
            unsafe { sync_tree(ds, ss) };
        } else if unsafe { GetMenuItemID(dst, i) != GetMenuItemID(src, i) } {
            continue;
        }
        let mut info = MENUITEMINFOW { cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32, fMask: MIIM_STATE | MIIM_BITMAP, ..Default::default() };
        if unsafe { GetMenuItemInfoW(src, i as u32, TRUE, &mut info) }.is_err() {
            continue;
        }
        // keep the highlight where the open menu has it
        let hilite = unsafe { GetMenuState(dst, i as u32, MF_BYPOSITION) } & MF_HILITE.0;
        let mut buf = [0u16; 256];
        let len = unsafe { GetMenuStringW(src, i as u32, Some(&mut buf), MF_BYPOSITION) };
        let mut set = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE | MIIM_BITMAP,
            fState: MENU_ITEM_STATE((info.fState.0 & !MFS_HILITE.0) | hilite),
            hbmpItem: info.hbmpItem,
            ..Default::default()
        };
        if len > 0 {
            set.fMask |= MIIM_STRING;
            set.dwTypeData = PWSTR(buf.as_mut_ptr());
        }
        let _ = unsafe { SetMenuItemInfoW(dst, i as u32, TRUE, &set) };
    }
}
