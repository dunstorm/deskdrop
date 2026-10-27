//! Remote files on the Windows clipboard as virtual files (FileGroupDescriptorW plus
//! FileContents), the way Remote Desktop does it. Explorer reads the file list whenever it
//! likes (it does so as soon as the clipboard changes) but asks for contents only when it
//! pastes. That first request starts the download, and each file streams out of the cache
//! as its bytes arrive, under Explorer's own copy dialog.

use crate::engine::{Engine, Transfer};
use crate::log;
use std::cell::RefCell;
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::{implement, Ref, BOOL, HRESULT, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::*;
use windows::Win32::UI::Shell::*;

const CF_UNICODETEXT: u16 = 13;

#[derive(Clone, Copy)]
struct Formats {
    descriptor: u16,
    contents: u16,
    drop_effect: u16,
    history: u16,
    cloud: u16,
}

thread_local! {
    static ENGINE: RefCell<Option<Arc<Engine>>> = const { RefCell::new(None) };
    static FMT: RefCell<Option<Formats>> = const { RefCell::new(None) };
}

fn reg(name: &str) -> u16 {
    let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe { windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW(w.as_ptr()) as u16 }
}

/// Call once on the clipboard thread, before any `set`.
pub fn init(engine: Arc<Engine>) {
    if let Err(e) = unsafe { OleInitialize(None) } {
        log!("OleInitialize failed: {e}");
    }
    ENGINE.with(|x| *x.borrow_mut() = Some(engine));
    FMT.with(|f| {
        *f.borrow_mut() = Some(Formats {
            descriptor: reg("FileGroupDescriptorW"),
            contents: reg("FileContents"),
            drop_effect: reg("Preferred DropEffect"),
            history: reg("CanIncludeInClipboardHistory"),
            cloud: reg("CanUploadToCloudClipboard"),
        })
    });
}

/// Put `t`'s files (and optional text) on the clipboard without downloading anything.
pub fn set(t: Arc<Transfer>, text: Option<String>) {
    let Some(fmt) = FMT.with(|f| *f.borrow()) else { return };
    let obj: IDataObject = Files { t, text, fmt }.into();
    if let Err(e) = unsafe { OleSetClipboard(&obj) } {
        log!("could not put files on the clipboard: {e}");
    }
}

fn filetime(unix: i64) -> FILETIME {
    let t = (unix.max(0) as u64 + 11_644_473_600) * 10_000_000;
    FILETIME {
        dwLowDateTime: t as u32,
        dwHighDateTime: (t >> 32) as u32,
    }
}

fn format(cf: u16, tymed: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: tymed.0 as u32,
    }
}

