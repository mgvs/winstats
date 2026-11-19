//! What the screens are doing, for the power model: which built-in panels are active, whether
//! the console display is on, off or dimmed, the brightness setting, and how bright the picture
//! on each panel is (OLED power follows the picture as much as the brightness).
//!
//! All of it is read without elevation. In a Remote Desktop session Windows describes the RDP
//! displays, not the machine's panels: `remote` marks that, and such samples must not be used.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use windows::core::GUID;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Gdi::*;

/// One built-in panel with an active display path.
#[derive(Clone, Debug, PartialEq)]
pub struct Panel {
    /// stable identity: EDID code and the device instance UID, e.g. "ABC1234#UID12345"
    pub id: String,
    /// position on the desktop (top-left), to tell the upper from the lower panel
    pub x: i32,
    pub y: i32,
    /// mean linear luminance of the picture, 0..1 (None when not sampled)
    pub apl: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct DisplayState {
    /// a Remote Desktop session: the panel data below is not the machine's
    pub remote: bool,
    /// console display: 0 off, 1 on, 2 dimmed
    pub console: u8,
    /// built-in panels with an active path, upper first
    pub panels: Vec<Panel>,
    /// active displays that are not built in
    pub external: usize,
    /// brightness setting of the built-in display, 0..100 (power scheme value)
    pub brightness: Option<u32>,
}

impl DisplayState {
    /// The built-in panels actually lit: active and the console display not switched off.
    pub fn lit(&self) -> impl Iterator<Item = &Panel> {
        let on = self.console != 0;
        self.panels.iter().filter(move |_| on)
    }
}

// ---------------------------------------------------------------- console display state

/// GUID_CONSOLE_DISPLAY_STATE {6FE69556-704A-47A0-8F24-C28D936FDA47}
const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);
const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
static CONSOLE: AtomicU8 = AtomicU8::new(1);

unsafe extern "system" fn on_display_state(_ctx: *const std::ffi::c_void, kind: u32, setting: *const std::ffi::c_void) -> u32 {
    if kind == PBT_POWERSETTINGCHANGE && !setting.is_null() {
        let s = &*(setting as *const windows::Win32::System::Power::POWERBROADCAST_SETTING);
        if s.PowerSetting == GUID_CONSOLE_DISPLAY_STATE && s.DataLength >= 1 {
            CONSOLE.store(*s.Data.as_ptr(), Ordering::Relaxed);
        }
    }
    0
}

/// Subscribe once to console display on / off / dimmed changes (the first callback reports the
/// current state).
fn watch_console() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        use windows::Win32::System::Power::{PowerSettingRegisterNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS};
        use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK;
        // lives as long as the process
        let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS { Callback: Some(on_display_state), Context: std::ptr::null_mut() }));
        let mut handle = std::ptr::null_mut();
        let r = PowerSettingRegisterNotification(&GUID_CONSOLE_DISPLAY_STATE, DEVICE_NOTIFY_CALLBACK, HANDLE(params as *mut _ as *mut _), &mut handle);
        if r.0 != 0 {
            crate::log!("displays: console state notification failed: {}", r.0);
        }
    });
}

// ---------------------------------------------------------------- brightness

fn brightness(on_ac: bool) -> Option<u32> {
    use windows::Win32::System::Power::{PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex};
    // GUID_VIDEO_SUBGROUP, VIDEO_NORMAL_LEVEL (the brightness slider of the active scheme)
    const VIDEO: GUID = GUID::from_u128(0x7516b95f_f776_4464_8c53_06167f40cc99);
    const LEVEL: GUID = GUID::from_u128(0xaded5e82_b909_4619_9949_f5d71dac0bcb);
    unsafe {
        let mut scheme: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut scheme).0 != 0 || scheme.is_null() {
            return None;
        }
        let mut v = 0u32;
        let ok = if on_ac {
            PowerReadACValueIndex(None, Some(scheme), Some(&VIDEO), Some(&LEVEL), &mut v).0 == 0
        } else {
            PowerReadDCValueIndex(None, Some(scheme), Some(&VIDEO), Some(&LEVEL), &mut v) == 0
        };
        let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(scheme as *mut _));
        (ok && v <= 100).then_some(v)
    }
}

// ---------------------------------------------------------------- topology

