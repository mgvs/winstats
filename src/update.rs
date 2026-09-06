//! Update check against the GitHub releases of the repository: one HTTPS request through
//! WinINet (present on every Windows since 95, TLS handled by the system), done on a worker
//! thread; the result is posted to the hidden window as WM_APP_UPDATE.

use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinInet::{
    InternetCloseHandle, InternetOpenUrlW, InternetOpenW, InternetReadFile, INTERNET_FLAG_NO_CACHE_WRITE,
    INTERNET_FLAG_RELOAD, INTERNET_FLAG_SECURE, INTERNET_OPEN_TYPE_PRECONFIG,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::util::{pcwstr, wide};

pub const RELEASES_URL: &str = "https://github.com/mgvs/winstats/releases/latest";
const API_URL: &str = "https://api.github.com/repos/mgvs/winstats/releases/latest";

/// Posted to the hidden window when a check finishes; wparam = 1 for a manual check.
pub const WM_APP_UPDATE: u32 = WM_APP + 1;

/// Outcome of the last check, taken by the UI thread on WM_APP_UPDATE.
static RESULT: Mutex<Option<Result<String, String>>> = Mutex::new(None);

pub fn take_result() -> Option<Result<String, String>> {
    RESULT.lock().ok().and_then(|mut r| r.take())
}

/// Start a check on a worker thread. `manual` decides how the result is shown.
pub fn check_async(hidden: HWND, manual: bool) {
    let hwnd = hidden.0 as isize;
    std::thread::spawn(move || {
        let result = fetch_latest();
        if let Ok(mut r) = RESULT.lock() {
            *r = Some(result);
        }
        unsafe {
            let _ = PostMessageW(HWND(hwnd as *mut _), WM_APP_UPDATE, WPARAM(manual as usize), LPARAM(0));
        }
    });
}

/// Latest release version ("0.1.5") from the GitHub API.
pub fn fetch_latest() -> Result<String, String> {
    let body = http_get(API_URL)?;
    let tag = json_string(&body, "tag_name").ok_or_else(|| String::from("no tag_name in the response"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// `latest` is a higher version than `current`, comparing dotted numbers.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect() };
    let (a, b) = (parse(latest), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Value of the first `"key":"..."` in a JSON document; enough for the one field we read.
fn json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = json.find(&needle)? + needle.len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn http_get(url: &str) -> Result<String, String> {
    unsafe {
        let agent = wide(&format!("winstats/{}", crate::about::VERSION));
        let session = InternetOpenW(pcwstr(&agent), INTERNET_OPEN_TYPE_PRECONFIG.0, PCWSTR::null(), PCWSTR::null(), 0);
        if session.is_null() {
            return Err(last_error());
        }
        let url_w = wide(url);
        let headers = wide("Accept: application/vnd.github+json\r\n");
        let flags = INTERNET_FLAG_RELOAD | INTERNET_FLAG_NO_CACHE_WRITE | INTERNET_FLAG_SECURE;
        let file = InternetOpenUrlW(session, pcwstr(&url_w), Some(&headers[..headers.len() - 1]), flags, 0);
        if file.is_null() {
            let e = last_error();
            let _ = InternetCloseHandle(session);
            return Err(e);
        }
        let mut body = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut got = 0u32;
            if InternetReadFile(file, chunk.as_mut_ptr() as *mut _, chunk.len() as u32, &mut got).is_err() {
                let e = last_error();
                let _ = InternetCloseHandle(file);
                let _ = InternetCloseHandle(session);
                return Err(e);
            }
            if got == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..got as usize]);
            if body.len() > 1 << 20 {
                break;
            }
        }
        let _ = InternetCloseHandle(file);
        let _ = InternetCloseHandle(session);
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

fn last_error() -> String {
    let e = windows::core::Error::from_win32();
    let msg = e.message();
    if msg.is_empty() {
        format!("error {}", e.code().0)
    } else {
        msg.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.1.5", "0.1.4"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.4", "0.1.4"));
        assert!(!is_newer("0.1.3", "0.1.4"));
    }

    #[test]
    #[ignore]
    fn live_fetch() {
        let v = fetch_latest().expect("fetch");
        println!("latest = {v}");
        assert!(v.starts_with("0."));
    }

    #[test]
    fn tag_from_json() {
        let j = r#"{"url":"x","tag_name": "v0.1.4","name":"winstats 0.1.4"}"#;
        assert_eq!(json_string(j, "tag_name").as_deref(), Some("v0.1.4"));
    }
}
