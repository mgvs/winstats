//! A small modal text prompt (a remote machine's label): a line of text, an edit box, OK and
//! Cancel, built from plain controls so no dialog resource is needed. Enter, Esc and Tab work
//! through IsDialogMessage; the window opens just above the point it is given (the taskbar).

use std::cell::RefCell;
use std::sync::Once;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetMonitorInfoW, GetSysColorBrush, MonitorFromPoint, SetBkMode, CLIP_DEFAULT_PRECIS, COLOR_WINDOW,
    DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS,
    CLEARTYPE_QUALITY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_SETLIMITTEXT, EM_SETSEL};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::util::{pcwstr, wide};

const ID_EDIT: i32 = 100;
const CLASS: &str = "WinStatsPrompt";

thread_local! {
    /// what the open prompt ended with: Some(text) on OK, None on Cancel / close
    static RESULT: RefCell<Option<String>> = const { RefCell::new(None) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            if id == IDOK.0 {
                let edit = unsafe { GetDlgItem(hwnd, ID_EDIT) }.unwrap_or_default();
                let mut buf = [0u16; 64];
                let n = unsafe { GetWindowTextW(edit, &mut buf) } as usize;
                let text = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
                RESULT.with(|r| *r.borrow_mut() = Some(text));
                let _ = unsafe { DestroyWindow(hwnd) };
            } else if id == IDCANCEL.0 {
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        // the prompt line on the window colour rather than the grey of a dialog
        WM_CTLCOLORSTATIC => {
            unsafe {
                SetBkMode(HDC(wparam.0 as *mut _), TRANSPARENT);
            }
            LRESULT(unsafe { GetSysColorBrush(COLOR_WINDOW) }.0 as isize)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let cls = wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default(),
            lpszClassName: pcwstr(&cls),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };
        std::mem::forget(cls);
        RegisterClassExW(&wc);
    });
}

/// Make the prompt the foreground window. A plain SetForegroundWindow can be refused once the
/// menu that led here has closed; joining the input of the current foreground thread for a
/// moment lets it through.
unsafe fn take_focus(hwnd: HWND) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    unsafe {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return;
        }
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me {
            let _ = AttachThreadInput(me, fg_thread, true);
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
}

/// Ask for a line of text of at most `max_len` characters. `dpi` scales the layout; `above` is
/// the point the window sits just above (centred on its x). None when the user cancels.
pub fn prompt(caption: &str, text: &str, initial: &str, max_len: usize, dpi: u32, above: POINT, ok: &str, cancel: &str) -> Option<String> {
    register();
    let s = |dip: i32| (dip as f32 * dpi as f32 / 96.0).round() as i32;
    let (cw, ch) = (s(360), s(160));
    unsafe {
        let hinstance: windows::Win32::Foundation::HINSTANCE = GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default();
        let cls = wide(CLASS);
        let cap = wide(caption);
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
            pcwstr(&cls),
            pcwstr(&cap),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            0,
            0,
            cw,
            ch,
            None,
            None,
            hinstance,
            None,
        )
        .ok()?;
        let font: HFONT = CreateFontW(
            -s(12),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            windows::core::w!("Segoe UI"),
        );
        let child = |class: PCWSTR, label: &str, style: u32, ex: WINDOW_EX_STYLE, x: i32, y: i32, w: i32, h: i32, id: i32| -> HWND {
            let l = wide(label);
            let c = CreateWindowExW(ex, class, pcwstr(&l), WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style), x, y, w, h, hwnd, HMENU(id as isize as *mut _), hinstance, None)
                .unwrap_or_default();
            SendMessageW(c, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            c
        };
        let pad = s(14);
        let (bw, bh) = (s(84), s(28));
        child(windows::core::w!("STATIC"), text, 0, WINDOW_EX_STYLE(0), pad, pad, cw - pad * 2, s(56), -1);
        let edit = child(
            windows::core::w!("EDIT"),
            initial,
            (WS_TABSTOP.0 | WS_BORDER.0) | ES_AUTOHSCROLL as u32,
            WINDOW_EX_STYLE(0),
            pad,
            pad + s(64),
            s(90),
            s(26),
            ID_EDIT,
        );
        child(windows::core::w!("BUTTON"), ok, WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32, WINDOW_EX_STYLE(0), cw - pad - bw * 2 - s(8), ch - pad - bh, bw, bh, IDOK.0);
        child(windows::core::w!("BUTTON"), cancel, WS_TABSTOP.0 | BS_PUSHBUTTON as u32, WINDOW_EX_STYLE(0), cw - pad - bw, ch - pad - bh, bw, bh, IDCANCEL.0);
        SendMessageW(edit, EM_SETLIMITTEXT, WPARAM(max_len), LPARAM(0));
        SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));

        // the client area is what the layout above assumes: grow the window by its frame
        let (mut wr, mut cr) = (RECT::default(), RECT::default());
        let _ = GetWindowRect(hwnd, &mut wr);
        let _ = GetClientRect(hwnd, &mut cr);
        let (fw, fh) = ((wr.right - wr.left) - (cr.right - cr.left), (wr.bottom - wr.top) - (cr.bottom - cr.top));
        let (w, h) = (cw + fw, ch + fh);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromPoint(above, MONITOR_DEFAULTTONEAREST), &mut mi);
        let work = mi.rcWork;
        let x = (above.x - w / 2).clamp(work.left, (work.right - w).max(work.left));
        let y = (above.y - h - s(8)).clamp(work.top, (work.bottom - h).max(work.top));
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
        take_focus(hwnd);
        let _ = SetFocus(edit);

        RESULT.with(|r| *r.borrow_mut() = None);
        let mut msg = MSG::default();
        while IsWindow(hwnd).as_bool() {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 {
                // WM_QUIT while the prompt is open: hand it back to the main loop
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            if r.0 == -1 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if IsWindow(hwnd).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let _ = DeleteObject(font);
        RESULT.with(|r| r.borrow_mut().take())
    }
}
