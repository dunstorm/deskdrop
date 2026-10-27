//! Windows clipboard via a hidden message-only window. Remote files go on the clipboard
//! as virtual files (see `win_vfiles`), so nothing downloads until Explorer pastes.

use crate::engine::{Engine, LocalClip, RemoteClip};
use crate::log;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Shell::DragQueryFileW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

struct Formats {
    html: u32,
    png: u32,
}

thread_local! {
    static HWND_: Cell<HWND> = const { Cell::new(null_mut()) };
    static FMT: RefCell<Option<Formats>> = const { RefCell::new(None) };
    static IO: RefCell<Option<(Sender<LocalClip>, Receiver<RemoteClip>)>> = const { RefCell::new(None) };
    /// Test hook (devpeer builds): leave the real clipboard alone.
    static NO_CLIPBOARD: Cell<bool> = const { Cell::new(false) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn reg(name: &str) -> u32 {
    unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) }
}

fn fmts<R>(f: impl FnOnce(&Formats) -> R) -> R {
    FMT.with(|x| f(x.borrow().as_ref().unwrap()))
}

fn open_clipboard(hwnd: HWND) -> bool {
    for _ in 0..20 {
        if unsafe { OpenClipboard(hwnd) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

unsafe fn global_bytes(h: HANDLE) -> Option<Vec<u8>> {
    if h.is_null() {
        return None;
    }
    let p = GlobalLock(h) as *const u8;
    if p.is_null() {
        return None;
    }
    let v = std::slice::from_raw_parts(p, GlobalSize(h)).to_vec();
    GlobalUnlock(h);
    Some(v)
}

unsafe fn global_from(bytes: &[u8]) -> HANDLE {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
    if h.is_null() {
        return h;
    }
    let p = GlobalLock(h) as *mut u8;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    GlobalUnlock(h);
    h
}

unsafe fn set_bytes(fmt: u32, bytes: &[u8]) {
    let h = global_from(bytes);
    if !h.is_null() && SetClipboardData(fmt, h).is_null() {
        GlobalFree(h);
    }
}

fn utf16_bytes(s: &str) -> Vec<u8> {
    wide(s).iter().flat_map(|c| c.to_le_bytes()).collect()
}

// ---------- read / write ----------

unsafe fn read_local() -> Option<LocalClip> {
    let hwnd = HWND_.get();
    if !open_clipboard(hwnd) {
        log!("could not open clipboard to read it");
        return None;
    }
    let mut clip = LocalClip::default();
    if IsClipboardFormatAvailable(CF_HDROP as u32) != 0 {
        let h = GetClipboardData(CF_HDROP as u32);
        if !h.is_null() {
            let n = DragQueryFileW(h, u32::MAX, null_mut(), 0);
            for i in 0..n {
                let len = DragQueryFileW(h, i, null_mut(), 0);
                let mut buf = vec![0u16; len as usize + 1];
                DragQueryFileW(h, i, buf.as_mut_ptr(), buf.len() as u32);
                clip.files
                    .push(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])));
            }
        }
    }
    if let Some(b) = global_bytes(GetClipboardData(CF_UNICODETEXT as u32)) {
        let w: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .take_while(|&c| c != 0)
            .collect();
        clip.text = Some(String::from_utf16_lossy(&w).replace("\r\n", "\n"));
    }
    if clip.files.is_empty() {
        let (html, png) = fmts(|f| (f.html, f.png));
        clip.html = global_bytes(GetClipboardData(html)).and_then(|b| crate::cfhtml::decode(&b));
        clip.png = match global_bytes(GetClipboardData(png)) {
            Some(b) => Some(b),
            None => global_bytes(GetClipboardData(CF_DIB as u32)).and_then(|b| crate::img::dib_to_png(&b)),
        };
    }
    CloseClipboard();
    if clip.text.is_none() && clip.png.is_none() && clip.files.is_empty() {
        return None;
    }
    Some(clip)
}

unsafe fn write_remote(clip: RemoteClip) {
    if let Some(t) = clip.files {
        super::win_vfiles::set(t, clip.text);
        return;
    }
    let hwnd = HWND_.get();
    if !open_clipboard(hwnd) {
        log!("could not open clipboard to write it");
        return;
    }
    EmptyClipboard();
    let f = FMT.with(|x| {
        let b = x.borrow();
        let f = b.as_ref().unwrap();
        (f.html, f.png)
    });
    if let Some(t) = &clip.text {
        let crlf = t.replace("\r\n", "\n").replace('\n', "\r\n");
        set_bytes(CF_UNICODETEXT as u32, &utf16_bytes(&crlf));
    }
    if let Some(h) = &clip.html {
        set_bytes(f.0, &crate::cfhtml::encode(h));
    }
    if let Some(png) = &clip.png {
        set_bytes(f.1, png);
        if let Some(dib) = crate::img::png_to_dib(png) {
            set_bytes(CF_DIB as u32, &dib);
        }
    }
    CloseClipboard();
}

/// Whether the clipboard was last set by this process (our window, or OLE's for virtual files).
unsafe fn owned_by_us() -> bool {
    let owner = GetClipboardOwner();
    let mut pid = 0;
    !owner.is_null() && GetWindowThreadProcessId(owner, &mut pid) != 0 && pid == GetCurrentProcessId()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            if !owned_by_us() && !NO_CLIPBOARD.get() {
                if let Some(clip) = read_local() {
                    IO.with(|io| {
                        let _ = io.borrow().as_ref().unwrap().0.send(clip);
                    });
                }
            }
            0
        }
        WM_TIMER => {
            loop {
                let next = IO.with(|io| io.borrow().as_ref().unwrap().1.try_recv().ok());
                match next {
                    // Test mode: act as if the user pasted right away.
                    Some(c) if NO_CLIPBOARD.get() => {
                        if let Some(t) = c.files {
                            t.request("test mode");
                        }
                    }
                    Some(c) => write_remote(c),
                    None => break,
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub fn run(local_tx: Sender<LocalClip>, remote_rx: Receiver<RemoteClip>, engine: Arc<Engine>) -> ! {
    unsafe {
        SetProcessDPIAware();
        #[cfg(feature = "devpeer")]
        NO_CLIPBOARD.set(std::env::var_os("DESKDROP_NO_CLIPBOARD").is_some());
        super::win_progress::spawn(engine.clone());
        super::win_vfiles::init(engine);
        FMT.with(|f| {
            *f.borrow_mut() = Some(Formats {
                html: reg("HTML Format"),
                png: reg("PNG"),
            })
        });
        IO.with(|io| *io.borrow_mut() = Some((local_tx, remote_rx)));
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("deskdrop");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            hinst,
            null_mut::<c_void>(),
        );
        if hwnd.is_null() {
            log!("CreateWindowExW failed");
            std::process::exit(1);
        }
        HWND_.set(hwnd);
        AddClipboardFormatListener(hwnd);
        SetTimer(hwnd, 1, 100, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    std::process::exit(0)
}
