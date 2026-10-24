//! Floating progress panel in the top-right corner (a menu bar item can end up hidden
//! behind the notch). Click-through, non-activating, visible on every Space.

use crate::engine::{Engine, ProgressView, ViewState};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSFontWeightRegular, NSFontWeightSemibold, NSLineBreakMode, NSPanel,
    NSProgressIndicator, NSProgressIndicatorStyle, NSScreen, NSStatusWindowLevel, NSTextField, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CATransaction;
use std::cell::RefCell;
use std::sync::Arc;

const W: f64 = 320.0;
const H: f64 = 74.0;
const MARGIN: f64 = 12.0;

struct Hud {
    engine: Arc<Engine>,
    panel: Retained<NSPanel>,
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    bar: Retained<NSProgressIndicator>,
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
    panel.setIgnoresMouseEvents(true);
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
    let bar = NSProgressIndicator::initWithFrame(mtm.alloc(), rect(16.0, 10.0, W - 32.0, 10.0));
    bar.setStyle(NSProgressIndicatorStyle::Bar);
    bar.setIndeterminate(false);
    bar.setMinValue(0.0);
    bar.setMaxValue(1.0);

    let views: [&NSView; 3] = [&title, &detail, &bar];
    for v in views {
        fx.addSubview(v);
    }
    panel.setContentView(Some(&fx));
    HUD.with(|h| {
        *h.borrow_mut() = Some(Hud {
            engine,
            panel,
            title,
            detail,
            bar,
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
