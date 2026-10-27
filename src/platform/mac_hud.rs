//! Floating progress panel in the top-right corner (a menu bar item can end up hidden
//! behind the notch). Non-activating, visible on every Space, with a Cancel button.

use crate::engine::{Engine, ProgressView, ViewState};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBezelStyle, NSButton, NSColor, NSControlSize, NSEvent, NSEventMask,
    NSEventTrackingRunLoopMode, NSFont, NSFontWeightRegular, NSFontWeightSemibold, NSLineBreakMode, NSPanel,
    NSProgressIndicator, NSProgressIndicatorStyle, NSScreen, NSStatusWindowLevel, NSTextField, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CATransaction;
use std::cell::{Cell, RefCell};
use std::sync::Arc;

const W: f64 = 320.0;
const H: f64 = 74.0;
const MARGIN: f64 = 12.0;
const BUTTON_W: f64 = 64.0;

pub struct CancelIvars {
    engine: Arc<Engine>,
    id: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSButton, objc2_app_kit::NSControl, NSView, objc2_app_kit::NSResponder, objc2::runtime::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DeskdropCancelButton"]
    #[ivars = CancelIvars]
    struct CancelButton;

    impl CancelButton {
        /// Work on the first click even though the panel never becomes key.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(cancelTransfer:))]
        fn cancel_transfer(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            iv.engine.cancel(iv.id.get());
        }
    }
);

impl CancelButton {
    fn new(engine: Arc<Engine>, frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CancelIvars {
            engine,
            id: Cell::new(0),
        });
        let b: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        b.setTitle(&NSString::from_str("Cancel"));
        b.setBezelStyle(NSBezelStyle::Push);
        b.setControlSize(NSControlSize::Small);
        b.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        unsafe {
            b.setTarget(Some(&b));
            b.setAction(Some(sel!(cancelTransfer:)));
        }
        b
    }
}

struct Hud {
    panel: Retained<NSPanel>,
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    bar: Retained<NSProgressIndicator>,
    cancel: Retained<CancelButton>,
    engine: Arc<Engine>,
    visible: bool,
}

thread_local! {
    static HUD: RefCell<Option<Hud>> = const { RefCell::new(None) };
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn label(mtm: MainThreadMarker, frame: NSRect, font: &NSFont) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::new(), mtm);
    l.setFrame(frame);
    l.setFont(Some(font));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    l
}

pub fn init(engine: Arc<Engine>, mtm: MainThreadMarker) {
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        mtm.alloc(),
        rect(0.0, 0.0, W, H),
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(NSStatusWindowLevel);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );

    let fx = NSVisualEffectView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, W, H));
    fx.setMaterial(NSVisualEffectMaterial::HUDWindow);
    fx.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    fx.setState(NSVisualEffectState::Active);
    fx.setWantsLayer(true);
    if let Some(layer) = fx.layer() {
        layer.setCornerRadius(12.0);
        layer.setMasksToBounds(true);
    }

    let (title_font, detail_font) = unsafe {
        (
            NSFont::systemFontOfSize_weight(13.0, NSFontWeightSemibold),
            NSFont::monospacedDigitSystemFontOfSize_weight(11.0, NSFontWeightRegular),
        )
    };
    let title = label(mtm, rect(16.0, 44.0, W - 32.0, 18.0), &title_font);
    let detail = label(mtm, rect(16.0, 26.0, W - 32.0, 16.0), &detail_font);
    detail.setTextColor(Some(&NSColor::secondaryLabelColor()));
    let cancel = CancelButton::new(engine.clone(), rect(W - 12.0 - BUTTON_W, 40.0, BUTTON_W, 24.0), mtm);
    let bar = NSProgressIndicator::initWithFrame(mtm.alloc(), rect(16.0, 10.0, W - 32.0, 10.0));
    bar.setStyle(NSProgressIndicatorStyle::Bar);
    bar.setIndeterminate(false);
    bar.setMinValue(0.0);
    bar.setMaxValue(1.0);

    let views: [&NSView; 4] = [&title, &detail, &bar, &cancel];
    for v in views {
        fx.addSubview(v);
    }
    panel.setContentView(Some(&fx));
    HUD.with(|h| {
        *h.borrow_mut() = Some(Hud {
            panel,
            title,
            detail,
            bar,
            cancel,
            engine,
            visible: false,
        })
    });
}

fn place(panel: &NSPanel, mtm: MainThreadMarker) {
    if let Some(screen) = NSScreen::mainScreen(mtm) {
        let f = screen.visibleFrame();
        let x = f.origin.x + f.size.width - W - MARGIN;
        let y = f.origin.y + f.size.height - H - MARGIN;
        panel.setFrame_display(rect(x, y, W, H), false);
    }
}

fn show(h: &Hud, v: &ProgressView) {
    h.title.setStringValue(&NSString::from_str(&v.title));
    h.detail.setStringValue(&NSString::from_str(&v.detail));
    h.bar.setDoubleValue(v.fraction);
    let tint = match v.state {
        ViewState::Running => NSColor::secondaryLabelColor(),
        ViewState::Done => NSColor::systemGreenColor(),
        ViewState::Failed => NSColor::systemRedColor(),
    };
    h.detail.setTextColor(Some(&tint));
    h.bar.setHidden(v.state == ViewState::Failed);
    let running = v.state == ViewState::Running;
    h.cancel.ivars().id.set(v.id);
    h.cancel.setHidden(!running);
    let reserve = if running { BUTTON_W } else { 0.0 };
    h.title.setFrameSize(NSSize::new(W - 32.0 - reserve, 18.0));
}

/// Deliver pending clicks to the panel while the main thread is stuck serving a paste.
pub fn pump_clicks() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let mask = NSEventMask::LeftMouseDown | NSEventMask::LeftMouseUp;
    // The tracking mode doesn't service pasteboard requests, so this can't re-enter `provide`.
    while let Some(ev) = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            mask,
            Some(&NSDate::distantPast()),
            NSEventTrackingRunLoopMode,
            true,
        )
    } {
        app.sendEvent(&ev);
    }
}

/// Show, update or hide the panel to match the engine. Main thread only; safe to call
/// from inside a blocking pasteboard callback.
pub fn update() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    HUD.with(|h| {
        let mut h = h.borrow_mut();
        let Some(h) = h.as_mut() else { return };
        match h.engine.progress() {
            Some(v) => {
                show(h, &v);
                if !h.visible {
                    place(&h.panel, mtm);
                    h.panel.orderFrontRegardless();
                    h.visible = true;
                }
                h.panel.displayIfNeeded();
            }
            None if h.visible => {
                h.panel.orderOut(None);
                h.visible = false;
            }
            None => return,
        }
        // Push the change to the window server even if we're inside a blocking callback.
        CATransaction::flush();
    });
}
