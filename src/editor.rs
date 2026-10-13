//! The screenshot editor: a WebView window (like Settings) for marking up a
//! screenshot with arrows, shapes, text and pixelation. The page and the
//! image are served over a private `snip://` scheme; the edited PNG comes
//! back over IPC and replaces the screenshot.

use std::borrow::Cow;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};
use wry::http::{Response, header};
use wry::{WebView, WebViewBuilder};

use crate::platform::{self, Rect};
use crate::{App, Shot, Shown, UserEvent, log};

const EDITOR_HTML: &str = include_str!("editor.html");
/// Height of the toolbar plus the margin around the image, in points.
const BAR_H: f64 = 42.0;
/// Small enough for any screen, wide enough for the whole toolbar.
const MIN_SIZE: (f64, f64) = (840.0, 420.0);

pub struct Editor {
    id: u64,
    /// Webview first so it drops before its window.
    webview: WebView,
    window: Window,
    path: PathBuf,
    saved: bool,
    copied: bool,
    /// Pixels per point, so the edited image still pastes at on-screen size.
    scale: f64,
}

impl Editor {
    pub fn window_id(&self) -> u32 {
        platform::window_id(&self.window)
    }
}

impl App {
    /// Open `path` (a PNG screenshot) in a new editor window.
    pub fn open_editor(&mut self, el: &ActiveEventLoop, path: PathBuf, saved: bool, copied: bool) {
        let (x, y) = platform::cursor_pos();
        let Some((pw, ph, scale)) = png_info(&path) else {
            self.show_error(el, "Couldn't open the screenshot for editing", Rect::new(x, y, 1.0, 1.0));
            return;
        };
        // Show the image at up to its on-screen size, within most of the screen.
        let (w, h) = (pw as f64 / scale, ph as f64 / scale);
        let area = platform::visible_area((x, y));
        let fit = (area.w * 0.85 / w).min((area.h * 0.85 - BAR_H) / h).min(1.0);
        let size = LogicalSize::new((w * fit + 48.0).max(MIN_SIZE.0), (h * fit + BAR_H + 40.0).max(MIN_SIZE.1));

        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let attrs = Window::default_attributes()
            .with_title(&name)
            .with_inner_size(size)
            .with_min_inner_size(LogicalSize::new(MIN_SIZE.0, MIN_SIZE.1))
            .with_visible(false);
        #[cfg(target_os = "macos")]
        let attrs = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true)
                .with_title_hidden(true)
        };
        let window = el.create_window(attrs).expect("editor window");
        platform::prepare_app_window(&window);

        let id = self.next_editor;
        self.next_editor += 1;
        let proxy = self.proxy.clone();
        let file = path.clone();
        let webview = WebViewBuilder::new()
            .with_custom_protocol("snip".into(), move |_, req| serve(req.uri().path(), &file))
            .with_url("snip://localhost/editor.html")
            .with_transparent(true)
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UserEvent::EditorIpc(id, req.into_body()));
            })
            .build(&window)
            .expect("webview");
        window.set_visible(true);
        window.focus_window();
        log(format!("editor open: {name} ({pw}x{ph} px @{scale}x)"));
        self.editors.push(Editor { id, webview, window, path, saved, copied, scale });
    }

    pub fn editor_ipc(&mut self, el: &ActiveEventLoop, id: u64, msg: &str) {
        let Some(i) = self.editors.iter().position(|e| e.id == id) else { return };
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(msg) else { return };
        let png = match v["png"].take() {
            serde_json::Value::String(s) => Some(s),
            _ => None,
        };
        let e = &self.editors[i];
        match v["type"].as_str().unwrap_or_default() {
            "ready" => {
                let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned());
                let payload = serde_json::json!({ "scale": e.scale, "name": name });
                let _ = e.webview.evaluate_script(&format!("window.editor && window.editor.load({payload})"));
            }
            "drag" => {
                let _ = e.window.drag_window();
            }
            "copy" => {
                let Some(png) = png else { return };
                let (scale, proxy) = (e.scale, self.proxy.clone());
                std::thread::spawn(move || {
                    let result = platform::with_pool(|| retag(&png, scale).map(|(png, _)| platform::copy_png(&png)));
                    let _ = proxy.send_event(UserEvent::EditorCopied(id, result));
                });
            }
            "done" => {
                // The window stays open (showing "Saving…") until the file is
                // written, so a failed save loses nothing.
                let Some(png) = png else { return };
                let (path, saved, copied, scale) = (e.path.clone(), e.saved, e.copied, e.scale);
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let result = platform::with_pool(|| {
                        let (png, image) = retag(&png, scale)?;
                        std::fs::write(&path, &png).map_err(|err| err.to_string())?;
                        // Keep the clipboard in step if the screenshot was copied.
                        if copied {
                            platform::copy_png(&png);
                        }
                        Ok(Shot { copied, saved, path, image })
                    });
                    let _ = proxy.send_event(UserEvent::EditSaved(id, result));
                });
            }
            "close" => self.close_editor(i),
            "error" => {
                let text = v["message"].as_str().unwrap_or("Editing failed").to_string();
                let (x, y) = platform::cursor_pos();
                self.show_error(el, &text, Rect::new(x, y, 1.0, 1.0));
            }
            _ => {}
        }
    }

    fn close_editor(&mut self, i: usize) {
        drop(self.editors.remove(i));
        // Back to whatever the user was doing, unless another Snip window is open.
        if self.editors.is_empty() && self.settings.is_none() {
            platform::yield_focus();
        }
    }

    pub fn edit_saved(&mut self, el: &ActiveEventLoop, id: u64, result: Result<Shot, String>) {
        let (x, y) = platform::cursor_pos();
        let near = Rect::new(x, y, 1.0, 1.0);
        let i = self.editors.iter().position(|e| e.id == id);
        match result {
            Ok(shot) => {
                log(format!("edits saved {:?}", shot.path));
                if let Some(i) = i {
                    self.close_editor(i);
                }
                let text = match (shot.saved, shot.copied) {
                    (true, true) => "✓  Edits saved & copied",
                    (true, false) => "✓  Edits saved",
                    _ => "✓  Edits copied",
                };
                self.show_toast(el, text, near);
                if self.cfg.show_preview {
                    let shown = Shown { path: shot.path, video: false, saved: shot.saved, copied: shot.copied, secs: 0 };
                    self.show_preview(shown, Some(&shot.image), near);
                }
            }
            Err(err) => {
                if let Some(e) = i.map(|i| &self.editors[i]) {
                    let _ = e.webview.evaluate_script("window.editor && window.editor.failed()");
                }
                self.show_error(el, &format!("Couldn't save edits: {err}"), near);
            }
        }
    }

    pub fn editor_copied(&mut self, el: &ActiveEventLoop, id: u64, result: Result<(), String>) {
        match result {
            Ok(()) => {
                if let Some(e) = self.editors.iter_mut().find(|e| e.id == id) {
                    e.copied = true;
                    let _ = e.webview.evaluate_script("window.editor && window.editor.copied()");
                }
            }
            Err(err) => {
                let (x, y) = platform::cursor_pos();
                self.show_error(el, &format!("Couldn't copy: {err}"), Rect::new(x, y, 1.0, 1.0));
            }
        }
    }

    /// Handles window events for editor windows; false if `id` isn't one.
    pub fn editor_window_event(&mut self, id: WindowId, ev: &WindowEvent) -> bool {
        let Some(e) = self.editors.iter().find(|e| e.window.id() == id) else { return false };
        if let WindowEvent::CloseRequested = ev {
            // The page asks about unsaved edits, then replies "close" or "done".
            let _ = e.webview.evaluate_script("window.editor && window.editor.requestClose()");
        }
        true
    }
}

