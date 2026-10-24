//! macOS clipboard via NSPasteboard. Polls changeCount (there is no change notification)
//! and serves remote files lazily: Finder's paste blocks until the download is done.

use crate::engine::{LocalClip, RemoteClip, Transfer};
use crate::log;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardTypeFileURL,
    NSPasteboardTypeHTML, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSDate, NSDefaultRunLoopMode, NSRunLoop, NSString, NSURL};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

const PASTE_WAIT: Duration = Duration::from_secs(600);

pub struct ProviderIvars {
    path: PathBuf,
    transfer: Arc<Transfer>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "DeskdropFileProvider"]
    #[ivars = ProviderIvars]
    struct FileProvider;

    unsafe impl NSObjectProtocol for FileProvider {}

    unsafe impl NSPasteboardItemDataProvider for FileProvider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(&self, _pb: Option<&NSPasteboard>, item: &NSPasteboardItem, ty: &NSPasteboardType) {
            let iv = self.ivars();
            if let Err(e) = iv.transfer.wait(PASTE_WAIT) {
                log!("paste of remote files failed: {e}");
                return;
            }
            let url = NSURL::fileURLWithPath(&NSString::from_str(&iv.path.to_string_lossy()));
            if let Some(s) = url.absoluteString() {
                item.setString_forType(&s, ty);
            }
        }
    }
);

impl FileProvider {
    fn new(path: PathBuf, transfer: Arc<Transfer>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ProviderIvars { path, transfer });
        unsafe { msg_send![super(this), init] }
    }
}

fn read_local(pb: &NSPasteboard) -> LocalClip {
    let mut clip = LocalClip::default();
    unsafe {
        if let Some(items) = pb.pasteboardItems() {
            for item in items.iter() {
                let Some(s) = item.stringForType(NSPasteboardTypeFileURL) else {
                    continue;
                };
                // Finder writes file *reference* URLs (file:///.file/id=...); resolve to a path.
                let path = NSURL::URLWithString(&s)
                    .and_then(|u| u.filePathURL())
                    .and_then(|u| u.path());
                if let Some(p) = path {
                    clip.files.push(PathBuf::from(p.to_string()));
                }
            }
        }
        clip.text = pb.stringForType(NSPasteboardTypeString).map(|s| s.to_string());
        if clip.files.is_empty() {
            clip.html = pb.stringForType(NSPasteboardTypeHTML).map(|s| s.to_string());
            clip.png = match pb.dataForType(NSPasteboardTypePNG) {
                Some(d) => Some(d.to_vec()),
                None => pb
                    .dataForType(NSPasteboardTypeTIFF)
                    .and_then(|d| crate::img::to_png(&d.to_vec())),
            };
        }
    }
    clip
}

fn write_remote(pb: &NSPasteboard, clip: &RemoteClip, keep: &mut Vec<Retained<FileProvider>>) {
    keep.clear();
    pb.clearContents();
    let mut items: Vec<Retained<NSPasteboardItem>> = Vec::new();
    unsafe {
        if let Some(t) = &clip.files {
            for p in &t.top {
                let item = NSPasteboardItem::new();
                let prov = FileProvider::new(p.clone(), t.clone());
                let types = NSArray::from_slice(&[NSPasteboardTypeFileURL]);
                item.setDataProvider_forTypes(ProtocolObject::from_ref(&*prov), &types);
                keep.push(prov);
                items.push(item);
            }
        }
        if items.is_empty() {
            items.push(NSPasteboardItem::new());
        }
        let first = &items[0];
        if let Some(s) = &clip.text {
            first.setString_forType(&NSString::from_str(s), NSPasteboardTypeString);
        }
        if let Some(h) = &clip.html {
            first.setString_forType(&NSString::from_str(h), NSPasteboardTypeHTML);
        }
        if let Some(png) = &clip.png {
            first.setData_forType(&NSData::with_bytes(png), NSPasteboardTypePNG);
        }
    }
    let objs: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
        items.into_iter().map(ProtocolObject::from_retained).collect();
    if !pb.writeObjects(&NSArray::from_retained_slice(&objs)) {
        log!("failed to write clipboard");
    }
}

pub fn run(local_tx: Sender<LocalClip>, remote_rx: Receiver<RemoteClip>) -> ! {
    let pb = NSPasteboard::generalPasteboard();
    let rl = NSRunLoop::currentRunLoop();
    let mut last = pb.changeCount();
    let mut keep = Vec::new();
    loop {
        // Pump the run loop so lazy-data requests from other apps get served.
        let until = NSDate::dateWithTimeIntervalSinceNow(0.2);
        if !unsafe { rl.runMode_beforeDate(NSDefaultRunLoopMode, &until) } {
            std::thread::sleep(Duration::from_millis(200));
        }
        while let Ok(clip) = remote_rx.try_recv() {
            write_remote(&pb, &clip, &mut keep);
            last = pb.changeCount();
        }
        let cc = pb.changeCount();
        if cc != last {
            last = cc;
            let clip = read_local(&pb);
            if clip.text.is_some() || clip.png.is_some() || !clip.files.is_empty() {
                let _ = local_tx.send(clip);
            }
        }
    }
}
