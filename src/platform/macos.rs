//! macOS backend.
//!
//! * Overlays are non-activating panels raised above the menu bar and drawn
//!   with Core Animation layers (GPU composited, nothing redrawn on the CPU).
//! * Capturing (screenshots and video) lives in `macos_capture.rs`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBitmapImageFileType, NSCursor, NSPanel, NSResponder, NSTrackingArea,
    NSTrackingAreaOptions, NSWindowStyleMask, NSBitmapImageRep, NSColor, NSEvent, NSFont,
    NSFontAttributeName, NSFontWeightMedium, NSFontWeightSemibold, NSModalResponseOK, NSOpenPanel,
    NSPasteboard, NSPasteboardTypePNG, NSScreen, NSSound, NSStringDrawing, NSView, NSWindow,
    NSWindowCollectionBehavior,
};
use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGImage, CGWindowListCopyWindowInfo, CGWindowListOption};
use objc2_foundation::{
    NSArray, NSAttributedStringKey, NSObject, NSData, NSDictionary, NSNumber, NSSize, NSString, NSURL,
    ns_string,
};
use objc2_quartz_core::{CALayer, CATextLayer, CATransaction, kCAAlignmentLeft};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use super::{OverlayState, Rect, Screen, WinInfo};

/// Above the menu bar, Dock and full-screen apps.
const OVERLAY_LEVEL: isize = 1000; // NSScreenSaverWindowLevel

pub(super) fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("must be called on the main thread")
}

fn ns_view(window: &Window) -> Retained<NSView> {
    let RawWindowHandle::AppKit(h) = window.window_handle().unwrap().as_raw() else {
        unreachable!("not an AppKit window")
    };
    unsafe { Retained::retain(h.ns_view.as_ptr().cast::<NSView>()) }.unwrap()
}

pub(super) fn rgba(r: f64, g: f64, b: f64, a: f64) -> CFRetained<CGColor> {
    CGColor::new_srgb(r, g, b, a)
}

pub(super) fn cg(r: Rect) -> CGRect {
    CGRect::new(CGPoint::new(r.x, r.y), CGSize::new(r.w, r.h))
}

fn primary_height() -> f64 {
    NSScreen::screens(mtm())
        .firstObject()
        .map(|s| s.frame().size.height)
        .unwrap_or(0.0)
}

/// Global top-left points -> AppKit bottom-left frame.
pub(super) fn to_ns_frame(r: Rect) -> CGRect {
    cg(Rect::new(r.x, primary_height() - (r.y + r.h), r.w, r.h))
}

pub fn screens() -> Vec<Screen> {
    let ph = primary_height();
    NSScreen::screens(mtm())
        .iter()
        .map(|s| {
            let f = s.frame();
            Screen {
                rect: Rect::new(
                    f.origin.x,
                    ph - (f.origin.y + f.size.height),
                    f.size.width,
                    f.size.height,
                ),
                scale: s.backingScaleFactor(),
            }
        })
        .collect()
}

/// The usable part of the screen containing `p` (no menu bar or Dock).
pub fn visible_area(p: (f64, f64)) -> Rect {
    let ph = primary_height();
    let screens = NSScreen::screens(mtm());
    let pick = screens
        .iter()
        .find(|s| {
            let f = s.frame();
            Rect::new(f.origin.x, ph - (f.origin.y + f.size.height), f.size.width, f.size.height).contains(p)
        })
        .or_else(|| screens.firstObject());
    pick.map(|s| {
        let v = s.visibleFrame();
        Rect::new(v.origin.x, ph - (v.origin.y + v.size.height), v.size.width, v.size.height)
    })
    .unwrap_or_default()
}

/// Size of `s` rendered in `font`.
pub(super) fn measure(s: &NSString, font: &NSFont) -> CGSize {
    let attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> =
        NSDictionary::from_slices(&[unsafe { NSFontAttributeName }], &[font.as_ref()]);
    unsafe { s.sizeWithAttributes(Some(&attrs)) }
}

pub(super) fn set_font(layer: &CATextLayer, font: &NSFont) {
    // NSFont is toll-free bridged to CTFont, which CATextLayer accepts.
    unsafe { layer.setFont(Some(&*(font as *const NSFont).cast::<CFType>())) };
    layer.setFontSize(font.pointSize());
}

pub fn cursor_pos() -> (f64, f64) {
    let p = NSEvent::mouseLocation();
    (p.x, primary_height() - p.y)
}

