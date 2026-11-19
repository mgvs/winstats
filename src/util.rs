//! Small helpers: wide strings, registry reads, OS version, theme detection, logging.

use std::io::Write;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::core::PCWSTR;

/// Null-terminated UTF-16 buffer.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR::from_raw(buf.as_ptr())
}

pub fn reg_dword_hkcu(subkey: &str, value: &str) -> Option<u32> {
    let k = wide(subkey);
    let v = wide(value);
    let mut data: u32 = 0;
    let mut size: u32 = 4;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            pcwstr(&k),
            pcwstr(&v),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    if r == ERROR_SUCCESS { Some(data) } else { None }
}

pub fn reg_sz_hklm(subkey: &str, value: &str) -> Option<String> {
    let k = wide(subkey);
    let v = wide(value);
    let mut buf = [0u16; 512];
    let mut size: u32 = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            pcwstr(&k),
            pcwstr(&v),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    Some(String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string())
}

pub fn windows_build() -> u32 {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = RtlGetVersion(&mut info);
    }
    info.dwBuildNumber
}

/// Load a DLL from System32 by its full path (vendor libraries the drivers install there):
/// never picked up from the working directory or PATH. None when it is not there.
pub fn load_system_dll(name: PCWSTR) -> Option<windows::Win32::Foundation::HMODULE> {
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut dir = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut dir)) } as usize;
    if n == 0 || n >= dir.len() {
        return None;
    }
    let name = unsafe { name.as_wide() };
    let mut path: Vec<u16> = dir[..n].to_vec();
    path.push(b'\\' as u16);
    path.extend_from_slice(name);
    path.push(0);
    unsafe { LoadLibraryW(PCWSTR(path.as_ptr())).ok() }
}

/// Open a URI (ms-settings:..., https://...) with its registered handler.
pub fn open_uri(uri: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (op, target) = (wide("open"), wide(uri));
    let r = unsafe { ShellExecuteW(windows::Win32::Foundation::HWND::default(), pcwstr(&op), pcwstr(&target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if r.0 as isize <= 32 {
        log(&format!("open {uri} failed: {}", r.0 as isize));
    }
}

/// Windows "Animation effects" (Accessibility > Visual effects) is on.
pub fn animations_enabled() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};
    let mut on = windows::Win32::Foundation::BOOL(1);
    unsafe {
        if SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut on as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_err() {
            return true;
        }
    }
    on.as_bool()
}

/// Windows 8 is build 9200; anything older cannot host layered child windows.
pub fn is_windows7() -> bool {
    windows_build() < 9200
}

pub fn user32_proc(name: &str) -> Option<unsafe extern "system" fn() -> isize> {
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    let module = unsafe { GetModuleHandleW(windows::core::w!("user32.dll")) }.ok()?;
    let cname = std::ffi::CString::new(name).ok()?;
    unsafe { GetProcAddress(module, windows::core::PCSTR::from_raw(cname.as_ptr() as *const u8)) }
}

/// Per-monitor-v2 DPI awareness (Windows 10 1607+), resolved at run time so the binary still
/// loads on Windows 7/8 where the export does not exist. Returns false when unavailable.
pub fn set_per_monitor_dpi() -> bool {
    type SetCtx = unsafe extern "system" fn(*mut core::ffi::c_void) -> i32;
    let Some(p) = user32_proc("SetProcessDpiAwarenessContext") else { return false };
    // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 == (HANDLE)-4
    unsafe {
        let f: SetCtx = std::mem::transmute(p);
        f(-4isize as *mut core::ffi::c_void) != 0
    }
}

/// GetDpiForWindow (Windows 10 1607+), or None on older systems.
pub fn dpi_for_window(hwnd: windows::Win32::Foundation::HWND) -> Option<u32> {
    type GetDpi = unsafe extern "system" fn(windows::Win32::Foundation::HWND) -> u32;
    let p = user32_proc("GetDpiForWindow")?;
    let d = unsafe {
        let f: GetDpi = std::mem::transmute(p);
        f(hwnd)
    };
    if d == 0 { None } else { Some(d) }
}

pub fn is_windows11() -> bool {
    windows_build() >= 22000
}

/// True when the system (taskbar) theme is light.
pub fn system_light_theme() -> bool {
    reg_dword_hkcu(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "SystemUsesLightTheme",
    ) == Some(1)
}

/// Windows 11: 1 = taskbar centered (default), 0 = left aligned.

pub fn config_dir() -> std::path::PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("winstats")
}

