//! Windows-only clipboard probe used to test the agent (e.g. under Wine).
//! `wintest paste` reads CF_HDROP like Explorer's paste; `wintest copy PATH...` sets it.
#[cfg(windows)]
fn main() {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
    use windows_sys::Win32::UI::Shell::DragQueryFileW;
    let args: Vec<String> = std::env::args().skip(1).collect();
    unsafe {
        while OpenClipboard(std::ptr::null_mut()) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match args[0].as_str() {
            "paste" => {
                println!("hdrop available: {}", IsClipboardFormatAvailable(CF_HDROP as u32));
                let t = std::time::Instant::now();
                let h = GetClipboardData(CF_HDROP as u32);
                println!("GetClipboardData took {:?}, null={}", t.elapsed(), h.is_null());
                if !h.is_null() {
                    for i in 0..DragQueryFileW(h, u32::MAX, std::ptr::null_mut(), 0) {
                        let mut b = [0u16; 1024];
                        let n = DragQueryFileW(h, i, b.as_mut_ptr(), 1024);
                        println!("file: {}", String::from_utf16_lossy(&b[..n as usize]));
                    }
                }
                let h = GetClipboardData(CF_UNICODETEXT as u32);
                if !h.is_null() {
                    let p = GlobalLock(h) as *const u16;
                    let mut n = 0;
                    while *p.add(n) != 0 {
                        n += 1;
                    }
                    println!("text: {:?}", String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                    GlobalUnlock(h);
                }
            }
            "copy" => {
                EmptyClipboard();
                let mut v: Vec<u8> = Vec::new();
                for x in [20u32, 0, 0, 0, 1] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                for a in &args[1..] {
                    for c in a.encode_utf16().chain(Some(0)) {
                        v.extend_from_slice(&c.to_le_bytes());
                    }
                }
                v.extend_from_slice(&[0, 0]);
                let h = GlobalAlloc(GMEM_MOVEABLE, v.len());
                std::ptr::copy_nonoverlapping(v.as_ptr(), GlobalLock(h) as *mut u8, v.len());
                GlobalUnlock(h);
                SetClipboardData(CF_HDROP as u32, h);
                println!("copied {} path(s)", args.len() - 1);
            }
            "text" => {
                EmptyClipboard();
                let v: Vec<u8> = args[1]
                    .replace("\\n", "\r\n")
                    .encode_utf16()
                    .chain(Some(0))
                    .flat_map(|c| c.to_le_bytes())
                    .collect();
                let h = GlobalAlloc(GMEM_MOVEABLE, v.len());
                std::ptr::copy_nonoverlapping(v.as_ptr(), GlobalLock(h) as *mut u8, v.len());
                GlobalUnlock(h);
                SetClipboardData(CF_UNICODETEXT as u32, h);
            }
            "dib" => {
                // 4x2 32bpp BI_RGB, zero alpha like a typical screenshot
                EmptyClipboard();
                let mut v: Vec<u8> = Vec::new();
                for x in [40u32, 4, 2] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v.extend_from_slice(&1u16.to_le_bytes());
                v.extend_from_slice(&32u16.to_le_bytes());
                for x in [0u32, 32, 2835, 2835, 0, 0] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                for i in 0..8u8 {
                    v.extend_from_slice(&[i * 30, 0, 255 - i * 30, 0]);
                }
                let h = GlobalAlloc(GMEM_MOVEABLE, v.len());
                std::ptr::copy_nonoverlapping(v.as_ptr(), GlobalLock(h) as *mut u8, v.len());
                GlobalUnlock(h);
                SetClipboardData(windows_sys::Win32::System::Ole::CF_DIB as u32, h);
            }
            _ => {}
        }
        CloseClipboard();
    }
}
#[cfg(not(windows))]
fn main() {}