fn wide_str(w: &[u16]) -> String {
    let end = w.iter().position(|c| *c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// "\\?\DISPLAY#ABC1234#4&1a2b3c4&0&UID12345#{...}" -> "ABC1234#UID12345"
pub fn panel_id(device_path: &str) -> String {
    let parts: Vec<&str> = device_path.split('#').collect();
    let code = parts.get(1).cloned().unwrap_or("");
    let uid = parts.get(2).and_then(|s| s.split('&').find(|p| p.starts_with("UID"))).unwrap_or("");
    if uid.is_empty() {
        code.to_string()
    } else {
        format!("{code}#{uid}")
    }
}

fn is_internal(tech: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> bool {
    tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED || tech == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
}

/// Active paths: (internal panels with their desktop rectangles, external count).
fn topology() -> Option<(Vec<(Panel, (i32, i32, i32, i32))>, usize)> {
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).0 != 0 {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).0 != 0 {
            return None;
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        let mut internal = Vec::new();
        let mut external = 0;
        for p in &paths {
            let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            name.header.adapterId = p.targetInfo.adapterId;
            name.header.id = p.targetInfo.id;
            let tech = if DisplayConfigGetDeviceInfo(&mut name.header) == 0 { name.outputTechnology } else { p.targetInfo.outputTechnology };
            if !is_internal(tech) {
                external += 1;
                continue;
            }
            let src = &p.sourceInfo;
            let rect = modes
                .iter()
                .find(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE && m.id == src.id && m.adapterId.LowPart == src.adapterId.LowPart && m.adapterId.HighPart == src.adapterId.HighPart)
                .map(|m| {
                    let s = m.Anonymous.sourceMode;
                    (s.position.x, s.position.y, s.width as i32, s.height as i32)
                })
                .unwrap_or((0, 0, 0, 0));
            let id = panel_id(&wide_str(&name.monitorDevicePath));
            internal.push((Panel { id, x: rect.0, y: rect.1, apl: None }, rect));
        }
        internal.sort_by_key(|(p, _)| (p.y, p.x));
        Some((internal, external))
    }
}

// ---------------------------------------------------------------- picture level

/// Mean linear luminance of a desktop rectangle, from a 32 x 20 halftoned copy.
fn picture_level(rect: (i32, i32, i32, i32)) -> Option<f32> {
    let (x, y, w, h) = rect;
    if w <= 0 || h <= 0 {
        return None;
    }
    const W: i32 = 32;
    const H: i32 = 20;
    unsafe {
        let screen = GetDC(HWND::default());
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let bmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: W, biHeight: -H, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let mut out = None;
        if let Ok(dib) = dib {
            let old = SelectObject(mem, dib);
            SetStretchBltMode(mem, HALFTONE);
            let _ = SetBrushOrgEx(mem, 0, 0, None);
            if StretchBlt(mem, 0, 0, W, H, screen, x, y, w, h, SRCCOPY).as_bool() && !bits.is_null() {
                let px = std::slice::from_raw_parts(bits as *const u32, (W * H) as usize);
                let lin = |v: u32| (v as f32 / 255.0).powf(2.2);
                let sum: f32 = px.iter().map(|p| 0.2126 * lin((p >> 16) & 0xFF) + 0.7152 * lin((p >> 8) & 0xFF) + 0.0722 * lin(p & 0xFF)).sum();
                out = Some(sum / px.len() as f32);
            }
            SelectObject(mem, old);
            let _ = DeleteObject(dib);
        }
        let _ = DeleteDC(mem);
        ReleaseDC(HWND::default(), screen);
        out
    }
}

/// The current display state; the picture is sampled only on lit built-in panels and never in a
/// Remote Desktop session.
pub fn read(on_ac: bool, sample_picture: bool) -> DisplayState {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    watch_console();
    let remote = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
    let console = CONSOLE.load(Ordering::Relaxed);
    let mut st = DisplayState { remote, console, brightness: brightness(on_ac), ..Default::default() };
    if let Some((internal, external)) = topology() {
        st.external = external;
        for (mut p, rect) in internal {
            if sample_picture && !remote && console != 0 {
                p.apl = picture_level(rect);
            }
            st.panels.push(p);
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_ids_keep_code_and_uid() {
        assert_eq!(panel_id(r"\\?\DISPLAY#ABC1234#4&1a2b3c4&0&UID12345#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"), "ABC1234#UID12345");
        assert_eq!(panel_id(r"\\?\DISPLAY#ABC1234#xyz"), "ABC1234");
    }

    /// `cargo test display_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn display_probe() {
        let _ = read(true, false);
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("{:#?}", read(true, true));
    }
}
