#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod icons;
mod platform;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use config::Config;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use platform::{OverlayState, Rect, Screen, WinInfo};
use tray_icon::menu::accelerator::Accelerator;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId};
use wry::{WebView, WebViewBuilder};

const SETTINGS_HTML: &str = include_str!("settings.html");
const TOAST_TIME: Duration = Duration::from_millis(1600);
const ERROR_TIME: Duration = Duration::from_millis(5000);
const PREVIEW_TIME: Duration = Duration::from_secs(6);
/// After the pointer leaves the card, it lingers this long.
const PREVIEW_LINGER: Duration = Duration::from_secs(3);

enum UserEvent {
    HotKey(GlobalHotKeyEvent),
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Ipc(String),
    Overlay(usize, platform::OverlayInput),
    Preview(platform::PreviewInput),
    ShotDone { rect: Rect, result: Result<Shot, String>, image: platform::Image },
    RecordingDone { rect: Rect, result: Result<PathBuf, String>, thumb: Option<platform::Image>, secs: u64 },
}

/// A finished screenshot. `path` is the saved file, or a cache copy when the
/// screenshot only went to the clipboard (so it can still be opened/dragged).
struct Shot {
    copied: bool,
    saved: bool,
    path: PathBuf,
}

/// What the preview card is currently showing.
struct Shown {
    path: PathBuf,
    video: bool,
    saved: bool,
    copied: bool,
    secs: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Shot,
    Video,
}

enum Target {
    Region(Rect),
    Window(WinInfo),
}

struct OverlayWin {
    ui: platform::Overlay,
    screen: Screen,
}

/// State of an open selection overlay.
struct Selection {
    mode: Mode,
    /// Overlay (screen) the cursor is on.
    active: usize,
    /// Cursor position, local to the active overlay.
    cursor: (f64, f64),
    /// Where the mouse went down, if it is down.
    anchor: Option<(f64, f64)>,
    dragging: bool,
    windows: Vec<WinInfo>,
    hover: Option<usize>,
}

struct Floating<T> {
    window: Window,
    ui: T,
}

struct Recording {
    recorder: platform::Recorder,
    rect: Rect,
    started: Instant,
    shown_secs: u64,
    hud_anchor: (f64, f64),
}

struct TrayUi {
    icon: TrayIcon,
    shot: MenuItem,
    record: MenuItem,
    stop: MenuItem,
}

struct App {
    cfg: Config,
    proxy: EventLoopProxy<UserEvent>,
    hotkeys: GlobalHotKeyManager,
    /// [screenshot, record] — what the config asks for.
    keys: [HotKey; 2],
    registered: Vec<HotKey>,
    tray: Option<TrayUi>,
    overlays: Vec<OverlayWin>,
    sel: Option<Selection>,
    recording: Option<Recording>,
    border: Option<Floating<platform::BorderWindow>>,
    hud: Option<Floating<platform::PillWindow>>,
    toast: Option<Floating<platform::PillWindow>>,
    toast_until: Option<Instant>,
    preview: Option<platform::PreviewCard>,
    shown: Option<Shown>,
    preview_until: Option<Instant>,
    /// Webview first so it drops before its window.
    settings: Option<(WebView, Window)>,
    startup_action: Option<String>,
}

/// Append a line to ~/Library/Logs/Snip.log (and stderr).
fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    let line = format!("{} {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), msg.as_ref());
    eprint!("{line}");
    let Some(home) = dirs::home_dir() else { return };
    let path = home.join("Library/Logs/Snip.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn parse_keys(cfg: &Config) -> Result<[HotKey; 2], String> {
    let a = HotKey::from_str(&cfg.screenshot_hotkey).map_err(|e| format!("Screenshot shortcut: {e}"))?;
    let b = HotKey::from_str(&cfg.record_hotkey).map_err(|e| format!("Record shortcut: {e}"))?;
    if a == b {
        return Err("Screenshot and video need different shortcuts".into());
    }
    Ok([a, b])
}

/// Where clipboard-only screenshots are kept so the preview can open/drag them.
fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("Snip")
}

fn format_duration(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn format_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{} KB", (b / 1024).max(1)),
    }
}

fn unique_path(dir: &Path, kind: &str, ext: &str) -> PathBuf {
    let base = format!("{kind} {}", chrono::Local::now().format("%Y-%m-%d at %H.%M.%S"));
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    p
}

