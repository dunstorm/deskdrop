//! macOS clipboard via NSPasteboard. Polls changeCount (there is no change notification)
//! and serves remote files lazily: Finder's paste starts the download and blocks until
//! it's done.
//! Transfer progress is shown in a floating panel (see `mac_hud`).

use crate::engine::{Engine, LocalClip, RemoteClip, Transfer};
use crate::log;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEventMask, NSPasteboard, NSPasteboardItem,
    NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardTypeHTML, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSDate, NSDefaultRunLoopMode, NSString, NSURL};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
            iv.transfer.request();
            // This runs on the main thread, so keep the progress panel (and its Cancel
            // button) alive while we wait.
            let deadline = Instant::now() + PASTE_WAIT;
            let r = loop {
                match iv.transfer.wait_step(Duration::from_millis(100)) {
                    Some(r) => break r,
                    None if Instant::now() > deadline => break Err("timed out".into()),
                    None => {
                        super::mac_hud::pump_clicks();
                        super::mac_hud::update();
                    }
                }
            };
            super::mac_hud::update();
            if let Err(e) = r {
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

pub fn run(local_tx: Sender<LocalClip>, remote_rx: Receiver<RemoteClip>, engine: Arc<Engine>) -> ! {
    let mtm = MainThreadMarker::new().expect("platform::run must be called on the main thread");
    // A UI-less "accessory" app: no Dock icon, but allowed to own a menu bar item.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    super::mac_hud::init(engine, mtm);
    // Test hook: leave the real clipboard alone (agent only moves data and shows progress).
    #[cfg(feature = "devpeer")]
    let no_clipboard = std::env::var_os("DESKDROP_NO_CLIPBOARD").is_some();
    #[cfg(not(feature = "devpeer"))]
    let no_clipboard = false;
    let pb = NSPasteboard::generalPasteboard();
    let mut last = pb.changeCount();
    let mut keep = Vec::new();
    loop {
        // Runs the run loop too, so lazy-data requests from other apps get served.
        let until = NSDate::dateWithTimeIntervalSinceNow(0.2);
        let ev = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&until),
                NSDefaultRunLoopMode,
                true,
            )
        };
        if let Some(ev) = ev {
            app.sendEvent(&ev);
        }
        super::mac_hud::update();
        while let Ok(clip) = remote_rx.try_recv() {
            if no_clipboard {
                // Test mode: act as if the user pasted right away.
                if let Some(t) = &clip.files {
                    t.request();
                }
            } else {
                write_remote(&pb, &clip, &mut keep);
                last = pb.changeCount();
            }
        }
        let cc = pb.changeCount();
        if cc != last && !no_clipboard {
            last = cc;
            let clip = read_local(&pb);
            if clip.text.is_some() || clip.png.is_some() || !clip.files.is_empty() {
                let _ = local_tx.send(clip);
            }
        }
    }
}