pub fn log(msg: &str) {
    eprintln!("{msg}");
    // unit tests must not write into the real log
    if cfg!(test) {
        return;
    }
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("winstats.log"))
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::util::log(&format!($($arg)*)) };
}

/// A unit symbol in the UI language ("GB", "ГБ", "Go"): the `[units]` table of the locale.
pub fn unit(key: &str) -> String {
    let full = format!("units.{key}");
    let v = crate::i18n::t(&full);
    if v != full {
        return v;
    }
    // no locale loaded (tests) or a missing key: the English unit
    match key {
        "b" => "B",
        "kb" => "KB",
        "mb" => "MB",
        "gb" => "GB",
        "tb" => "TB",
        "per_s" => "/s",
        "mbit" => "Mbit",
        "gbit" => "Gbit",
        "mhz" => "MHz",
        "ghz" => "GHz",
        "w" => "W",
        "wh" => "Wh",
        "rpm" => "RPM",
        "ms" => "ms",
        "h" => "h",
        "min" => "min",
        _ => key,
    }
    .to_string()
}

pub fn format_bytes_rate(bps: f64) -> String {
    let b = bps.max(0.0);
    let per = unit("per_s");
    if b < 1024.0 {
        format!("{:.0} {}{per}", b, unit("b"))
    } else if b < 1024.0 * 1024.0 {
        format!("{:.0} {}{per}", b / 1024.0, unit("kb"))
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} {}{per}", b / 1024.0 / 1024.0, unit("mb"))
    } else {
        format!("{:.2} {}{per}", b / 1024.0 / 1024.0 / 1024.0, unit("gb"))
    }
}

/// Metrics stay in bytes per second; only the network display converts to bits.
pub fn format_net_rate(bytes_per_sec: f64, bits: bool) -> String {
    if !bits {
        return format_bytes_rate(bytes_per_sec);
    }
    let b = bytes_per_sec.max(0.0) * 8.0;
    if b < 1_000.0 {
        format!("{:.0} b/s", b)
    } else if b < 1_000_000.0 {
        format!("{:.0} Kb/s", b / 1_000.0)
    } else if b < 1_000_000_000.0 {
        format!("{:.1} Mb/s", b / 1_000_000.0)
    } else {
        format!("{:.2} Gb/s", b / 1_000_000_000.0)
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn network_bits_use_decimal_units_and_convert_bytes() {
        for (bytes, expected) in [
            (-1.0, "0 b/s"),
            (0.0, "0 b/s"),
            (1.0, "8 b/s"),
            (124.875, "999 b/s"),
            (125.0, "1 Kb/s"),
            (125_000.0 - 1.0, "1000 Kb/s"),
            (125_000.0, "1.0 Mb/s"),
            (1_250_000.0, "10.0 Mb/s"),
            (125_000_000.0 - 1.0, "1000.0 Mb/s"),
            (125_000_000.0, "1.00 Gb/s"),
            (1_250_000_000.0, "10.00 Gb/s"),
        ] {
            assert_eq!(format_net_rate(bytes, true), expected, "{bytes} bytes/s");
        }
    }

    #[test]
    fn network_bytes_keep_existing_binary_units() {
        for (bytes, expected) in [
            (-1.0, "0 B/s"),
            (0.0, "0 B/s"),
            (1.0, "1 B/s"),
            (1023.0, "1023 B/s"),
            (1024.0, "1 KB/s"),
            (1_048_576.0, "1.0 MB/s"),
            (1_073_741_824.0, "1.00 GB/s"),
        ] {
            assert_eq!(format_net_rate(bytes, false), expected, "{bytes} bytes/s");
        }
    }
}

pub fn format_bytes(b: u64) -> String {
    let f = b as f64;
    const K: f64 = 1024.0;
    if f < K * K {
        format!("{:.0} {}", f / K, unit("kb"))
    } else if f < K * K * K {
        format!("{:.0} {}", f / K / K, unit("mb"))
    } else if f < K * K * K * K {
        format!("{:.1} {}", f / K / K / K, unit("gb"))
    } else {
        format!("{:.2} {}", f / K / K / K / K, unit("tb"))
    }
}

pub fn format_duration(secs: u32) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 { format!("{h} {} {m:02} {}", unit("h"), unit("min")) } else { format!("{m} {}", unit("min")) }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "winstats";

/// True when the HKCU Run entry points at this executable.
pub fn autostart_enabled() -> bool {
    let k = wide(RUN_KEY);
    let v = wide(RUN_VALUE);
    let mut buf = [0u16; 1024];
    let mut size: u32 = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, pcwstr(&k), pcwstr(&v), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size))
    };
    r == ERROR_SUCCESS
}