fn float_attrs() -> WindowAttributes {
    Window::default_attributes()
        .with_title("Snip")
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_visible(false)
        .with_inner_size(LogicalSize::new(10.0, 10.0))
}

impl App {
    // --- hotkeys --------------------------------------------------------------

    fn register(&mut self, keys: [HotKey; 2]) -> Result<(), String> {
        let _ = self.hotkeys.unregister_all(&self.registered);
        self.registered.clear();
        for (k, name) in keys.into_iter().zip(["Screenshot", "Record"]) {
            self.hotkeys
                .register(k)
                .map_err(|e| format!("{name} shortcut is unavailable (used by another app?): {e}"))?;
            self.registered.push(k);
        }
        Ok(())
    }

    fn suspend_hotkeys(&mut self) {
        let _ = self.hotkeys.unregister_all(&self.registered);
        self.registered.clear();
    }

    /// Apply a config from the settings UI. Hotkey problems are reported but
    /// never leave the app without working shortcuts.
    fn apply_config(&mut self, mut new: Config) -> Result<(), String> {
        let mut result = Ok(());
        match parse_keys(&new) {
            Ok(keys) if keys != self.keys || self.registered.len() != 2 => {
                if let Err(e) = self.register(keys) {
                    let _ = self.register(self.keys);
                    new.screenshot_hotkey = self.cfg.screenshot_hotkey.clone();
                    new.record_hotkey = self.cfg.record_hotkey.clone();
                    result = Err(e);
                } else {
                    self.keys = keys;
                }
            }
            Ok(_) => {}
            Err(e) => {
                new.screenshot_hotkey = self.cfg.screenshot_hotkey.clone();
                new.record_hotkey = self.cfg.record_hotkey.clone();
                result = Err(e);
            }
        }
        self.cfg = new;
        if let Err(e) = self.cfg.save() {
            log(format!("saving config failed: {e}"));
        }
        self.update_menu();
        result
    }

    // --- tray -------------------------------------------------------------------

    fn build_tray(&mut self) {
        let accel = |s: &str| Accelerator::from_str(s).ok();
        let shot = MenuItem::with_id("shot", "Capture Screenshot", true, accel(&self.cfg.screenshot_hotkey));
        let record = MenuItem::with_id("record", "Record Video", true, accel(&self.cfg.record_hotkey));
        let stop = MenuItem::with_id("stop", "Stop Recording", false, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[
            &shot,
            &record,
            &stop,
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("shots_dir", "Open Screenshots Folder", true, None),
            &MenuItem::with_id("videos_dir", "Open Videos Folder", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("settings", "Settings…", true, None),
            &MenuItem::with_id("quit", "Quit Snip", true, None),
        ]);
        let builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Snip");
        // macOS tints template icons to match the menu bar.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icons::idle());
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icons::idle());
        let icon = builder
            .build()
            .expect("tray icon");
        self.tray = Some(TrayUi { icon, shot, record, stop });
    }

    fn update_menu(&self) {
        let Some(t) = &self.tray else { return };
        let accel = |s: &str| Accelerator::from_str(s).ok();
        let _ = t.shot.set_accelerator(accel(&self.cfg.screenshot_hotkey));
        let _ = t.record.set_accelerator(accel(&self.cfg.record_hotkey));
        let rec = self.recording.is_some();
        t.record.set_enabled(!rec);
        t.stop.set_enabled(rec);
        // While recording, a click on the icon stops instead of opening the menu.
        t.icon.set_show_menu_on_left_click(!rec);
        if rec {
            let _ = t.icon.set_icon(Some(icons::recording()));
        } else {
            #[cfg(target_os = "macos")]
            let _ = t.icon.set_icon_templated(Some(icons::idle()));
            #[cfg(not(target_os = "macos"))]
            let _ = t.icon.set_icon(Some(icons::idle()));
        }
    }

    // --- selection overlay ----------------------------------------------------------