/// Prepare a borderless winit window to float above everything, on every
/// Space, including over full-screen apps.
fn float_window(window: &Window, frame: Rect, click_through: bool) -> Retained<NSWindow> {
    let w = ns_view(window).window().unwrap();
    w.setLevel(OVERLAY_LEVEL);
    w.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    w.setOpaque(false);
    w.setBackgroundColor(Some(&NSColor::clearColor()));
    w.setHasShadow(false);
    w.setIgnoresMouseEvents(click_through);
    // Stay visible when we hand focus back to the previous app.
    w.setCanHide(false);
    w.setFrame_display(to_ns_frame(frame), false);
    w
}

/// A flipped (top-left origin) root layer filling the window's content view.
fn root_layer(window: &Window) -> Retained<CALayer> {
    let view = ns_view(window);
    view.setWantsLayer(true);
    let host = view.layer().unwrap();
    let root = CALayer::new();
    root.setGeometryFlipped(!host.contentsAreFlipped());
    root.setFrame(host.bounds());
    host.addSublayer(&root);
    root
}

pub(super) fn no_anim(f: impl FnOnce()) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    f();
    CATransaction::commit();
}

// --- Pill: a rounded label with an optional coloured dot -------------------

struct Pill {
    bg: Retained<CALayer>,
    dot: Retained<CALayer>,
    text: Retained<CATextLayer>,
    font: Retained<NSFont>,
    size: f64,
}

impl Pill {
    fn new(parent: &CALayer, size: f64, mono: bool, scale: f64) -> Pill {
        let font = unsafe {
            if mono {
                NSFont::monospacedDigitSystemFontOfSize_weight(size, NSFontWeightSemibold)
            } else {
                NSFont::systemFontOfSize_weight(size, NSFontWeightMedium)
            }
        };
        let bg = CALayer::new();
        bg.setBackgroundColor(Some(&rgba(0.09, 0.09, 0.10, 0.86)));
        bg.setBorderColor(Some(&rgba(1.0, 1.0, 1.0, 0.14)));
        bg.setBorderWidth(0.5);
        bg.setContentsScale(scale);

        let dot = CALayer::new();
        let text = CATextLayer::new();
        unsafe {
            text.setFont(Some(&*Retained::as_ptr(&font).cast::<CFType>()));
            text.setAlignmentMode(kCAAlignmentLeft);
        }
        text.setFontSize(size);
        text.setForegroundColor(Some(&rgba(1.0, 1.0, 1.0, 0.96)));
        text.setContentsScale(scale);

        bg.addSublayer(&dot);
        bg.addSublayer(&text);
        parent.addSublayer(&bg);
        Pill { bg, dot, text, font, size }
    }

    /// Lay out the pill for `s`; returns its (width, height).
    fn set(&self, s: &str, dot: Option<&CGColor>) -> (f64, f64) {
        let ns = NSString::from_str(s);
        let m = measure(&ns, &self.font);

        let h = (self.size * 2.15).round();
        let pad = (self.size * 0.95).round();
        let d = (self.size * 0.6).round();
        let dot_w = if dot.is_some() { d + (self.size * 0.55).round() } else { 0.0 };
        let w = (pad * 2.0 + dot_w + m.width).ceil();

        let f = self.bg.frame();
        self.bg.setFrame(CGRect::new(f.origin, CGSize::new(w, h)));
        self.bg.setCornerRadius(h / 2.0);
        self.dot.setHidden(dot.is_none());
        if let Some(c) = dot {
            self.dot.setBackgroundColor(Some(c));
            self.dot.setCornerRadius(d / 2.0);
            self.dot.setFrame(CGRect::new(CGPoint::new(pad, ((h - d) / 2.0).round()), CGSize::new(d, d)));
        }
        self.text.setFrame(CGRect::new(
            CGPoint::new(pad + dot_w, ((h - m.height) / 2.0).round()),
            CGSize::new(m.width.ceil() + 1.0, m.height.ceil()),
        ));
        unsafe { self.text.setString(Some(ns.as_ref())) };
        (w, h)
    }

    fn place(&self, x: f64, y: f64) {
        let f = self.bg.frame();
        self.bg.setFrame(CGRect::new(CGPoint::new(x.round(), y.round()), f.size));
    }

    fn set_hidden(&self, hidden: bool) {
        self.bg.setHidden(hidden);
    }
}

