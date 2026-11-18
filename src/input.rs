//! A small modal text prompt (a remote machine's address, token or label): a line of text, an
//! edit box, OK and Cancel, built from plain controls so no dialog resource is needed; for the
//! label also a checkbox and a live preview of how the strip will look. Enter, Esc and Tab work
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

use crate::canvas::Canvas;
use crate::util::{pcwstr, wide};

const ID_EDIT: i32 = 100;
const ID_CHECK: i32 = 101;
const CLASS: &str = "WinStatsPrompt";
const PREVIEW_CLASS: &str = "WinStatsPromptPreview";

/// Draws the preview into a canvas of the given size for the text typed and the checkbox state;
/// `advance` (from the preview's timer) moves its sample data on by a step.
pub type PreviewFn = Box<dyn FnMut(&mut Canvas, i32, i32, &str, bool, bool)>;
/// how often the preview's data moves on
const PREVIEW_TICK_MS: u32 = 400;

/// What a prompt shows beyond the text box: a checkbox (its text and state) and a preview.
#[derive(Default)]
pub struct Extras {
    pub check: Option<(String, bool)>,
    pub preview: Option<PreviewFn>,
}

struct Preview {
    draw: PreviewFn,
    canvas: Canvas,
    hwnd: HWND,
}

thread_local! {
    /// what the open prompt ended with: Some((text, checkbox)) on OK, None on Cancel / close
    static RESULT: RefCell<Option<(String, bool)>> = const { RefCell::new(None) };
    static PREVIEW: RefCell<Option<Preview>> = const { RefCell::new(None) };
}

/// The text in the edit box and the checkbox state of the open prompt.
unsafe fn current(hwnd: HWND) -> (String, bool) {
    unsafe {
        let edit = GetDlgItem(hwnd, ID_EDIT).unwrap_or_default();
        let mut buf = [0u16; 300];
        let n = GetWindowTextW(edit, &mut buf) as usize;
        let text = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        let checked = GetDlgItem(hwnd, ID_CHECK).map_or(false, |c| SendMessageW(c, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1);
        (text, checked)
    }
}

/// Draw the preview again for what the prompt holds now; `advance` moves its data on.
unsafe fn refresh_preview(hwnd: HWND, advance: bool) {
    let (text, checked) = unsafe { current(hwnd) };
    PREVIEW.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            let mut r = RECT::default();
            unsafe {
                let _ = GetClientRect(p.hwnd, &mut r);
            }
            (p.draw)(&mut p.canvas, r.right - r.left, r.bottom - r.top, &text, checked, advance);
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(p.hwnd, None, false);
            }
        }
    });
}

unsafe extern "system" fn preview_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
    match msg {
        WM_PAINT => {
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                PREVIEW.with(|p| {
                    if let Some(p) = p.borrow_mut().as_mut() {
                        p.canvas.paint_dc(hdc);
                    }
                });
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER => {
            unsafe {
                if let Ok(parent) = GetParent(hwnd) {
                    refresh_preview(parent, true);
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if (id == ID_EDIT && code == EN_CHANGE) || (id == ID_CHECK && code == BN_CLICKED) {
                unsafe { refresh_preview(hwnd, false) };
            } else if id == IDOK.0 {
                let result = unsafe { current(hwnd) };
                RESULT.with(|r| *r.borrow_mut() = Some(result));
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
        let pcls = wide(PREVIEW_CLASS);
        let pc = WNDCLASSEXW { cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(preview_wndproc), hInstance: wc.hInstance, lpszClassName: pcwstr(&pcls), ..Default::default() };
        std::mem::forget(pcls);
        RegisterClassExW(&pc);
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
    prompt_ex(caption, text, initial, max_len, dpi, above, ok, cancel, Extras::default()).map(|r| r.0)
}

/// `prompt` with a checkbox under the text box and a preview to its right (`extras`); gives the
/// text and the checkbox state.
pub fn prompt_ex(caption: &str, text: &str, initial: &str, max_len: usize, dpi: u32, above: POINT, ok: &str, cancel: &str, extras: Extras) -> Option<(String, bool)> {
    register();
    let s = |dip: i32| (dip as f32 * dpi as f32 / 96.0).round() as i32;
    // a short value (a label) gets a short box in a bigger font, with the preview beside it; an
    // address or a token the whole width of a wider window, so a 32-character token shows in full
    let long = max_len > 16;
    let rich = extras.check.is_some() || extras.preview.is_some();
    let (cw, ch) = (s(if long || rich { 440 } else { 360 }), s(if extras.check.is_some() { 196 } else { 160 }));
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
        let (edit_w, edit_h) = if long { (cw - pad * 2, s(26)) } else if rich { (s(120), s(34)) } else { (s(90), s(26)) };
        let edit_y = pad + s(60);
        let edit = child(windows::core::w!("EDIT"), initial, (WS_TABSTOP.0 | WS_BORDER.0) | ES_AUTOHSCROLL as u32, WINDOW_EX_STYLE(0), pad, edit_y, edit_w, edit_h, ID_EDIT);
        // the label box in a bigger font: four letters that should be easy to read
        let big: Option<HFONT> = rich.then(|| {
            CreateFontW(-s(18), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32, (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32, windows::core::w!("Segoe UI"))
        });
        if let Some(b) = big {
            SendMessageW(edit, WM_SETFONT, WPARAM(b.0 as usize), LPARAM(1));
        }
        if let Some((label, on)) = &extras.check {
            let c = child(windows::core::w!("BUTTON"), label, WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32, WINDOW_EX_STYLE(0), pad, edit_y + edit_h + s(10), s(260), s(24), ID_CHECK);
            SendMessageW(c, BM_SETCHECK, WPARAM(if *on { 1 } else { 0 }), LPARAM(0));
        }
        if let Some(draw) = extras.preview {
            // beside the box, as wide as the rest of the window
            let (px, py) = (pad + edit_w + s(16), edit_y - s(8));
            let (pw, ph) = (cw - px - pad, edit_h + s(16));
            let pcls = wide(PREVIEW_CLASS);
            let ph_wnd = CreateWindowExW(WINDOW_EX_STYLE(0), pcwstr(&pcls), PCWSTR::null(), WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0), px, py, pw, ph, hwnd, None, hinstance, None).unwrap_or_default();
            PREVIEW.with(|p| *p.borrow_mut() = Some(Preview { draw, canvas: Canvas::new(), hwnd: ph_wnd }));
            refresh_preview(hwnd, false);
            SetTimer(ph_wnd, 1, PREVIEW_TICK_MS, None);
        }
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
        PREVIEW.with(|p| *p.borrow_mut() = None);
        let _ = DeleteObject(font);
        if let Some(b) = big {
            let _ = DeleteObject(b);
        }
        RESULT.with(|r| r.borrow_mut().take())
    }
}