/// The editor page and the screenshot it edits.
fn serve(path: &str, file: &Path) -> Response<Cow<'static, [u8]>> {
    let (body, mime): (Option<Cow<'static, [u8]>>, _) = match path {
        "/editor.html" => (Some(EDITOR_HTML.as_bytes().into()), "text/html"),
        "/image.png" => (std::fs::read(file).ok().map(Into::into), "image/png"),
        _ => (None, "text/plain"),
    };
    let res = Response::builder().header(header::CACHE_CONTROL, "no-store");
    match body {
        Some(body) => res.header(header::CONTENT_TYPE, mime).body(body),
        None => res.status(404).body(Cow::Borrowed(&[][..])),
    }
    .expect("valid response")
}

/// Decode the editor's PNG and re-encode it tagged with the screenshot's DPI.
fn retag(b64: &str, scale: f64) -> Result<(Vec<u8>, platform::Image), String> {
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| e.to_string())?;
    let image = platform::decode_png(&raw).ok_or("Couldn't read the edited image")?;
    let png = platform::encode_png(&image, image.size().0 / scale).ok_or("Couldn't encode image")?;
    Ok((png, image))
}

/// Pixel size of a PNG and its pixels per point (from the pHYs chunk; 1 when untagged).
fn png_info(path: &Path) -> Option<(u32, u32, f64)> {
    let mut head = Vec::new();
    std::fs::File::open(path).ok()?.take(64 * 1024).read_to_end(&mut head).ok()?;
    parse_png_info(&head)
}

fn parse_png_info(data: &[u8]) -> Option<(u32, u32, f64)> {
    if data.get(..8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let be = |i: usize| Some(u32::from_be_bytes(data.get(i..i + 4)?.try_into().ok()?));
    let (w, h) = (be(16)?, be(20)?);
    let mut scale = 1.0;
    let mut i = 8;
    while let (Some(len), Some(kind)) = (be(i), data.get(i + 4..i + 8)) {
        match kind {
            // Pixels per metre (unit 1) -> pixels per point, at 72 points per inch.
            b"pHYs" if data.get(i + 16) == Some(&1) => {
                scale = (be(i + 8)? as f64 * 0.0254 / 72.0 * 4.0).round() / 4.0;
                break;
            }
            b"IDAT" => break,
            _ => i += 12 + len as usize,
        }
    }
    Some((w, h, scale.max(1.0)))
}

#[cfg(test)]
mod tests {
    use super::parse_png_info;

    #[test]
    fn reads_size_and_retina_scale() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend([0, 0, 0, 13]);
        png.extend(b"IHDR");
        png.extend(800u32.to_be_bytes());
        png.extend(600u32.to_be_bytes());
        png.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]); // rest of IHDR + CRC
        png.extend([0, 0, 0, 9]);
        png.extend(b"pHYs");
        png.extend(5669u32.to_be_bytes()); // 144 dpi
        png.extend(5669u32.to_be_bytes());
        png.extend([1, 0, 0, 0, 0]); // unit + CRC
        assert_eq!(parse_png_info(&png), Some((800, 600, 2.0)));
    }

    #[test]
    fn untagged_png_is_one_to_one() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(10u32.to_be_bytes());
        png.extend(20u32.to_be_bytes());
        assert_eq!(parse_png_info(&png), Some((10, 20, 1.0)));
        assert_eq!(parse_png_info(b"GIF89a"), None);
    }
}