    fn start_selection(&mut self, el: &ActiveEventLoop, mode: Mode) {
        if self.recording.is_some() {
            if mode == Mode::Video {
                self.stop_recording();
            }
            return;
        }
        if let Some(sel) = &mut self.sel {
            // Same hotkey again cancels; the other one switches mode.
            if sel.mode == mode {
                self.cancel_selection();
            } else {
                sel.mode = mode;
                self.redraw_all();
            }
            return;
        }

        // The card and toast would otherwise end up in the next capture.
        self.hide_preview();
        self.hide_toast();
        if !platform::has_screen_access() {
            log("selection blocked: no Screen Recording permission");
            platform::request_screen_access();
            platform::open_screen_access_settings();
            let (x, y) = platform::cursor_pos();
            self.show_error(
                el,
                "Turn on Snip under Screen Recording, then quit & reopen Snip",
                Rect::new(x, y, 1.0, 1.0),
            );
            return;
        }
        self.ensure_overlays();
        if self.overlays.is_empty() {
            return;
        }
        let windows = if self.cfg.highlight_windows { platform::window_list() } else { vec![] };
        log(format!("selection open: {} screen(s), {} window(s)", self.overlays.len(), windows.len()));
        let g = platform::cursor_pos();
        let active = self.overlays.iter().position(|o| o.screen.rect.contains(g)).unwrap_or(0);
        let origin = self.overlays[active].screen.rect;
        self.sel = Some(Selection {
            mode,
            active,
            cursor: (g.0 - origin.x, g.1 - origin.y),
            anchor: None,
            dragging: false,
            windows,
            hover: None,
        });
        self.update_hover();
        self.redraw_all();
        for (i, o) in self.overlays.iter().enumerate() {
            if i != active {
                o.ui.show(false);
            }
        }
        self.overlays[active].ui.show(true);
    }

    /// Overlay panels are created ahead of time (and rebuilt when displays
    /// change) so the hotkey shows them instantly.
    fn ensure_overlays(&mut self) {
        let screens = platform::screens();
        if !self.overlays.iter().map(|o| o.screen).eq(screens.iter().copied()) {
            self.overlays = screens
                .into_iter()
                .enumerate()
                .map(|(i, s)| OverlayWin { ui: platform::Overlay::new(i, &s), screen: s })
                .collect();
        }
    }

    fn update_hover(&mut self) {
        let Some(sel) = &mut self.sel else { return };
        if sel.anchor.is_some() {
            return;
        }
        let o = self.overlays[sel.active].screen.rect;
        let g = (sel.cursor.0 + o.x, sel.cursor.1 + o.y);
        sel.hover = sel.windows.iter().position(|w| w.rect.contains(g));
    }

    fn redraw(&self, i: usize) {
        let Some(sel) = &self.sel else { return };
        let o = &self.overlays[i];
        let mut st = OverlayState {
            selection: None,
            is_window: false,
            video: sel.mode == Mode::Video,
            show_hint: i == sel.active,
            cursor: sel.cursor,
        };
        if i == sel.active {
            if let (true, Some(a)) = (sel.dragging, sel.anchor) {
                st.selection = Some(Rect::from_points(a, sel.cursor));
            } else if let Some(h) = sel.hover {
                st.selection = Some(sel.windows[h].rect.offset(-o.screen.rect.x, -o.screen.rect.y));
                st.is_window = true;
            }
        }
        o.ui.update(&st);
    }

    fn redraw_all(&self) {
        for i in 0..self.overlays.len() {
            self.redraw(i);
        }
    }

    fn hide_overlays(&mut self) {
        for o in &self.overlays {
            o.ui.hide();
        }
        platform::yield_focus();
    }

    fn cancel_selection(&mut self) {
        self.sel = None;
        self.hide_overlays();
    }

    fn overlay_input(&mut self, el: &ActiveEventLoop, i: usize, input: platform::OverlayInput) {
        use platform::OverlayInput as In;
        let Some(sel) = &mut self.sel else { return };
        let Some(screen) = self.overlays.get(i).map(|o| o.screen.rect) else { return };
        let clamp = |x: f64, y: f64| (x.clamp(0.0, screen.w), y.clamp(0.0, screen.h));
        match input {
            In::Move(x, y) | In::Drag(x, y) => {
                if sel.active != i {
                    if sel.anchor.is_some() {
                        return; // a drag stays on the screen it started on
                    }
                    // Cursor moved to another display: hand it the keyboard.
                    let prev = sel.active;
                    sel.active = i;
                    self.overlays[i].ui.make_key();
                    self.redraw(prev);
                }
                let Some(sel) = &mut self.sel else { return };
                sel.cursor = clamp(x, y);
                if let Some(a) = sel.anchor {
                    if (a.0 - sel.cursor.0).abs() > 3.0 || (a.1 - sel.cursor.1).abs() > 3.0 {
                        sel.dragging = true;
                    }
                }
                self.update_hover();
                self.redraw(i);
            }
            In::Down(x, y) => {
                sel.active = i;
                sel.cursor = clamp(x, y);
                sel.anchor = Some(sel.cursor);
                sel.dragging = false;
            }
            In::Up(x, y) => {
                let Some(a) = sel.anchor else { return };
                let cursor = clamp(x, y);
                let target = if sel.dragging {
                    Target::Region(Rect::from_points(a, cursor).offset(screen.x, screen.y))
                } else if let Some(h) = sel.hover {
                    Target::Window(sel.windows[h].clone())
                } else {
                    Target::Region(screen)
                };
                self.finish(el, target);
            }
            In::Cancel => self.cancel_selection(),
            In::ToggleMode => {
                sel.mode = if sel.mode == Mode::Shot { Mode::Video } else { Mode::Shot };
                self.redraw_all();
            }
            In::FullScreen => {
                let r = self.overlays[sel.active].screen.rect;
                self.finish(el, Target::Region(r));
            }
        }
    }