// --- Selection overlay ------------------------------------------------------
//
// The overlay is a borderless *non-activating* NSPanel: it can become the key
// window (so it gets Esc/Space and mouse moves) without stealing activation
// from the app the user is working in. macOS 14+ refuses to let background
// apps activate themselves, so a regular window would never get the keyboard.

/// Input from an overlay panel, in points local to that panel.
pub enum OverlayInput {
    Move(f64, f64),
    Down(f64, f64),
    Drag(f64, f64),
    Up(f64, f64),
    Cancel,
    ToggleMode,
    FullScreen,
}

thread_local! {
    static OVERLAY_HANDLER: RefCell<Option<Box<dyn Fn(usize, OverlayInput)>>> = const { RefCell::new(None) };
}

/// Install the callback that receives `(overlay index, input)`.
pub fn set_overlay_handler(f: impl Fn(usize, OverlayInput) + 'static) {
    OVERLAY_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

fn emit(index: usize, input: OverlayInput) {
    OVERLAY_HANDLER.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f(index, input);
        }
    });
}

define_class!(
    // SAFETY: NSPanel has no subclassing requirements; we only override getters.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SnipOverlayPanel"]
    struct OverlayPanel;

    impl OverlayPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

struct ViewIvars {
    index: usize,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements; OverlayView has no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SnipOverlayView"]
    #[ivars = ViewIvars]
    struct OverlayView;

    impl OverlayView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            let (x, y) = self.local(e);
            emit(self.ivars().index, OverlayInput::Move(x, y));
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let (x, y) = self.local(e);
            emit(self.ivars().index, OverlayInput::Down(x, y));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            let (x, y) = self.local(e);
            emit(self.ivars().index, OverlayInput::Drag(x, y));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            let (x, y) = self.local(e);
            emit(self.ivars().index, OverlayInput::Up(x, y));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _e: &NSEvent) {
            emit(self.ivars().index, OverlayInput::Cancel);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            let input = match e.keyCode() {
                53 => OverlayInput::Cancel,          // Esc
                49 if !e.isARepeat() => OverlayInput::ToggleMode, // Space
                36 | 76 => OverlayInput::FullScreen, // Return / Enter
                _ => return,
            };
            emit(self.ivars().index, input);
        }

        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _e: &NSEvent) {
            NSCursor::crosshairCursor().set();
        }
    }
);

impl OverlayView {
    fn local(&self, e: &NSEvent) -> (f64, f64) {
        let p = self.convertPoint_fromView(e.locationInWindow(), None);
        (p.x, p.y)
    }
}

pub struct Overlay {
    panel: Retained<OverlayPanel>,
    dims: [Retained<CALayer>; 4],
    sel: Retained<CALayer>,
    size_pill: Pill,
    hint: Pill,
    size: (f64, f64),
}

