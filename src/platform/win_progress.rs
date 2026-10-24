//! Small always-on-top progress popup above the taskbar clock. It lives on its own UI thread
//! so it keeps painting while the clipboard thread is blocked serving a paste. Click to dismiss.

use crate::engine::{Engine, ProgressView, ViewState};
use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const W: i32 = 340;
const H: i32 = 78;
const MARGIN: i32 = 12;

struct Fonts {
    title: HFONT,
    body: HFONT,
}

thread_local! {
    static ENGINE: RefCell<Option<Arc<Engine>>> = const { RefCell::new(None) };
    static VIEW: RefCell<Option<ProgressView>> = const { RefCell::new(None) };
    static FONTS: RefCell<Option<Fonts>> = const { RefCell::new(None) };
    static DISMISSED: Cell<u64> = const { Cell::new(0) };
    static DPI: Cell<i32> = const { Cell::new(96) };
}

/// Scale a 96-DPI length to the screen.
fn s(v: i32) -> i32 {
    v * DPI.get() / 96
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn spawn(engine: Arc<Engine>) {
    std::thread::spawn(move || unsafe { run(engine) });
}

unsafe fn font(weight: i32, pt: i32) -> HFONT {
    let face = wide("Segoe UI");
    // CLEARTYPE_QUALITY = 5
    CreateFontW(
        -(pt * DPI.get() / 72),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        1,
        0,
        0,
        5,
        0,
        face.as_ptr(),
    )
}

unsafe fn run(engine: Arc<Engine>) {
    ENGINE.with(|e| *e.borrow_mut() = Some(engine));
    let screen = GetDC(null_mut());
    DPI.set(GetDeviceCaps(screen, LOGPIXELSX as i32));
    ReleaseDC(null_mut(), screen);
    FONTS.with(|f| {
        *f.borrow_mut() = Some(Fonts {
            title: font(600, 10),
            body: font(400, 9),
        })
    });

    let hinst = GetModuleHandleW(null());
    let class = wide("deskdrop-progress");
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(wndproc);
    wc.hInstance = hinst;
    wc.lpszClassName = class.as_ptr();
    wc.hCursor = LoadCursorW(null_mut(), IDC_HAND);
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        class.as_ptr(),
        class.as_ptr(),
        WS_POPUP,
        0,
        0,
        s(W),
        s(H),
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );
    if hwnd.is_null() {
        return;
    }
    // Rounded corners on Windows 11 (DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND); ignored on 10.
    let round: u32 = 2;
    DwmSetWindowAttribute(hwnd, 33, &round as *const u32 as *const _, 4);
    SetTimer(hwnd, 1, 150, None);
    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe fn place(hwnd: HWND) {
    let mut wa: RECT = std::mem::zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut RECT as *mut _, 0);
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        wa.right - s(W) - s(MARGIN),
        wa.bottom - s(H) - s(MARGIN),
        s(W),
        s(H),
        SWP_NOACTIVATE,
    );
}

unsafe fn fill(dc: HDC, r: &RECT, color: COLORREF) {
    let b = CreateSolidBrush(color);
    FillRect(dc, r, b);
    DeleteObject(b);
}

unsafe fn text(dc: HDC, s: &str, mut r: RECT, f: HFONT, color: COLORREF) {
    SelectObject(dc, f);
    SetTextColor(dc, color);
    let w = wide(s);
    DrawTextW(
        dc,
        w.as_ptr(),
        -1,
        &mut r,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
}

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    // Double-buffer to avoid flicker on the frequent updates.
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
    let old = SelectObject(mem, bmp);
    fill(mem, &rc, rgb(32, 32, 32));
    let border = CreateSolidBrush(rgb(64, 64, 64));
    FrameRect(mem, &rc, border);
    DeleteObject(border);
    SetBkMode(mem, TRANSPARENT as i32);
    VIEW.with(|v| {
        let Some(v) = &*v.borrow() else { return };
        FONTS.with(|f| {
            let f = f.borrow();
            let f = f.as_ref().unwrap();
            let (l, r) = (s(16), rc.right - s(16));
            text(
                mem,
                &v.title,
                RECT {
                    left: l,
                    top: s(12),
                    right: r,
                    bottom: s(32),
                },
                f.title,
                rgb(255, 255, 255),
            );
            let detail_color = match v.state {
                ViewState::Running => rgb(190, 190, 190),
                ViewState::Done => rgb(108, 203, 95),
                ViewState::Failed => rgb(255, 153, 164),
            };
            text(
                mem,
                &v.detail,
                RECT {
                    left: l,
                    top: s(33),
                    right: r,
                    bottom: s(51),
                },
                f.body,
                detail_color,
            );
            if v.state != ViewState::Failed {
                let track = RECT {
                    left: l,
                    top: s(60),
                    right: r,
                    bottom: s(64),
                };
                fill(mem, &track, rgb(72, 72, 72));
                let done = RECT {
                    right: l + ((r - l) as f64 * v.fraction.clamp(0.0, 1.0)) as i32,
                    ..track
                };
                let accent = if v.state == ViewState::Done {
                    rgb(108, 203, 95)
                } else {
                    rgb(76, 194, 255)
                };
                fill(mem, &done, accent);
            }
        });
    });
    BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            let v = ENGINE.with(|e| e.borrow().as_ref().unwrap().progress());
            let v = v.filter(|v| v.id != DISMISSED.get());
            let visible = IsWindowVisible(hwnd) != 0;
            let show = v.is_some();
            VIEW.with(|x| *x.borrow_mut() = v);
            if show {
                if !visible {
                    place(hwnd);
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
                InvalidateRect(hwnd, null(), 0);
            } else if visible {
                ShowWindow(hwnd, SW_HIDE);
            }
            0
        }
        WM_LBUTTONUP => {
            if let Some(id) = VIEW.with(|v| v.borrow().as_ref().map(|v| v.id)) {
                DISMISSED.set(id);
            }
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
