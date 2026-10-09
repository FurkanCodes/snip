//! The post-capture preview card: a small floating panel in the bottom-right
//! corner with a thumbnail, where the capture went, and quick actions.
//! Clicking the thumbnail opens the file; dragging it hands the file to
//! whatever app it is dropped on.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameVibrantDark, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSFont, NSFontWeightMedium, NSFontWeightRegular,
    NSFontWeightSemibold, NSImage, NSPanel, NSResponder, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSNumber, NSObject, NSPoint, NSSize, NSString, NSURL, ns_string};
use objc2_quartz_core::{
    CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction, CATextLayer,
    kCAAlignmentCenter, kCAAlignmentLeft, kCAGravityResizeAspect, kCAMediaTimingFunctionEaseOut,
    kCATruncationMiddle,
};

use super::macos::{measure, mtm, no_anim, rgba, set_font, to_ns_frame};
use super::{PreviewContent, PreviewInput, PreviewPart, Rect};

const W: f64 = 300.0;
const PAD: f64 = 10.0;
const THUMB_W: f64 = W - 2.0 * PAD;
const BUTTON_H: f64 = 28.0;
/// Floats above normal windows, below the capture overlay.
const PREVIEW_LEVEL: isize = 101; // NSPopUpMenuWindowLevel

thread_local! {
    static HANDLER: RefCell<Option<Box<dyn Fn(PreviewInput)>>> = const { RefCell::new(None) };
}

/// Install the callback that receives input from the preview card.
pub fn set_preview_handler(f: impl Fn(PreviewInput) + 'static) {
    HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

fn emit(input: PreviewInput) {
    HANDLER.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f(input);
        }
    });
}

#[derive(Default)]
struct ViewIvars {
    parts: RefCell<Vec<(PreviewPart, CGRect)>>,
    file: RefCell<Option<PathBuf>>,
    drag_image: RefCell<Option<Retained<NSImage>>>,
    down: Cell<Option<(CGPoint, Option<PreviewPart>)>>,
    dragging: Cell<bool>,
    hover: Cell<(bool, Option<PreviewPart>)>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements; PreviewView has no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SnipPreviewView"]
    #[ivars = ViewIvars]
    struct PreviewView;

    unsafe impl NSObjectProtocol for PreviewView {}

    unsafe impl NSDraggingSource for PreviewView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(&self, _s: &NSDraggingSession, _c: NSDraggingContext) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _s: &NSDraggingSession, _p: NSPoint, op: NSDragOperation) {
            self.ivars().dragging.set(false);
            if op != NSDragOperation::None {
                emit(PreviewInput::Dropped);
            }
        }
    }

    impl PreviewView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            let p = self.local(e);
            self.set_hover(true, self.hit(p));
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, e: &NSEvent) {
            let p = self.local(e);
            self.set_hover(true, self.hit(p));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _e: &NSEvent) {
            self.set_hover(false, None);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let p = self.local(e);
            self.ivars().down.set(Some((p, self.hit(p))));
            self.ivars().dragging.set(false);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            let iv = self.ivars();
            let Some((start, Some(PreviewPart::Thumb))) = iv.down.get() else { return };
            let p = self.local(e);
            if iv.dragging.get() || (p.x - start.x).hypot(p.y - start.y) < 4.0 {
                return;
            }
            self.begin_drag(e);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            let iv = self.ivars();
            let down = iv.down.take();
            if iv.dragging.get() {
                return;
            }
            let p = self.local(e);
            if let Some((_, Some(part))) = down {
                if self.hit(p) == Some(part) {
                    emit(PreviewInput::Click(part));
                }
            }
        }
    }
);

impl PreviewView {
    fn local(&self, e: &NSEvent) -> CGPoint {
        self.convertPoint_fromView(e.locationInWindow(), None)
    }

    fn hit(&self, p: CGPoint) -> Option<PreviewPart> {
        let hover = self.ivars().hover.get().0;
        self.ivars()
            .parts
            .borrow()
            .iter()
            // The close button only exists while the card is hovered.
            .filter(|(part, _)| *part != PreviewPart::Close || hover)
            .find(|(_, r)| {
                p.x >= r.origin.x
                    && p.x < r.origin.x + r.size.width
                    && p.y >= r.origin.y
                    && p.y < r.origin.y + r.size.height
            })
            .map(|(part, _)| *part)
    }