impl Overlay {
    pub fn new(index: usize, screen: &Screen) -> Overlay {
        let mtm = mtm();
        let frame = to_ns_frame(screen.rect);
        let panel: Retained<OverlayPanel> = unsafe {
            msg_send![
                OverlayPanel::alloc(mtm),
                initWithContentRect: frame,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(false);
        panel.setLevel(OVERLAY_LEVEL);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setAcceptsMouseMovedEvents(true);
        panel.setFrame_display(frame, false);

        let bounds = CGRect::new(CGPoint::ZERO, CGSize::new(screen.rect.w, screen.rect.h));
        let view = OverlayView::alloc(mtm).set_ivars(ViewIvars { index });
        let view: Retained<OverlayView> = unsafe { msg_send![super(view), initWithFrame: bounds] };
        view.setWantsLayer(true);
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                bounds,
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::CursorUpdate
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&area);
        panel.setContentView(Some(&view));
        panel.makeFirstResponder(Some(&view));

        let host = view.layer().unwrap();
        let root = CALayer::new();
        root.setGeometryFlipped(!host.contentsAreFlipped());
        root.setFrame(bounds);
        host.addSublayer(&root);

        let dim = rgba(0.0, 0.0, 0.0, 0.38);
        let dims = std::array::from_fn(|_| {
            let l = CALayer::new();
            l.setBackgroundColor(Some(&dim));
            root.addSublayer(&l);
            l
        });
        let sel = CALayer::new();
        root.addSublayer(&sel);
        let size_pill = Pill::new(&root, 11.0, true, screen.scale);
        let hint = Pill::new(&root, 12.5, false, screen.scale);
        Overlay {
            panel,
            dims,
            sel,
            size_pill,
            hint,
            size: (screen.rect.w, screen.rect.h),
        }
    }

    /// Show the panel; `key` makes it receive the keyboard.
    pub fn show(&self, key: bool) {
        if key {
            self.panel.makeKeyAndOrderFront(None);
        } else {
            self.panel.orderFrontRegardless();
        }
        NSCursor::crosshairCursor().set();
    }

    pub fn make_key(&self) {
        self.panel.makeKeyWindow();
    }

    pub fn hide(&self) {
        self.panel.orderOut(None);
    }

    pub fn update(&self, st: &OverlayState) {
        let (w, h) = self.size;
        no_anim(|| {
            let full = Rect::new(0.0, 0.0, w, h);
            let s = st.selection.map(|s| s.intersect(&full)).filter(|s| !s.is_empty());
            let rects = match s {
                None => [full, Rect::default(), Rect::default(), Rect::default()],
                Some(s) => [
                    Rect::new(0.0, 0.0, w, s.y),
                    Rect::new(0.0, s.y + s.h, w, h - s.y - s.h),
                    Rect::new(0.0, s.y, s.x, s.h),
                    Rect::new(s.x + s.w, s.y, w - s.x - s.w, s.h),
                ],
            };
            for (l, r) in self.dims.iter().zip(rects) {
                l.setFrame(cg(r));
            }

            let accent = if st.video { (1.0, 0.27, 0.23) } else { (0.04, 0.52, 1.0) };
            match s {
                Some(s) => {
                    self.sel.setHidden(false);
                    self.sel.setFrame(cg(s));
                    if st.is_window {
                        self.sel.setBackgroundColor(Some(&rgba(accent.0, accent.1, accent.2, 0.16)));
                        self.sel.setBorderColor(Some(&rgba(accent.0, accent.1, accent.2, 0.95)));
                        self.sel.setBorderWidth(2.0);
                    } else {
                        // Barely-there fill keeps the hole clickable.
                        self.sel.setBackgroundColor(Some(&rgba(1.0, 1.0, 1.0, 0.01)));
                        let c = if st.video { rgba(accent.0, accent.1, accent.2, 1.0) } else { rgba(1.0, 1.0, 1.0, 0.95) };
                        self.sel.setBorderColor(Some(&c));
                        self.sel.setBorderWidth(if st.video { 2.0 } else { 1.0 });
                    }

                    let label = format!("{} × {}", s.w.round() as i64, s.h.round() as i64);
                    let (pw, ph) = self.size_pill.set(&label, None);
                    let mut x = s.x + s.w - pw;
                    let mut y = s.y + s.h + 8.0;
                    if y + ph > h - 4.0 {
                        y = s.y + s.h - ph - 8.0;
                        x -= 8.0;
                    }
                    self.size_pill.place(x.max(4.0), y.max(4.0));
                    self.size_pill.set_hidden(st.is_window);
                }
                None => {
                    self.sel.setHidden(true);
                    self.size_pill.set_hidden(true);
                }
            }

            self.hint.set_hidden(!st.show_hint);
            if st.show_hint {
                let text = if st.video {
                    "Record video  ·  Drag or click a window  ·  Space: screenshot  ·  Esc"
                } else {
                    "Screenshot  ·  Drag or click a window  ·  Space: video  ·  Esc"
                };
                let dot = if st.video { rgba(1.0, 0.27, 0.23, 1.0) } else { rgba(1.0, 1.0, 1.0, 0.95) };
                let (pw, ph) = self.hint.set(text, Some(&dot));
                // Move out of the way if the cursor approaches.
                let top = 44.0;
                let y = if st.cursor.1 < top + ph + 40.0 && (st.cursor.0 - w / 2.0).abs() < pw / 2.0 + 40.0 {
                    h - top - ph
                } else {
                    top
                };
                self.hint.place((w - pw) / 2.0, y);
            }
        });
    }
}

// --- Floating pill window (recording HUD, toasts) -----------------------------

pub struct PillWindow {
    window: Retained<NSWindow>,
    pill: Pill,
}

impl PillWindow {
    pub fn new(window: &Window, scale: f64, click_through: bool) -> PillWindow {
        let w = float_window(window, Rect::new(0.0, 0.0, 10.0, 10.0), click_through);
        let root = root_layer(window);
        root.setFrame(CGRect::new(CGPoint::ZERO, CGSize::new(4000.0, 200.0)));
        let pill = Pill::new(&root, 13.0, true, scale);
        pill.place(0.0, 0.0);
        PillWindow { window: w, pill }
    }