    fn finish(&mut self, el: &ActiveEventLoop, target: Target) {
        let Some(sel) = self.sel.take() else { return };
        let rect = match &target {
            Target::Region(r) => *r,
            Target::Window(w) => w.rect,
        };
        match sel.mode {
            Mode::Shot => {
                // Grab while the overlay is still up: we capture *below* it.
                let below = self.overlays[sel.active].ui.window_number;
                let img = match &target {
                    Target::Region(r) => platform::capture_rect(*r, below),
                    Target::Window(w) => platform::capture_window(w.id),
                };
                self.hide_overlays();
                log(format!("capture {:?}: {}", rect, if img.is_some() { "ok" } else { "no image" }));
                let Some(img) = img else {
                    self.show_error(el, "Capture failed — allow Screen Recording in System Settings", rect);
                    return;
                };
                if self.cfg.play_sound {
                    platform::play_capture_sound();
                }
                let cfg = self.cfg.clone();
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let result = platform::with_pool(|| {
                        let png = platform::encode_png(&img, rect.w).ok_or("Couldn't encode image")?;
                        let copied = cfg.screenshot_action.copies();
                        if copied {
                            platform::copy_png(&png);
                        }
                        let saved = cfg.screenshot_action.saves();
                        let dir = if saved { cfg.screenshot_dir.clone() } else { cache_dir() };
                        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                        let path = unique_path(&dir, "Screenshot", "png");
                        std::fs::write(&path, &png).map_err(|e| e.to_string())?;
                        Ok(Shot { copied, saved, path })
                    });
                    let _ = proxy.send_event(UserEvent::ShotDone { rect, result, image: img });
                });
            }
            Mode::Video => {
                let rect = rect.intersect(&self.overlays[sel.active].screen.rect);
                self.hide_overlays();
                if rect.w < 16.0 || rect.h < 16.0 {
                    return;
                }
                self.start_recording(el, rect);
            }
        }
    }

    // --- recording -----------------------------------------------------------------

    fn start_recording(&mut self, el: &ActiveEventLoop, rect: Rect) {
        let rect = rect.round();
        let _ = std::fs::create_dir_all(&self.cfg.video_dir);
        let path = unique_path(&self.cfg.video_dir, "Recording", "mov");
        let recorder = match platform::Recorder::start(
            rect,
            &path,
            self.cfg.video_show_clicks,
            self.cfg.video_record_audio,
        ) {
            Ok(r) => {
                log(format!("recording started {rect:?} -> {path:?}"));
                r
            }
            Err(e) => {
                self.show_error(el, &format!("Couldn't start recording: {e}"), rect);
                return;
            }
        };

        let border = self.border.get_or_insert_with(|| {
            let window = el.create_window(float_attrs()).expect("border window");
            let ui = platform::BorderWindow::new(&window);
            Floating { window, ui }
        });
        border.ui.show(rect);

        // Put the stop pill just outside the recorded area when possible.
        let screen = screen_for(rect.center());
        let hud_h = 30.0;
        let below = rect.y + rect.h + 12.0;
        let above = rect.y - 12.0 - hud_h;
        let y = if below + hud_h < screen.y + screen.h - 4.0 {
            below
        } else if above > screen.y + 4.0 {
            above
        } else {
            rect.y + rect.h - hud_h - 12.0
        };
        let hud_anchor = (rect.center().0, y);
        let hud = self.hud.get_or_insert_with(|| {
            let window = el.create_window(float_attrs()).expect("hud window");
            window.set_cursor(CursorIcon::Pointer);
            let ui = platform::PillWindow::new(&window, screen_scale(rect.center()), false);
            Floating { window, ui }
        });
        hud.ui.show_at(&hud_text(0), true, hud_anchor.0, hud_anchor.1);

        self.recording = Some(Recording { recorder, rect, started: Instant::now(), shown_secs: 0, hud_anchor });
        self.update_menu();
    }

    fn stop_recording(&mut self) {
        let Some(mut rec) = self.recording.take() else { return };
        // Signal now, not on the worker thread, so quitting can't skip it.
        rec.recorder.stop();
        if let Some(b) = &self.border {
            b.ui.hide();
        }
        if let Some(h) = &self.hud {
            h.ui.hide();
        }
        self.update_menu();
        let proxy = self.proxy.clone();
        let rect = rec.rect;
        // The last frame doubles as the preview thumbnail.
        let thumb = platform::capture_screen_rect(rect);
        let secs = rec.started.elapsed().as_secs();
        std::thread::spawn(move || {
            let result = rec.recorder.wait();
            let _ = proxy.send_event(UserEvent::RecordingDone { rect, result, thumb, secs });
        });
    }

    // --- feedback ------------------------------------------------------------------

    fn show_toast(&mut self, el: &ActiveEventLoop, text: &str, near: Rect) {
        if !self.cfg.show_preview {
            self.toast(el, text, near, TOAST_TIME);
        }
    }

    // --- preview card ----------------------------------------------------------------

    fn show_preview(&mut self, shown: Shown, image: Option<&platform::Image>, near: Rect) {
        let card = self.preview.get_or_insert_with(|| platform::PreviewCard::new(screen_scale(near.center())));
        let (subtitle, primary, secondary) = preview_texts(&shown);
        let title = shown.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let badge = shown.video.then(|| format!("▶ {}", format_duration(shown.secs)));
        card.show(
            &platform::PreviewContent {
                image,
                title: &title,
                subtitle: &subtitle,
                badge: badge.as_deref(),
                primary,
                secondary,
                file: &shown.path,
            },
            platform::visible_area(near.center()),
        );
        self.shown = Some(shown);
        self.preview_until = Some(Instant::now() + PREVIEW_TIME);
    }

    fn refresh_preview(&self) {
        if let (Some(card), Some(shown)) = (&self.preview, &self.shown) {
            let (subtitle, primary, secondary) = preview_texts(shown);
            card.set_texts(&subtitle, primary, secondary, &shown.path);
        }
    }

    fn hide_preview(&mut self) {
        if let Some(card) = &self.preview {
            card.hide();
        }
        self.shown = None;
        self.preview_until = None;
    }

    fn preview_input(&mut self, el: &ActiveEventLoop, input: platform::PreviewInput) {
        use platform::{PreviewInput as In, PreviewPart as Part};
        let Some(shown) = &mut self.shown else { return };
        match input {
            In::Hover { inside, part } => {
                if let Some(card) = &self.preview {
                    card.set_hover(inside, part);
                }
                // Stay while hovered; linger briefly after the pointer leaves.
                self.preview_until = (!inside).then(|| Instant::now() + PREVIEW_LINGER);
            }
            In::Click(Part::Thumb) => {
                platform::open(&shown.path);
                self.hide_preview();
            }
            In::Click(Part::Primary) if shown.saved => {
                platform::reveal(&shown.path);
                self.hide_preview();
            }
            In::Click(Part::Primary) => {
                // Clipboard-only screenshot: move the cached copy to the save folder.
                let dir = self.cfg.screenshot_dir.clone();
                let dest = unique_path(&dir, "Screenshot", "png");
                let result = std::fs::create_dir_all(&dir).and_then(|_| std::fs::copy(&shown.path, &dest));
                match result {
                    Ok(_) => {
                        let _ = std::fs::remove_file(&shown.path);
                        shown.path = dest;
                        shown.saved = true;
                        self.refresh_preview();
                    }
                    Err(e) => {
                        let near = Rect::new(0.0, 0.0, 1.0, 1.0);
                        self.show_error(el, &format!("Couldn't save: {e}"), near);
                    }
                }
            }
            In::Click(Part::Secondary) => {
                if shown.video {
                    platform::copy_file(&shown.path);
                } else if let Ok(png) = std::fs::read(&shown.path) {
                    platform::copy_png(&png);
                }
                shown.copied = true;
                self.refresh_preview();
            }
            In::Click(Part::Close) | In::Dropped => self.hide_preview(),
        }
    }

    /// Errors always show, and stay up long enough to read.
    fn show_error(&mut self, el: &ActiveEventLoop, text: &str, near: Rect) {
        log(format!("error: {text}"));
        let text = match text.char_indices().nth(110) {
            Some((i, _)) => format!("{}…", &text[..i]),
            None => text.to_string(),
        };
        self.toast(el, &text, near, ERROR_TIME);
    }

    fn toast(&mut self, el: &ActiveEventLoop, text: &str, near: Rect, time: Duration) {
        let toast = self.toast.get_or_insert_with(|| {
            let window = el.create_window(float_attrs()).expect("toast window");
            let ui = platform::PillWindow::new(&window, screen_scale(near.center()), true);
            Floating { window, ui }
        });
        let s = screen_for(near.center());
        toast.ui.show_at(text, false, s.x + s.w / 2.0, s.y + s.h - 110.0);
        self.toast_until = Some(Instant::now() + time);
    }

    fn hide_toast(&mut self) {
        if let Some(toast) = &self.toast {
            toast.ui.hide();
        }
        self.toast_until = None;
    }

    // --- settings ------------------------------------------------------------------

    fn open_settings(&mut self, el: &ActiveEventLoop) {
        if let Some((_, w)) = &self.settings {
            w.focus_window();
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Snip Settings")
            .with_inner_size(LogicalSize::new(540.0, 812.0))
            .with_resizable(false)
            .with_visible(false);
        #[cfg(target_os = "macos")]
        let attrs = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true)
                .with_title_hidden(true)
        };
        let window = el.create_window(attrs).expect("settings window");
        platform::prepare_settings_window(&window);
        let proxy = self.proxy.clone();
        let webview = WebViewBuilder::new()
            .with_html(SETTINGS_HTML)
            .with_transparent(true)
            .with_ipc_handler(move |req| {
                let _ = proxy.send_event(UserEvent::Ipc(req.into_body()));
            })
            .build(&window)
            .expect("webview");
        window.set_visible(true);
        window.focus_window();
        self.settings = Some((webview, window));
    }

    fn push_settings(&self, error: Option<String>) {
        let Some((webview, _)) = &self.settings else { return };
        let payload = serde_json::json!({
            "config": self.cfg,
            "error": error,
            "version": env!("CARGO_PKG_VERSION"),
        });
        let _ = webview.evaluate_script(&format!("window.snip && window.snip.load({payload})"));
    }

    fn on_ipc(&mut self, el: &ActiveEventLoop, msg: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(msg) else { return };
        match v["type"].as_str().unwrap_or_default() {
            "ready" => self.push_settings(None),
            "save" => {
                if let Ok(new) = serde_json::from_value::<Config>(v["config"].clone()) {
                    let err = self.apply_config(new).err();
                    self.push_settings(err);
                }
            }
            "reset" => {
                let err = self.apply_config(Config::default()).err();
                self.push_settings(err);
            }
            "pick_folder" => {
                let video = v["field"] == "video_dir";
                let current = if video { &self.cfg.video_dir } else { &self.cfg.screenshot_dir };
                if let Some(p) = platform::pick_folder(current) {
                    let mut new = self.cfg.clone();
                    if video {
                        new.video_dir = p;
                    } else {
                        new.screenshot_dir = p;
                    }
                    let _ = self.apply_config(new);
                }
                self.push_settings(None);
                if let Some((_, w)) = &self.settings {
                    w.focus_window();
                }
            }
            "open_folder" => {
                let dir = if v["field"] == "video_dir" { &self.cfg.video_dir } else { &self.cfg.screenshot_dir };
                platform::open(dir);
            }
            "suspend_hotkeys" => self.suspend_hotkeys(),
            "resume_hotkeys" => {
                if let Err(e) = self.register(self.keys) {
                    self.push_settings(Some(e));
                }
            }
            "drag" => {
                if let Some((_, w)) = &self.settings {
                    let _ = w.drag_window();
                }
            }
            "close" => self.settings = None,
            "quit" => {
                self.stop_recording();
                el.exit();
            }
            _ => {}
        }
    }
}