pub fn set_autostart(enable: bool) -> bool {
    let k = wide(RUN_KEY);
    let v = wide(RUN_VALUE);
    unsafe {
        if enable {
            let Ok(exe) = std::env::current_exe() else { return false };
            let data = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(HKEY_CURRENT_USER, pcwstr(&k), pcwstr(&v), REG_SZ.0, Some(data.as_ptr() as *const _), (data.len() * 2) as u32) == ERROR_SUCCESS
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, pcwstr(&k), pcwstr(&v)) == ERROR_SUCCESS
        }
    }
}

/// "18.85 GB" / "512 MB", Stats-style size with two decimals for gigabytes.
pub fn format_size(b: u64) -> String {
    let f = b as f64;
    const K: f64 = 1024.0;
    if f < K * K * K {
        format!("{:.0} {}", f / K / K, unit("mb"))
    } else if f < K * K * K * K {
        format!("{:.2} {}", f / K / K / K, unit("gb"))
    } else {
        format!("{:.2} {}", f / K / K / K / K, unit("tb"))
    }
}

/// The accent shade Windows itself uses for accent fills on this background (Fluent
/// AccentFillColorDefault): "Light 2" on dark, "Dark 1" on light, from the accent palette the
/// shell derives from the accent colour. A deep accent stays itself, just lighter or darker,
/// instead of being replaced by grey.
pub fn system_accent_for(light_bg: bool) -> Option<crate::canvas::Color> {
    use windows::Win32::System::Registry::RRF_RT_REG_BINARY;
    let (k, v) = (wide(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent"), wide("AccentPalette"));
    let mut buf = [0u8; 32];
    let mut size = buf.len() as u32;
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, pcwstr(&k), pcwstr(&v), RRF_RT_REG_BINARY, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size)) } == ERROR_SUCCESS;
    if !ok || size < 32 {
        return None;
    }
    // 8 RGBA entries: Light 3, Light 2, Light 1, Base, Dark 1, Dark 2, Dark 3, unused
    let i = if light_bg { 4 } else { 1 } * 4;
    Some(crate::canvas::Color::rgb(buf[i], buf[i + 1], buf[i + 2]))
}

/// Windows accent colour (DWM colorization), if available.
pub fn system_accent_color() -> Option<crate::canvas::Color> {
    let mut argb: u32 = 0;
    let mut opaque = windows::Win32::Foundation::BOOL(0);
    unsafe { windows::Win32::Graphics::Dwm::DwmGetColorizationColor(&mut argb, &mut opaque).ok()? };
    Some(crate::canvas::Color::rgb((argb >> 16) as u8, (argb >> 8) as u8, argb as u8))
}

/// Put `text` on the clipboard as Unicode text.
pub fn set_clipboard(text: &str) -> bool {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return false;
        }
        let mut ok = false;
        if EmptyClipboard().is_ok() {
            if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
                let p = GlobalLock(h) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    let _ = GlobalUnlock(h);
                    ok = SetClipboardData(CF_UNICODETEXT, HANDLE(h.0)).is_ok();
                }
                if !ok {
                    let _ = GlobalFree(HGLOBAL(h.0));
                }
            }
        }
        let _ = CloseClipboard();
        ok
    }
}

/// Wall-clock seconds since 1970.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