    /// Show `text` with the pill centred horizontally on `anchor_x`, top at `y`.
    pub fn show_at(&self, text: &str, red_dot: bool, anchor_x: f64, y: f64) -> (f64, f64) {
        let mut size = (0.0, 0.0);
        no_anim(|| {
            let dot = rgba(1.0, 0.27, 0.23, 1.0);
            size = self.pill.set(text, red_dot.then_some(&*dot));
        });
        let r = Rect::new((anchor_x - size.0 / 2.0).round(), y.round(), size.0, size.1);
        self.window.setFrame_display(to_ns_frame(r), true);
        self.window.orderFrontRegardless();
        size
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
    }
}

// --- Recording border -----------------------------------------------------------

pub struct BorderWindow {
    window: Retained<NSWindow>,
    root: Retained<CALayer>,
}

impl BorderWindow {
    pub fn new(window: &Window) -> BorderWindow {
        let w = float_window(window, Rect::new(0.0, 0.0, 10.0, 10.0), true);
        let root = root_layer(window);
        root.setBorderColor(Some(&rgba(1.0, 0.27, 0.23, 0.95)));
        root.setBorderWidth(2.0);
        root.setCornerRadius(3.0);
        BorderWindow { window: w, root }
    }

    /// Draws just outside `r` so the border never ends up in the recording.
    pub fn show(&self, r: Rect) {
        let r = r.outset(3.0);
        self.window.setFrame_display(to_ns_frame(r), true);
        no_anim(|| self.root.setFrame(CGRect::new(CGPoint::ZERO, CGSize::new(r.w, r.h))));
        self.window.orderFrontRegardless();
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
    }
}

// --- Images, clipboard, windows --------------------------------------------------

/// A captured frame; CGImage is immutable and safe to move across threads.
#[derive(Clone)]
pub struct Image(pub(super) CFRetained<CGImage>);
unsafe impl Send for Image {}

impl Image {
    pub fn size(&self) -> (f64, f64) {
        (CGImage::width(Some(&self.0)) as f64, CGImage::height(Some(&self.0)) as f64)
    }
}

/// Encode as PNG tagged with the right DPI so it pastes at its on-screen size.
pub fn encode_png(img: &Image, points_wide: f64) -> Option<Vec<u8>> {
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &img.0);
    let (pw, ph) = (rep.pixelsWide() as f64, rep.pixelsHigh() as f64);
    if pw < 1.0 || points_wide < 1.0 {
        return None;
    }
    let scale = (pw / points_wide).max(1.0);
    rep.setSize(NSSize::new(pw / scale, ph / scale));
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(data.to_vec())
}

/// Decode a PNG (e.g. one the editor produced).
pub fn decode_png(png: &[u8]) -> Option<Image> {
    let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(png))?;
    let cg = rep.CGImage()?;
    Some(Image(unsafe { CFRetained::retain(std::ptr::NonNull::from(&*cg)) }))
}

pub fn copy_png(png: &[u8]) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.setData_forType(Some(&NSData::with_bytes(png)), unsafe { NSPasteboardTypePNG });
}

/// Put a file on the clipboard so ⌘V pastes the file itself (Finder, Slack, …).
pub fn copy_file(path: &Path) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let items = NSArray::from_retained_slice(&[ProtocolObject::from_retained(url)]);
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.writeObjects(&items);
}

