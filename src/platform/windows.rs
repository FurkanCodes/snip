#![allow(dead_code)]
//! Windows backend — not implemented yet. Mirrors the macOS API so the app
//! compiles; every capture path reports "unsupported".
//!
//! Planned implementation:
//! * Overlay: layered `WS_EX_TOPMOST | WS_EX_TOOLWINDOW` window per monitor,
//!   drawn with Direct2D/DirectComposition (or `UpdateLayeredWindow`).
//! * Screenshot: DXGI Desktop Duplication or BitBlt from the screen DC.
//! * Clipboard: `CF_DIB` + PNG registered format; `CF_HDROP` for videos.
//! * Video: Windows.Graphics.Capture + Media Foundation H.264 sink writer.

use std::path::{Path, PathBuf};

use winit::window::Window;

use super::{OverlayState, PreviewContent, PreviewInput, PreviewPart, Rect, Screen, Source, WinInfo};

const UNSUPPORTED: &str = "not supported on this platform yet";

pub enum OverlayInput {
    Move(f64, f64),
    Down(f64, f64),
    Drag(f64, f64),
    Up(f64, f64),
    Cancel,
    ToggleMode,
    FullScreen,
}

pub fn set_overlay_handler(_f: impl Fn(usize, OverlayInput) + 'static) {}

pub fn screens() -> Vec<Screen> {
    vec![]
}

pub fn cursor_pos() -> (f64, f64) {
    (0.0, 0.0)
}

pub struct Overlay;

impl Overlay {
    pub fn new(_index: usize, _screen: &Screen) -> Overlay {
        Overlay
    }
    pub fn show(&self, _key: bool) {}
    pub fn make_key(&self) {}
    pub fn hide(&self) {}
    pub fn update(&self, _st: &OverlayState) {}
}

pub struct PillWindow;

impl PillWindow {
    pub fn new(_window: &Window, _scale: f64, _click_through: bool) -> PillWindow {
        PillWindow
    }
    pub fn show_at(&self, _text: &str, _red_dot: bool, _x: f64, _y: f64) -> (f64, f64) {
        (0.0, 0.0)
    }
    pub fn hide(&self) {}
}

pub struct BorderWindow;

impl BorderWindow {
    pub fn new(_window: &Window) -> BorderWindow {
        BorderWindow
    }
    pub fn show(&self, _r: Rect) {}
    pub fn hide(&self) {}
}

#[derive(Clone)]
pub struct Image;

impl Image {
    pub fn size(&self) -> (f64, f64) {
        (0.0, 0.0)
    }
}

pub fn visible_area(_p: (f64, f64)) -> Rect {
    Rect::default()
}

pub fn set_preview_handler(_f: impl Fn(PreviewInput) + 'static) {}

pub struct PreviewCard;

impl PreviewCard {
    pub fn new(_scale: f64) -> PreviewCard {
        PreviewCard
    }
    pub fn show(&self, _c: &PreviewContent, _area: Rect) {}
    pub fn set_texts(&self, _subtitle: &str, _primary: &str, _secondary: &str, _file: &Path) {}
    pub fn set_hover(&self, _inside: bool, _part: Option<PreviewPart>) {}
    pub fn hide(&self) {}
}

pub fn capture(_source: Source, _keep: &[u32]) -> Result<Image, String> {
    Err(UNSUPPORTED.into())
}

pub fn encode_png(_img: &Image, _points_wide: f64) -> Option<Vec<u8>> {
    None
}

pub fn decode_png(_png: &[u8]) -> Option<Image> {
    None
}

pub fn copy_png(_png: &[u8]) {}

pub fn copy_file(_path: &Path) {}

pub fn window_list() -> Vec<WinInfo> {
    vec![]
}

pub struct Recorder {
    pub path: PathBuf,
}

impl Recorder {
    pub fn start(_r: Rect, _path: &Path, _clicks: bool, _audio: bool, _keep: Vec<u32>) -> std::io::Result<Recorder> {
        Err(std::io::Error::other(UNSUPPORTED))
    }
    pub fn has_failed(&self) -> bool {
        true
    }
    pub fn stop_and_wait(self) -> Result<PathBuf, String> {
        Err(UNSUPPORTED.into())
    }
}

pub fn with_pool<R>(f: impl FnOnce() -> R) -> R {
    f()
}

pub fn window_id(_window: &Window) -> u32 {
    0
}

pub fn prepare_app_window(window: &Window) {
    #[cfg(target_os = "windows")]
    let _ = window_vibrancy::apply_mica(window, None);
    let _ = window;
}

pub fn yield_focus() {}

pub fn play_capture_sound() {}

pub fn pick_folder(_current: &Path) -> Option<PathBuf> {
    None
}

pub fn reveal(_path: &Path) {}

pub fn open(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    let _ = path;
}

pub fn has_screen_access() -> bool {
    true
}

pub fn request_screen_access() {}

pub fn open_screen_access_settings() {}
