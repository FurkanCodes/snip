//! Everything OS-specific lives behind this module so the app logic in
//! `main.rs` stays portable. macOS is implemented; Windows is stubbed.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(target_os = "macos")]
mod macos_capture;
#[cfg(target_os = "macos")]
pub use macos_capture::*;
#[cfg(target_os = "macos")]
mod macos_preview;
#[cfg(target_os = "macos")]
pub use macos_preview::*;

#[cfg(not(target_os = "macos"))]
mod windows;
#[cfg(not(target_os = "macos"))]
pub use windows::*;

/// A rectangle in global screen points, origin at the top-left of the
/// primary display, y growing downwards.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn from_points(a: (f64, f64), b: (f64, f64)) -> Rect {
        Rect::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
    }

    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = (self.x + self.w).min(o.x + o.w);
        let b = (self.y + self.h).min(o.y + o.h);
        Rect::new(x, y, (r - x).max(0.0), (b - y).max(0.0))
    }

    pub fn offset(&self, dx: f64, dy: f64) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn outset(&self, d: f64) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    /// Snap outwards to whole points so recordings get integer geometry.
    pub fn round(&self) -> Rect {
        let x = self.x.floor();
        let y = self.y.floor();
        Rect::new(x, y, (self.x + self.w).ceil() - x, (self.y + self.h).ceil() - y)
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn is_empty(&self) -> bool {
        self.w < 1.0 || self.h < 1.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub rect: Rect,
    pub scale: f64,
}

/// An on-screen application window that can be captured by clicking it.
#[derive(Clone, Debug)]
pub struct WinInfo {
    pub id: u32,
    pub rect: Rect,
}

/// What a screenshot captures.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    /// A region in global points, on a single display.
    Region(Rect),
    /// One window, by window number, even where other windows cover it.
    Window(u32),
}

/// Clickable regions of the post-capture preview card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewPart {
    Thumb,
    Primary,
    Secondary,
    Close,
}

/// Input from the preview card.
#[derive(Debug)]
pub enum PreviewInput {
    /// Pointer moved: whether it is over the card, and over which part.
    Hover { inside: bool, part: Option<PreviewPart> },
    Click(PreviewPart),
    /// The thumbnail was dragged out and dropped somewhere.
    Dropped,
}

/// What the preview card shows.
pub struct PreviewContent<'a> {
    pub image: Option<&'a Image>,
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Small label over the thumbnail, e.g. a video duration.
    pub badge: Option<&'a str>,
    pub primary: &'a str,
    pub secondary: &'a str,
    /// File that dragging the thumbnail hands to other apps.
    pub file: &'a std::path::Path,
}

/// What the overlay should draw. All coordinates are local to the overlay
/// window (points, top-left origin).
pub struct OverlayState {
    pub selection: Option<Rect>,
    /// The selection is a hovered window rather than a dragged region.
    pub is_window: bool,
    pub video: bool,
    pub show_hint: bool,
    pub cursor: (f64, f64),
}