/// Normal app windows on screen, front to back, excluding our own.
pub fn window_list() -> Vec<WinInfo> {
    let me = std::process::id() as i64;
    let Some(arr) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        0,
    ) else {
        return vec![];
    };
    // CFArray of CFDictionary is toll-free bridged to NSArray of NSDictionary.
    let arr: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*CFRetained::as_ptr(&arr).as_ptr().cast() };
    let num = |d: &NSDictionary<NSString, AnyObject>, k: &NSString| -> Option<f64> {
        d.objectForKey(k)?.downcast::<NSNumber>().ok().map(|n| n.doubleValue())
    };
    let mut out = vec![];
    for d in arr.iter() {
        if num(&d, ns_string!("kCGWindowLayer")) != Some(0.0)
            || num(&d, ns_string!("kCGWindowOwnerPID")) == Some(me as f64)
            || num(&d, ns_string!("kCGWindowAlpha")).unwrap_or(1.0) <= 0.01
        {
            continue;
        }
        let Some(b) = d
            .objectForKey(ns_string!("kCGWindowBounds"))
            .and_then(|b| b.downcast::<NSDictionary>().ok())
        else {
            continue;
        };
        let b: &NSDictionary<NSString, AnyObject> = unsafe { &*Retained::as_ptr(&b).cast() };
        let rect = Rect::new(
            num(b, ns_string!("X")).unwrap_or(0.0),
            num(b, ns_string!("Y")).unwrap_or(0.0),
            num(b, ns_string!("Width")).unwrap_or(0.0),
            num(b, ns_string!("Height")).unwrap_or(0.0),
        );
        if rect.w < 40.0 || rect.h < 40.0 {
            continue;
        }
        if let Some(id) = num(&d, ns_string!("kCGWindowNumber")) {
            out.push(WinInfo { id: id as u32, rect });
        }
    }
    out
}

// --- Misc ------------------------------------------------------------------------

/// Run `f` inside an autorelease pool (for AppKit work on worker threads).
pub fn with_pool<R>(f: impl FnOnce() -> R) -> R {
    objc2::rc::autoreleasepool(|_| f())
}

/// The window number of a winit window, as ScreenCaptureKit and CoreGraphics know it.
pub fn window_id(window: &Window) -> u32 {
    ns_view(window).window().map_or(0, |w| w.windowNumber() as u32)
}

/// Native look for the settings and editor windows: sidebar vibrancy,
/// centred, and kept visible when we hand focus back after a capture.
pub fn prepare_app_window(window: &Window) {
    use window_vibrancy::{NSVisualEffectMaterial, NSVisualEffectState, apply_vibrancy};
    let _ = apply_vibrancy(window, NSVisualEffectMaterial::Sidebar, Some(NSVisualEffectState::FollowsWindowActiveState), None);
    let w = ns_view(window).window().unwrap();
    w.setCanHide(false);
    w.center();
}

/// Give focus back to whatever app was active before the overlay.
pub fn yield_focus() {
    let app = NSApplication::sharedApplication(mtm());
    if app.isActive() {
        // Hiding activates the next app in line; unhiding without activation
        // then lets our floating windows (toast, HUD) keep showing.
        app.hide(None);
        app.unhideWithoutActivation();
    }
}

thread_local! {
    static SOUND: RefCell<Option<Retained<NSSound>>> = const { RefCell::new(None) };
}

pub fn play_capture_sound() {
    SOUND.with(|s| {
        let mut s = s.borrow_mut();
        if s.is_none() {
            let path = "/System/Library/Components/CoreAudio.component/Contents/SharedSupport/SystemSounds/system/Screen Capture.aif";
            *s = if Path::new(path).exists() {
                NSSound::initWithContentsOfFile_byReference(
                    NSSound::alloc(),
                    &NSString::from_str(path),
                    true,
                )
            } else {
                NSSound::soundNamed(ns_string!("Tink"))
            };
        }
        if let Some(snd) = s.as_ref() {
            snd.stop();
            snd.play();
        }
    });
}

pub fn pick_folder(current: &Path) -> Option<PathBuf> {
    let panel = NSOpenPanel::openPanel(mtm());
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setCanCreateDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setPrompt(Some(ns_string!("Choose")));
    let url = NSURL::fileURLWithPath(&NSString::from_str(&current.to_string_lossy()));
    panel.setDirectoryURL(Some(&url));
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm()).activateIgnoringOtherApps(true);
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    panel.URL()?.path().map(|p| PathBuf::from(p.to_string()))
}

pub fn reveal(path: &Path) {
    let _ = Command::new("/usr/bin/open").arg("-R").arg(path).spawn();
}

pub fn open(path: &Path) {
    let _ = Command::new("/usr/bin/open").arg(path).spawn();
}

unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Whether we may capture other apps' windows. Without it every capture
/// fails, so check first and send the user to System Settings instead.
pub fn has_screen_access() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Shows the system prompt (first time only) and lists Snip in Settings.
pub fn request_screen_access() {
    unsafe { CGRequestScreenCaptureAccess() };
}

pub fn open_screen_access_settings() {
    let _ = Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}