/// Subtitle and button labels for the preview card.
fn preview_texts(s: &Shown) -> (String, &'static str, &'static str) {
    let dir = short_dir(&s.path);
    let subtitle = if s.video {
        let size = std::fs::metadata(&s.path).map(|m| format_size(m.len())).unwrap_or_default();
        let copied = if s.copied { " · copied" } else { "" };
        format!("{} · {size} · in {dir}{copied}", format_duration(s.secs))
    } else {
        match (s.copied, s.saved) {
            (true, true) => format!("Copied · saved to {dir}"),
            (true, false) => "Copied to clipboard — ⌘V to paste".to_string(),
            (false, true) => format!("Saved to {dir}"),
            (false, false) => "Not saved".to_string(),
        }
    };
    let primary = if s.saved { "Show in Finder" } else { "Save" };
    let secondary = if s.copied { "Copied ✓" } else { "Copy" };
    (subtitle, primary, secondary)
}

fn hud_text(secs: u64) -> String {
    format!("{}:{:02}  ·  Click to stop", secs / 60, secs % 60)
}

fn screen_for(p: (f64, f64)) -> Rect {
    let screens = platform::screens();
    screens
        .iter()
        .find(|s| s.rect.contains(p))
        .or(screens.first())
        .map(|s| s.rect)
        .unwrap_or_default()
}