fn hglobal(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

#[implement(IDataObject)]
struct Files {
    t: Arc<Transfer>,
    text: Option<String>,
    fmt: Formats,
}

impl Files {
    fn formats(&self) -> Vec<FORMATETC> {
        let f = self.fmt;
        let mut v = vec![
            format(f.descriptor, TYMED_HGLOBAL),
            format(f.contents, TYMED_ISTREAM),
            format(f.drop_effect, TYMED_HGLOBAL),
            format(f.history, TYMED_HGLOBAL),
            format(f.cloud, TYMED_HGLOBAL),
        ];
        if self.text.is_some() {
            v.push(format(CF_UNICODETEXT, TYMED_HGLOBAL));
        }
        v
    }

    /// FILEGROUPDESCRIPTORW: a count followed by one FILEDESCRIPTORW per entry.
    fn descriptor(&self) -> Vec<u8> {
        let flags = [FD_ATTRIBUTES, FD_FILESIZE, FD_WRITESTIME, FD_PROGRESSUI, FD_UNICODE]
            .iter()
            .fold(0u32, |acc, f| acc | f.0 as u32);
        let mut out = (self.t.entries.len() as u32).to_le_bytes().to_vec();
        for e in &self.t.entries {
            // FILEDESCRIPTORW is packed, so build the name separately.
            let mut name = [0u16; 260];
            for (dst, c) in name[..259].iter_mut().zip(e.path.replace('/', "\\").encode_utf16()) {
                *dst = c;
            }
            let d = FILEDESCRIPTORW {
                dwFlags: flags,
                dwFileAttributes: if e.dir {
                    FILE_ATTRIBUTE_DIRECTORY.0
                } else {
                    FILE_ATTRIBUTE_NORMAL.0
                },
                ftLastWriteTime: filetime(e.mtime),
                nFileSizeHigh: (e.size >> 32) as u32,
                nFileSizeLow: e.size as u32,
                cFileName: name,
                ..Default::default()
            };
            let raw = unsafe { std::slice::from_raw_parts(&d as *const _ as *const u8, size_of::<FILEDESCRIPTORW>()) };
            out.extend_from_slice(raw);
        }
        out
    }
}

impl IDataObject_Impl for Files_Impl {
    fn GetData(&self, fe: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let fe = unsafe { &*fe };
        let f = self.fmt;
        if fe.cfFormat == f.contents {
            if fe.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                return Err(DV_E_TYMED.into());
            }
            let idx = fe.lindex as usize;
            let Some(e) = self.t.entries.get(idx).filter(|e| !e.dir) else {
                return Err(DV_E_LINDEX.into());
            };
            // Explorer is pasting: start the download.
            self.t.request("a paste in Explorer");
            let s: IStream = Stream {
                t: self.t.clone(),
                idx,
                size: e.size,
                at: Mutex::new((0, None)),
            }
            .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(s)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        if fe.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return Err(DV_E_TYMED.into());
        }
        let bytes = match fe.cfFormat {
            c if c == f.descriptor => self.descriptor(),
            c if c == f.drop_effect => 1u32.to_le_bytes().to_vec(), // DROPEFFECT_COPY
            c if c == f.history || c == f.cloud => 0u32.to_le_bytes().to_vec(),
            CF_UNICODETEXT if self.text.is_some() => {
                let t = self.text.as_ref().unwrap().replace("\r\n", "\n").replace('\n', "\r\n");
                t.encode_utf16().chain(Some(0)).flat_map(|c| c.to_le_bytes()).collect()
            }
            _ => return Err(DV_E_FORMATETC.into()),
        };
        hglobal(&bytes)
    }

    fn GetDataHere(&self, _fe: *const FORMATETC, _m: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, fe: *const FORMATETC) -> HRESULT {
        let fe = unsafe { &*fe };
        match self.formats().iter().find(|f| f.cfFormat == fe.cfFormat) {
            Some(f) if f.tymed & fe.tymed != 0 => S_OK,
            Some(_) => DV_E_TYMED,
            None => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _i: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            unsafe { (*out).ptd = std::ptr::null_mut() };
        }
        E_NOTIMPL
    }

    fn SetData(&self, _fe: *const FORMATETC, m: *const STGMEDIUM, release: BOOL) -> windows::core::Result<()> {
        // Explorer reports "Performed DropEffect" etc. here; nothing to keep.
        if release.as_bool() {
            unsafe { ReleaseStgMedium(m as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dir: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dir != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }

    fn DAdvise(&self, _fe: *const FORMATETC, _a: u32, _s: Ref<IAdviseSink>) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _c: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// One file's contents, read from the cache and waiting for bytes that haven't arrived yet.
#[implement(IStream)]
struct Stream {
    t: Arc<Transfer>,
    idx: usize,
    size: u64,
    /// Read position and the open cache file.
    at: Mutex<(u64, Option<File>)>,
}

impl ISequentialStream_Impl for Stream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        let mut at = self.at.lock().unwrap();
        let pos = at.0;
        let mut n = 0u32;
        let want = (cb as u64).min(self.size.saturating_sub(pos));
        if want > 0 {
            let avail = loop {
                let avail = self.t.available(self.idx);
                if avail > pos {
                    break avail;
                }
                match self.t.wait_step(Duration::from_millis(50)) {
                    Some(Err(e)) if e == "cancelled" => return ERROR_CANCELLED.to_hresult(),
                    Some(Err(_)) => return E_FAIL,
                    Some(Ok(())) if self.t.available(self.idx) <= pos => return E_FAIL,
                    _ => {}
                }
            };
            if at.1.is_none() {
                match File::open(self.t.root.join(&self.t.entries[self.idx].path)) {
                    Ok(f) => at.1 = Some(f),
                    Err(e) => {
                        log!("paste: cannot open cached file: {e}");
                        return E_FAIL;
                    }
                }
            }
            let f = at.1.as_mut().unwrap();
            let len = want.min(avail - pos) as usize;
            let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, len) };
            let r = f.seek(SeekFrom::Start(pos)).and_then(|_| f.read(buf));
            match r {
                Ok(k) => n = k as u32,
                Err(e) => {
                    log!("paste: read failed: {e}");
                    return E_FAIL;
                }
            }
            at.0 += n as u64;
        }
        if !pcbread.is_null() {
            unsafe { *pcbread = n };
        }
        S_OK
    }

    fn Write(&self, _pv: *const c_void, _cb: u32, _w: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for Stream_Impl {
    fn Seek(&self, mv: i64, origin: STREAM_SEEK, newpos: *mut u64) -> windows::core::Result<()> {
        let mut at = self.at.lock().unwrap();
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => at.0 as i64,
            STREAM_SEEK_END => self.size as i64,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let p = base.checked_add(mv).filter(|p| *p >= 0).ok_or(STG_E_INVALIDFUNCTION)?;
        at.0 = p as u64;
        if !newpos.is_null() {
            unsafe { *newpos = at.0 };
        }
        Ok(())
    }

    fn SetSize(&self, _n: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, _s: Ref<IStream>, _cb: u64, _r: *mut u64, _w: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _f: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _o: u64, _cb: u64, _t: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _o: u64, _cb: u64, _t: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, st: *mut STATSTG, flag: &STATFLAG) -> windows::core::Result<()> {
        let e = &self.t.entries[self.idx];
        let mut name = PWSTR::null();
        if *flag != STATFLAG_NONAME {
            let leaf = e.path.rsplit(['/', '\\']).next().unwrap_or(&e.path);
            let w: Vec<u16> = leaf.encode_utf16().chain(Some(0)).collect();
            unsafe {
                let p = CoTaskMemAlloc(w.len() * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                    name = PWSTR(p);
                }
            }
        }
        unsafe {
            *st = STATSTG {
                pwcsName: name,
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.size,
                mtime: filetime(e.mtime),
                ..Default::default()
            };
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

impl Drop for Stream {
    /// Explorer releases the stream early when its copy dialog is cancelled; stop the download.
    fn drop(&mut self) {
        let pos = self.at.get_mut().map(|a| a.0).unwrap_or(0);
        if pos > 0 && pos < self.size && self.t.wait_step(Duration::ZERO).is_none() {
            log!("paste stopped in Explorer");
            ENGINE.with(|e| {
                if let Some(e) = e.borrow().as_ref() {
                    e.cancel(self.t.id);
                }
            });
        }
    }
}
