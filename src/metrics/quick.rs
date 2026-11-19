//! Windows quick settings from the popups, all without elevation: keep the PC awake and the
//! screen-off timeouts of the active power plan (on AC and on battery).

use crate::util::{pcwstr, wide};
use windows::core::GUID;

/// Screen-off choices offered, in seconds (0 = never).
pub const SCREEN_CHOICES: [u32; 7] = [60, 180, 300, 600, 900, 1800, 0];

/// Power-plan setting "Turn off display after" (VIDEOIDLE) in the display subgroup.
const SUB_VIDEO: GUID = GUID::from_u128(0x7516b95f_f776_4464_8c53_06167f40cc99);
const VIDEO_IDLE: GUID = GUID::from_u128(0x3c0bc021_c8a8_4e07_a973_6b14cbcb2b7e);

/// Keep the PC and the screen awake while on (ES_CONTINUOUS on the calling thread: call it from
/// the UI thread, which lives as long as WinStats).
pub fn set_keep_awake(on: bool) -> bool {
    use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};
    let flags = if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED } else { ES_CONTINUOUS };
    let prev = unsafe { SetThreadExecutionState(flags) };
    crate::log!("keep awake: {}", if on { "on" } else { "off" });
    prev.0 != 0
}

type GetActive = unsafe extern "system" fn(isize, *mut *mut GUID) -> u32;
type ReadIndex = unsafe extern "system" fn(isize, *const GUID, *const GUID, *const GUID, *mut u32) -> u32;
type WriteIndex = unsafe extern "system" fn(isize, *const GUID, *const GUID, *const GUID, u32) -> u32;
type SetActive = unsafe extern "system" fn(isize, *const GUID) -> u32;

fn powrprof(name: &str) -> Option<unsafe extern "system" fn() -> isize> {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let dll = wide("powrprof.dll");
    let lib = unsafe { LoadLibraryW(pcwstr(&dll)).ok()? };
    let n = std::ffi::CString::new(name).ok()?;
    unsafe { GetProcAddress(lib, windows::core::PCSTR(n.as_ptr() as *const u8)) }
}

/// The active power plan's GUID.
fn active_scheme() -> Option<GUID> {
    let get: GetActive = unsafe { std::mem::transmute(powrprof("PowerGetActiveScheme")?) };
    let mut p: *mut GUID = std::ptr::null_mut();
    if unsafe { get(0, &mut p) } != 0 || p.is_null() {
        return None;
    }
    let g = unsafe { *p };
    unsafe {
        let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(p as *mut _));
    }
    Some(g)
}

/// Screen-off timeouts of the active plan in seconds (0 = never): (on AC, on battery).
pub fn screen_timeouts() -> Option<(u32, u32)> {
    let scheme = active_scheme()?;
    let ac: ReadIndex = unsafe { std::mem::transmute(powrprof("PowerReadACValueIndex")?) };
    let dc: ReadIndex = unsafe { std::mem::transmute(powrprof("PowerReadDCValueIndex")?) };
    let (mut a, mut d) = (0u32, 0u32);
    unsafe {
        if ac(0, &scheme, &SUB_VIDEO, &VIDEO_IDLE, &mut a) != 0 || dc(0, &scheme, &SUB_VIDEO, &VIDEO_IDLE, &mut d) != 0 {
            return None;
        }
    }
    Some((a, d))
}

/// Set the screen-off timeout of the active plan for AC or battery, and apply it.
pub fn set_screen_timeout(on_ac: bool, secs: u32) -> bool {
    let Some(scheme) = active_scheme() else { return false };
    let name = if on_ac { "PowerWriteACValueIndex" } else { "PowerWriteDCValueIndex" };
    let (Some(w), Some(s)) = (powrprof(name), powrprof("PowerSetActiveScheme")) else { return false };
    let write: WriteIndex = unsafe { std::mem::transmute(w) };
    let set: SetActive = unsafe { std::mem::transmute(s) };
    let r = unsafe { write(0, &scheme, &SUB_VIDEO, &VIDEO_IDLE, secs) };
    // re-activating the plan makes Windows pick the new value up at once
    let a = if r == 0 { unsafe { set(0, &scheme) } } else { r };
    crate::log!("screen off {} -> {secs} s: {}", if on_ac { "AC" } else { "DC" }, if a == 0 { "ok".to_string() } else { format!("error {a}") });
    a == 0
}

/// Minutes as the screen-off buttons label them ("Never" is the caller's).
pub fn minutes(secs: u32) -> u32 {
    (secs + 59) / 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_are_whole_minutes_and_never() {
        assert_eq!(SCREEN_CHOICES.iter().map(|s| minutes(*s)).collect::<Vec<_>>(), vec![1, 3, 5, 10, 15, 30, 0]);
    }

    /// `cargo test quick_probe -- --ignored --nocapture`: read-only.
    #[test]
    #[ignore]
    fn quick_probe() {
        println!("screen off (AC, DC) = {:?}", screen_timeouts());
    }
}