fn screen_scale(p: (f64, f64)) -> f64 {
    let screens = platform::screens();
    screens.iter().find(|s| s.rect.contains(p)).or(screens.first()).map_or(2.0, |s| s.scale)
}

fn short_dir(p: &Path) -> String {
    p.parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "folder".into())
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, el: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init {
            return;
        }
        // The tray must be created once the app has finished launching.
        self.build_tray();
        self.ensure_overlays();
        if let Err(e) = self.register(self.keys) {
            log(format!("hotkeys: {e}"));
        }
        log(format!(
            "started v{} pid {} exe {:?}; screen access: {}; hotkeys registered: {}/2 ({}, {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            std::env::current_exe().unwrap_or_default(),
            platform::has_screen_access(),
            self.registered.len(),
            self.cfg.screenshot_hotkey,
            self.cfg.record_hotkey,
        ));
        if !platform::has_screen_access() {
            // First launch: show the system prompt and add Snip to the list.
            platform::request_screen_access();
        }
        let action = self.startup_action.take().unwrap_or_default();
        match action.as_str() {
            "--shot" => self.start_selection(el, Mode::Shot),
            "--record" => self.start_selection(el, Mode::Video),
            "--settings" => self.open_settings(el),
            // Dev aid: run the capture pipeline on a fixed region, no mouse needed.
            #[cfg(debug_assertions)]
            a if a.starts_with("--test-shot=") || a.starts_with("--test-record=") => {
                let n: Vec<f64> = a.split('=').nth(1).unwrap().split(',').filter_map(|v| v.parse().ok()).collect();
                let mode = if a.starts_with("--test-shot") { Mode::Shot } else { Mode::Video };
                self.start_selection(el, mode);
                self.finish(el, Target::Region(Rect::new(n[0], n[1], n[2], n[3])));
            }
            _ => {}
        }
    }

    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::HotKey(e) if e.state == HotKeyState::Pressed => {
                log(format!("hotkey pressed (id {})", e.id));
                if e.id == self.keys[0].id() {
                    self.start_selection(el, Mode::Shot);
                } else if e.id == self.keys[1].id() {
                    self.start_selection(el, Mode::Video);
                }
            }
            UserEvent::HotKey(_) => {}
            UserEvent::Menu(e) => match e.id.as_ref() {
                "shot" => self.start_selection(el, Mode::Shot),
                "record" => self.start_selection(el, Mode::Video),
                "stop" => self.stop_recording(),
                "shots_dir" => platform::open(&self.cfg.screenshot_dir),
                "videos_dir" => platform::open(&self.cfg.video_dir),
                "settings" => self.open_settings(el),
                "quit" => {
                    self.stop_recording();
                    el.exit();
                }
                _ => {}
            },
            UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) if self.recording.is_some() => self.stop_recording(),
            UserEvent::Tray(_) => {}
            UserEvent::Ipc(msg) => self.on_ipc(el, &msg),
            UserEvent::Overlay(i, input) => self.overlay_input(el, i, input),
            UserEvent::Preview(input) => self.preview_input(el, input),
            UserEvent::ShotDone { rect, result, image } => match result {
                Ok(shot) => {
                    let text = match (shot.saved, shot.copied) {
                        (true, true) => "✓  Copied & saved".to_string(),
                        (true, false) => format!("✓  Saved to {}", short_dir(&shot.path)),
                        _ => "✓  Copied to clipboard".to_string(),
                    };
                    self.show_toast(el, &text, rect);
                    if shot.saved && self.cfg.reveal_after_save {
                        platform::reveal(&shot.path);
                    }
                    if self.cfg.show_preview {
                        let shown = Shown { path: shot.path, video: false, saved: shot.saved, copied: shot.copied, secs: 0 };
                        self.show_preview(shown, Some(&image), rect);
                    }
                }
                Err(e) => self.show_error(el, &format!("Screenshot failed: {e}"), rect),
            },
            UserEvent::RecordingDone { rect, result, thumb, secs } => match result.inspect(|p| log(format!("recording saved {p:?}"))) {
                Ok(p) => {
                    let copied = self.cfg.video_copy_to_clipboard;
                    let text = if copied {
                        platform::copy_file(&p);
                        "✓  Video saved & copied".to_string()
                    } else {
                        format!("✓  Video saved to {}", short_dir(&p))
                    };
                    self.show_toast(el, &text, rect);
                    if self.cfg.reveal_after_save {
                        platform::reveal(&p);
                    }
                    if self.cfg.show_preview {
                        let shown = Shown { path: p, video: true, saved: true, copied, secs };
                        self.show_preview(shown, thumb.as_ref(), rect);
                    }
                }
                Err(e) => self.show_error(el, &format!("Recording failed: {e}"), rect),
            },
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, id: WindowId, ev: WindowEvent) {
        if self.hud.as_ref().is_some_and(|h| h.window.id() == id) {
            if let WindowEvent::MouseInput { state: ElementState::Pressed, .. } = ev {
                self.stop_recording();
                platform::yield_focus();
            }
            return;
        }
        if self.settings.as_ref().is_some_and(|(_, w)| w.id() == id) {
            if let WindowEvent::CloseRequested = ev {
                self.settings = None;
                // Shortcuts may have been suspended mid-edit.
                if self.registered.len() != 2 {
                    let _ = self.register(self.keys);
                }
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        let mut next: Option<Instant> = None;

        if let Some(t) = self.toast_until {
            if now >= t {
                self.hide_toast();
            } else {
                next = Some(t);
            }
        }

        if let Some(t) = self.preview_until {
            if now >= t {
                self.hide_preview();
            } else {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        }

        if let Some(rec) = &mut self.recording {
            if rec.recorder.has_exited() {
                log("screencapture exited on its own");
                // screencapture quit on its own (e.g. permission denied).
                self.stop_recording();
            } else {
                let secs = rec.started.elapsed().as_secs();
                if secs != rec.shown_secs {
                    rec.shown_secs = secs;
                    if let Some(h) = &self.hud {
                        h.ui.show_at(&hud_text(secs), true, rec.hud_anchor.0, rec.hud_anchor.1);
                    }
                }
                let tick = rec.started + Duration::from_secs(secs + 1);
                next = Some(next.map_or(tick, |n| n.min(tick)));
            }
        }

        el.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

fn main() {
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        // Menu-bar app: no Dock icon, no app switcher entry.
        builder
            .with_activation_policy(ActivationPolicy::Accessory)
            .with_activate_ignoring_other_apps(false);
    }
    let event_loop = builder.build().expect("event loop");
    let proxy = event_loop.create_proxy();

    let p = proxy.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(UserEvent::HotKey(e));
    }));
    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(UserEvent::Menu(e));
    }));
    let p = proxy.clone();
    platform::set_overlay_handler(move |i, input| {
        let _ = p.send_event(UserEvent::Overlay(i, input));
    });
    let p = proxy.clone();
    platform::set_preview_handler(move |input| {
        let _ = p.send_event(UserEvent::Preview(input));
    });
    let p = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(UserEvent::Tray(e));
    }));

    // Clipboard-only screenshots from earlier runs are no longer needed.
    let _ = std::fs::remove_dir_all(cache_dir());
    let cfg = Config::load();
    let keys = parse_keys(&cfg).unwrap_or_else(|_| parse_keys(&Config::default()).unwrap());
    let mut app = App {
        cfg,
        proxy,
        hotkeys: GlobalHotKeyManager::new().expect("hotkey manager"),
        keys,
        registered: vec![],
        tray: None,
        overlays: vec![],
        sel: None,
        recording: None,
        border: None,
        hud: None,
        toast: None,
        toast_until: None,
        preview: None,
        shown: None,
        preview_until: None,
        settings: None,
        startup_action: std::env::args().nth(1),
    };
    event_loop.run_app(&mut app).expect("run");
}
