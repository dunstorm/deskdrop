//! Windows-only stand-in for Explorer, to test virtual files on the clipboard (e.g. under Wine).
//! `vpaste list` reads only the file list, like Explorer reacting to a clipboard change;
//! `vpaste copy DIR` reads the contents too, like a paste.
#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use std::io::Write;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Memory::*;
    use windows::Win32::System::Ole::*;
    use windows::Win32::UI::Shell::FILEDESCRIPTORW;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let reg = |n: &str| {
        let w: Vec<u16> = n.encode_utf16().chain(Some(0)).collect();
        unsafe { windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW(w.as_ptr()) as u16 }
    };
    let fe = |cf, lindex, tymed: TYMED| FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex,
        tymed: tymed.0 as u32,
    };
    unsafe {
        OleInitialize(None)?;
        let obj = OleGetClipboard()?;
        let m = obj.GetData(&fe(reg("FileGroupDescriptorW"), -1, TYMED_HGLOBAL))?;
        let p = GlobalLock(m.u.hGlobal) as *const u8;
        let n = *(p as *const u32) as usize;
        let mut files = Vec::new();
        for i in 0..n {
            let d: FILEDESCRIPTORW = std::ptr::read_unaligned(p.add(4 + i * size_of::<FILEDESCRIPTORW>()) as *const _);
            let name = { d.cFileName };
            let len = name.iter().position(|&c| c == 0).unwrap_or(260);
            let size = ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
            let dir = d.dwFileAttributes & 0x10 != 0;
            println!(
                "{} {} {}",
                if dir { "dir " } else { "file" },
                size,
                String::from_utf16_lossy(&name[..len])
            );
            files.push((String::from_utf16_lossy(&name[..len]), dir));
        }
        if args.first().map(String::as_str) != Some("copy") {
            return Ok(());
        }
        let out = std::path::PathBuf::from(&args[1]);
        let contents = reg("FileContents");
        let t = std::time::Instant::now();
        for (i, (name, dir)) in files.iter().enumerate() {
            let dest = out.join(name.replace('\\', "/"));
            if *dir {
                std::fs::create_dir_all(&dest).unwrap();
                continue;
            }
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            let m = obj.GetData(&fe(contents, i as i32, TYMED_ISTREAM))?;
            let s = m.u.pstm.as_ref().unwrap();
            let mut f = std::fs::File::create(&dest).unwrap();
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let mut got = 0u32;
                s.Read(buf.as_mut_ptr() as *mut _, buf.len() as u32, Some(&mut got))
                    .ok()?;
                if got == 0 {
                    break;
                }
                f.write_all(&buf[..got as usize]).unwrap();
            }
        }
        println!("copied in {:?}", t.elapsed());
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