    fn set_hover(&self, inside: bool, part: Option<PreviewPart>) {
        if self.ivars().hover.replace((inside, part)) != (inside, part) {
            emit(PreviewInput::Hover { inside, part });
        }
    }

    fn begin_drag(&self, e: &NSEvent) {
        let iv = self.ivars();
        let Some(file) = iv.file.borrow().clone() else { return };
        let Some(thumb) = iv.parts.borrow().iter().find(|(p, _)| *p == PreviewPart::Thumb).map(|(_, r)| *r)
        else {
            return;
        };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&file.to_string_lossy()));
        let item = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*url),
        );
        if let Some(img) = iv.drag_image.borrow().as_ref() {
            unsafe { item.setDraggingFrame_contents(thumb, Some(img.as_ref())) };
        } else {
            item.setDraggingFrame(thumb);
        }
        iv.dragging.set(true);
        let items = NSArray::from_retained_slice(&[item]);
        self.beginDraggingSessionWithItems_event_source(&items, e, ProtocolObject::from_ref(self));
    }
}

fn text_layer(parent: &CALayer, font: &NSFont, color: (f64, f64, f64, f64), scale: f64) -> Retained<CATextLayer> {
    let t = CATextLayer::new();
    set_font(&t, font);
    t.setForegroundColor(Some(&rgba(color.0, color.1, color.2, color.3)));
    t.setContentsScale(scale);
    unsafe {
        t.setTruncationMode(kCATruncationMiddle);
        t.setAlignmentMode(kCAAlignmentLeft);
    }
    parent.addSublayer(&t);
    t
}

fn set_text(layer: &CATextLayer, s: &str) {
    unsafe { layer.setString(Some(NSString::from_str(s).as_ref())) };
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

struct Button {
    bg: Retained<CALayer>,
    label: Retained<CATextLayer>,
}

pub struct PreviewCard {
    panel: Retained<NSPanel>,
    effect: Retained<NSVisualEffectView>,
    view: Retained<PreviewView>,
    thumb: Retained<CALayer>,
    thumb_hover: Retained<CALayer>,
    thumb_hint: Retained<CATextLayer>,
    badge_bg: Retained<CALayer>,
    badge: Retained<CATextLayer>,
    title: Retained<CATextLayer>,
    subtitle: Retained<CATextLayer>,
    primary: Button,
    secondary: Button,
    close_bg: Retained<CALayer>,
    fonts: [Retained<NSFont>; 4], // title, subtitle, button, small
}

impl PreviewCard {
    pub fn new(scale: f64) -> PreviewCard {
        let mtm = mtm();
        let frame = rect(0.0, 0.0, W, 200.0);
        let panel: Retained<NSPanel> = unsafe {
            msg_send![
                NSPanel::alloc(mtm),
                initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setLevel(PREVIEW_LEVEL);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setAcceptsMouseMovedEvents(true);
        panel.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameVibrantDark }).as_deref());

        // Blurred, rounded background.
        let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
        effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        effect.setState(NSVisualEffectState::Active);
        effect.setWantsLayer(true);
        if let Some(l) = effect.layer() {
            l.setCornerRadius(14.0);
            l.setMasksToBounds(true);
            l.setBorderWidth(0.5);
            l.setBorderColor(Some(&rgba(1.0, 1.0, 1.0, 0.14)));
        }
        panel.setContentView(Some(&effect));

        let view = PreviewView::alloc(mtm).set_ivars(ViewIvars::default());
        let view: Retained<PreviewView> = unsafe { msg_send![super(view), initWithFrame: frame] };
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.setWantsLayer(true);
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                frame,
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&area);
        effect.addSubview(&view);

        let host = view.layer().unwrap();
        let root = CALayer::new();
        root.setGeometryFlipped(!host.contentsAreFlipped());
        root.setFrame(rect(0.0, 0.0, W, 1000.0));
        host.addSublayer(&root);

        let fonts = unsafe {
            [
                NSFont::systemFontOfSize_weight(12.5, NSFontWeightSemibold),
                NSFont::systemFontOfSize_weight(11.5, NSFontWeightRegular),
                NSFont::systemFontOfSize_weight(12.0, NSFontWeightMedium),
                NSFont::monospacedDigitSystemFontOfSize_weight(10.5, NSFontWeightSemibold),
            ]
        };

        let thumb = CALayer::new();
        thumb.setCornerRadius(8.0);
        thumb.setMasksToBounds(true);
        thumb.setBackgroundColor(Some(&rgba(1.0, 1.0, 1.0, 0.06)));
        thumb.setBorderWidth(0.5);
        thumb.setBorderColor(Some(&rgba(1.0, 1.0, 1.0, 0.1)));
        unsafe { thumb.setContentsGravity(kCAGravityResizeAspect) };
        root.addSublayer(&thumb);

        let thumb_hover = CALayer::new();
        thumb_hover.setBackgroundColor(Some(&rgba(0.0, 0.0, 0.0, 0.45)));
        thumb_hover.setHidden(true);
        thumb.addSublayer(&thumb_hover);
        let thumb_hint = text_layer(&thumb_hover, &fonts[2], (1.0, 1.0, 1.0, 0.95), scale);
        unsafe { thumb_hint.setAlignmentMode(kCAAlignmentCenter) };
        set_text(&thumb_hint, "Click to open · Drag to share");

        let badge_bg = CALayer::new();
        badge_bg.setBackgroundColor(Some(&rgba(0.0, 0.0, 0.0, 0.62)));
        thumb.addSublayer(&badge_bg);
        let badge = text_layer(&badge_bg, &fonts[3], (1.0, 1.0, 1.0, 0.95), scale);

        let title = text_layer(&root, &fonts[0], (1.0, 1.0, 1.0, 0.95), scale);
        let subtitle = text_layer(&root, &fonts[1], (1.0, 1.0, 1.0, 0.6), scale);

        let button = || {
            let bg = CALayer::new();
            bg.setCornerRadius(7.0);
            bg.setBackgroundColor(Some(&rgba(1.0, 1.0, 1.0, 0.1)));
            root.addSublayer(&bg);
            let label = text_layer(&bg, &fonts[2], (1.0, 1.0, 1.0, 0.95), scale);
            unsafe { label.setAlignmentMode(kCAAlignmentCenter) };
            Button { bg, label }
        };
        let primary = button();
        let secondary = button();

        let close_bg = CALayer::new();
        close_bg.setCornerRadius(10.0);
        close_bg.setBackgroundColor(Some(&rgba(0.12, 0.12, 0.13, 0.9)));
        close_bg.setBorderWidth(0.5);
        close_bg.setBorderColor(Some(&rgba(1.0, 1.0, 1.0, 0.25)));
        close_bg.setHidden(true);
        root.addSublayer(&close_bg);
        let x = text_layer(&close_bg, &fonts[3], (1.0, 1.0, 1.0, 0.9), scale);
        unsafe { x.setAlignmentMode(kCAAlignmentCenter) };
        set_text(&x, "✕");
        let xh = measure(ns_string!("✕"), &fonts[3]).height;
        x.setFrame(rect(0.0, (20.0 - xh) / 2.0, 20.0, xh));

        PreviewCard {
            panel,
            effect,
            view,
            thumb,
            thumb_hover,
            thumb_hint,
            badge_bg,
            badge,
            title,
            subtitle,
            primary,
            secondary,
            close_bg,
            fonts,
        }
    }

    /// Show the card in the bottom-right corner of `area`.
    pub fn show(&self, c: &PreviewContent, area: Rect) {
        let line = |i: usize| measure(ns_string!("Ag"), &self.fonts[i]).height.ceil();
        let (iw, ih) = c.image.map(|i| i.size()).unwrap_or((16.0, 9.0));
        let thumb_h = (THUMB_W * ih / iw.max(1.0)).clamp(72.0, 170.0).round();
        let title_y = PAD + thumb_h + 9.0;
        let sub_y = title_y + line(0) + 1.0;
        let buttons_y = sub_y + line(1) + 10.0;
        let h = buttons_y + BUTTON_H + PAD;
        let bw = (THUMB_W - 8.0) / 2.0;
        let thumb_r = rect(PAD, PAD, THUMB_W, thumb_h);
        let primary_r = rect(PAD, buttons_y, bw, BUTTON_H);
        let secondary_r = rect(PAD + bw + 8.0, buttons_y, bw, BUTTON_H);
        let close_r = rect(2.0, 2.0, 20.0, 20.0);

        no_anim(|| {
            unsafe {
                self.thumb
                    .setContents(c.image.map(|i| &*(CFRetained::as_ptr(&i.0).as_ptr() as *const AnyObject)))
            };
            self.thumb.setFrame(thumb_r);
            self.thumb_hover.setFrame(rect(0.0, 0.0, THUMB_W, thumb_h));
            let hh = line(2);
            self.thumb_hint.setFrame(rect(0.0, ((thumb_h - hh) / 2.0).round(), THUMB_W, hh));

            self.badge_bg.setHidden(c.badge.is_none());
            if let Some(b) = c.badge {
                let m = measure(&NSString::from_str(b), &self.fonts[3]);
                let (bw, bh) = ((m.width + 14.0).ceil(), (m.height + 6.0).ceil());
                self.badge_bg.setFrame(rect(6.0, thumb_h - bh - 6.0, bw, bh));
                self.badge_bg.setCornerRadius(bh / 2.0);
                self.badge.setFrame(rect(7.0, 3.0, m.width.ceil() + 1.0, m.height.ceil()));
                set_text(&self.badge, b);
            }

            self.title.setFrame(rect(PAD + 2.0, title_y, THUMB_W - 4.0, line(0)));
            set_text(&self.title, c.title);
            self.subtitle.setFrame(rect(PAD + 2.0, sub_y, THUMB_W - 4.0, line(1)));
            set_text(&self.subtitle, c.subtitle);

            for (b, r, text) in [(&self.primary, primary_r, c.primary), (&self.secondary, secondary_r, c.secondary)] {
                b.bg.setFrame(r);
                let lh = line(2);
                b.label.setFrame(rect(0.0, ((BUTTON_H - lh) / 2.0).round(), r.size.width, lh));
                set_text(&b.label, text);
            }
            self.close_bg.setFrame(close_r);
            self.set_hover(false, None);
        });

        let iv = self.view.ivars();
        *iv.parts.borrow_mut() = vec![
            (PreviewPart::Close, close_r),
            (PreviewPart::Thumb, thumb_r),
            (PreviewPart::Primary, primary_r),
            (PreviewPart::Secondary, secondary_r),
        ];
        *iv.file.borrow_mut() = Some(c.file.to_owned());
        *iv.drag_image.borrow_mut() = c.image.map(|i| {
            NSImage::initWithCGImage_size(NSImage::alloc(), &i.0, NSSize::new(THUMB_W, thumb_h))
        });
        iv.hover.set((false, None));

        let frame = Rect::new(area.x + area.w - W - 16.0, area.y + area.h - h - 16.0, W, h);
        self.panel.setFrame_display(to_ns_frame(frame), true);
        self.panel.orderFrontRegardless();
        self.panel.invalidateShadow();

        // Slide in from the right.
        if let Some(layer) = self.effect.layer() {
            let ease = CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionEaseOut });
            for (key, from, to) in [("opacity", 0.0, 1.0), ("transform.translation.x", 28.0, 0.0)] {
                let a = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str(key)));
                unsafe {
                    a.setFromValue(Some(NSNumber::new_f64(from).as_ref()));
                    a.setToValue(Some(NSNumber::new_f64(to).as_ref()));
                }
                a.setDuration(0.24);
                a.setTimingFunction(Some(&ease));
                layer.addAnimation_forKey(&a, Some(&NSString::from_str(key)));
            }
        }
    }

    /// Update the texts after an action (e.g. after saving or copying).
    pub fn set_texts(&self, subtitle: &str, primary: &str, secondary: &str, file: &Path) {
        no_anim(|| {
            set_text(&self.subtitle, subtitle);
            set_text(&self.primary.label, primary);
            set_text(&self.secondary.label, secondary);
        });
        *self.view.ivars().file.borrow_mut() = Some(file.to_owned());
    }

    pub fn set_hover(&self, inside: bool, part: Option<PreviewPart>) {
        no_anim(|| {
            self.close_bg.setHidden(!inside);
            self.thumb_hover.setHidden(part != Some(PreviewPart::Thumb));
            for (b, p) in [(&self.primary, PreviewPart::Primary), (&self.secondary, PreviewPart::Secondary)] {
                let a = if part == Some(p) { 0.2 } else { 0.1 };
                b.bg.setBackgroundColor(Some(&rgba(1.0, 1.0, 1.0, a)));
            }
            let close = if part == Some(PreviewPart::Close) {
                rgba(0.3, 0.3, 0.32, 0.95)
            } else {
                rgba(0.12, 0.12, 0.13, 0.9)
            };
            self.close_bg.setBackgroundColor(Some(&close));
        });
    }

    pub fn hide(&self) {
        self.panel.orderOut(None);
        self.view.ivars().hover.set((false, None));
    }
}

