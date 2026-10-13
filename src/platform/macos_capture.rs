//! Screen capture with ScreenCaptureKit.
//!
//! * Screenshots: `SCScreenshotManager` grabs a display region or a single
//!   window. Snip's own windows (overlay, toast, recording HUD) are filtered
//!   out, so nothing has to be hidden first.
//! * Video: an `SCStream` with an `SCRecordingOutput` writes a QuickTime movie
//!   on the hardware H.264 encoder, with optional click highlighting and
//!   microphone audio.
//!
//! ScreenCaptureKit is asynchronous. Everything here blocks on its completion
//! handlers (which run on ScreenCaptureKit's own queues), so call it from a
//! worker thread, never the main one.

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{AVFileTypeQuickTimeMovie, AVVideoCodecTypeH264};
use objc2_core_foundation::{CFRetained, CGRect};
use objc2_core_graphics::CGImage;
use objc2_core_media::CMTime;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCDisplay, SCRecordingOutput, SCRecordingOutputConfiguration,
    SCRecordingOutputDelegate, SCScreenshotManager, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate,
};

use super::macos::{Image, cg};
use super::{Rect, Source};

/// How long to wait for ScreenCaptureKit before giving up.
const TIMEOUT: Duration = Duration::from_secs(10);
const FPS: i32 = 60;

/// Carries a ScreenCaptureKit result from its completion queue to the waiting thread.
struct Sendable<T>(T);
// SAFETY: only used for objects ScreenCaptureKit hands out from its own
// queues (shareable content, images), which are immutable once delivered.
unsafe impl<T> Send for Sendable<T> {}

type Reply<T> = mpsc::Sender<Sendable<Result<T, String>>>;

/// Start an asynchronous ScreenCaptureKit call and block until it replies.
fn wait<T>(start: impl FnOnce(Reply<T>)) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    start(tx);
    rx.recv_timeout(TIMEOUT).map_err(|_| "Screen capture didn't respond".to_string())?.0
}

fn describe(error: *mut NSError) -> String {
    match unsafe { error.as_ref() } {
        Some(e) => e.localizedDescription().to_string(),
        None => "Screen capture failed".into(),
    }
}

/// A completion handler that only reports success or an error.
fn done_block(tx: Reply<()>) -> RcBlock<dyn Fn(*mut NSError)> {
    RcBlock::new(move |error: *mut NSError| {
        let result = if error.is_null() { Ok(()) } else { Err(describe(error)) };
        let _ = tx.send(Sendable(result));
    })
}

fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    wait(|tx| {
        let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = unsafe { Retained::retain(content) }.ok_or_else(|| describe(error));
            let _ = tx.send(Sendable(result));
        });
        // Off-screen windows too, so Snip is listed (and excluded) even when
        // none of its windows happen to be showing.
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                false, false, &block,
            )
        };
    })
}

fn to_rect(r: CGRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

/// The display showing most of `r`.
fn display_for(content: &SCShareableContent, r: Rect) -> Result<Retained<SCDisplay>, String> {
    let area = |d: &SCDisplay| {
        let i = to_rect(unsafe { d.frame() }).intersect(&r);
        i.w * i.h
    };
    unsafe { content.displays() }
        .iter()
        .max_by(|a, b| area(a).total_cmp(&area(b)))
        .ok_or_else(|| "No display to capture".into())
}

/// Everything on `display` except Snip's own windows, apart from those in `keep`.
fn display_filter(content: &SCShareableContent, display: &SCDisplay, keep: &[u32]) -> Retained<SCContentFilter> {
    let pid = std::process::id() as i32;
    let snip: Vec<_> = unsafe { content.applications() }.iter().filter(|a| unsafe { a.processID() } == pid).collect();
    let kept: Vec<_> = unsafe { content.windows() }.iter().filter(|w| keep.contains(&unsafe { w.windowID() })).collect();
    unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::from_retained_slice(&snip),
            &NSArray::from_retained_slice(&kept),
        )
    }
}

/// Configure `config` to capture `r` (global points) from `display` at full
/// pixel resolution. Returns the size in pixels.
fn set_region(
    config: &SCStreamConfiguration,
    display: &SCDisplay,
    filter: &SCContentFilter,
    r: Rect,
) -> (f64, f64) {
    let frame = to_rect(unsafe { display.frame() });
    let local = r.intersect(&frame).offset(-frame.x, -frame.y);
    let scale = unsafe { filter.pointPixelScale() } as f64;
    let size = ((local.w * scale).round(), (local.h * scale).round());
    unsafe {
        config.setSourceRect(cg(local));
        config.setWidth(size.0 as usize);
        config.setHeight(size.1 as usize);
    }
    size
}

/// Capture `source`, leaving out Snip's own windows except those in `keep`.
/// Blocks until the image is ready; call it from a worker thread.
pub fn capture(source: Source, keep: &[u32]) -> Result<Image, String> {
    let content = shareable_content()?;
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setShowsCursor(false);
        config.setCaptureResolution(SCCaptureResolutionType::Best);
    }
    let filter = match source {
        Source::Region(r) => {
            let display = display_for(&content, r)?;
            let filter = display_filter(&content, &display, keep);
            set_region(&config, &display, &filter, r);
            filter
        }
        Source::Window(id) => {
            let window = unsafe { content.windows() }
                .iter()
                .find(|w| unsafe { w.windowID() } == id)
                .ok_or("The window has closed")?;
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
            };
            let (r, scale) = unsafe { (filter.contentRect(), filter.pointPixelScale() as f64) };
            unsafe {
                config.setWidth((r.size.width * scale).round() as usize);
                config.setHeight((r.size.height * scale).round() as usize);
                config.setIgnoreShadowsSingleWindow(true);
            }
            filter
        }
    };
    wait(|tx| {
        let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
            let result = NonNull::new(image)
                .map(|p| Image(unsafe { CFRetained::retain(p) }))
                .ok_or_else(|| describe(error));
            let _ = tx.send(Sendable(result));
        });
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&filter, &config, Some(&block))
        };
    })
}

// --- Video ---------------------------------------------------------------------

#[derive(Default)]
struct Status {
    /// The app asked to stop.
    stop: bool,
    /// The movie file is complete.
    finished: bool,
    /// The recording thread has exited.
    exited: bool,
    /// The first thing that went wrong, if anything did.
    error: Option<String>,
}

/// State shared between the app, the recording thread and the delegate.
#[derive(Default)]
struct Shared {
    status: Mutex<Status>,
    changed: Condvar,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
        self.changed.notify_all();
    }

    fn fail(&self, error: String) {
        self.update(|s| {
            s.error.get_or_insert(error);
        });
    }

    /// Block until `done` holds, or `timeout` passes.
    fn wait_until(&self, timeout: Option<Duration>, done: impl Fn(&Status) -> bool) -> MutexGuard<'_, Status> {
        let guard = self.status.lock().unwrap();
        match timeout {
            Some(t) => self.changed.wait_timeout_while(guard, t, |s| !done(s)).unwrap().0,
            None => self.changed.wait_while(guard, |s| !done(s)).unwrap(),
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; RecorderDelegate has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "SnipRecorderDelegate"]
    #[ivars = Arc<Shared>]
    struct RecorderDelegate;

    unsafe impl NSObjectProtocol for RecorderDelegate {}

    unsafe impl SCStreamDelegate for RecorderDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().fail(error.localizedDescription().to_string());
        }
    }

    unsafe impl SCRecordingOutputDelegate for RecorderDelegate {
        #[unsafe(method(recordingOutput:didFailWithError:))]
        fn output_failed(&self, _output: &SCRecordingOutput, error: &NSError) {
            self.ivars().fail(error.localizedDescription().to_string());
        }

        #[unsafe(method(recordingOutputDidFinishRecording:))]
        fn output_finished(&self, _output: &SCRecordingOutput) {
            self.ivars().update(|s| s.finished = true);
        }
    }
);

impl RecorderDelegate {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        unsafe { msg_send![super(this), init] }
    }
}

/// A screen recording in progress. It runs on its own thread, which owns all
/// the ScreenCaptureKit objects, so the handle itself can move anywhere.
pub struct Recorder {
    shared: Arc<Shared>,
    pub path: PathBuf,
}

impl Recorder {
    /// Start recording `r` (global points) to `path`. Returns at once; if the
    /// recording can't start, `has_failed` turns true shortly after.
    pub fn start(r: Rect, path: &Path, show_clicks: bool, audio: bool, keep: Vec<u32>) -> std::io::Result<Recorder> {
        // ScreenCaptureKit only reports an unwritable file once recording
        // stops, so check now rather than lose the whole recording.
        std::fs::File::create(path).and_then(|_| std::fs::remove_file(path))?;
        let shared = Arc::new(Shared::default());
        let (s, p) = (shared.clone(), path.to_owned());
        std::thread::Builder::new().name("recorder".into()).spawn(move || {
            objc2::rc::autoreleasepool(|_| {
                if let Err(e) = record(&s, r, &p, show_clicks, audio, &keep) {
                    s.fail(e);
                }
            });
            s.update(|st| st.exited = true);
        })?;
        Ok(Recorder { shared, path: path.to_owned() })
    }

    /// True if ScreenCaptureKit stopped the recording (e.g. permission revoked).
    pub fn has_failed(&self) -> bool {
        self.shared.status.lock().unwrap().error.is_some()
    }

    /// Stop, then wait (blocking) until the movie file is complete.
    pub fn stop_and_wait(self) -> Result<PathBuf, String> {
        self.shared.update(|s| s.stop = true);
        let st = self.shared.wait_until(Some(TIMEOUT * 2), |s| s.exited);
        match &st.error {
            Some(e) => Err(e.clone()),
            None if !st.exited => Err("Recording didn't finish in time".into()),
            None if !self.path.exists() => Err("Recording failed".into()),
            None => Ok(self.path.clone()),
        }
    }
}

/// The recording thread: set up the stream, record until asked to stop, then
/// let the recording output finish the file.
fn record(shared: &Arc<Shared>, r: Rect, path: &Path, show_clicks: bool, audio: bool, keep: &[u32]) -> Result<(), String> {
    let content = shareable_content()?;
    let display = display_for(&content, r)?;
    let filter = display_filter(&content, &display, keep);
    let config = unsafe { SCStreamConfiguration::new() };
    let (w, h) = set_region(&config, &display, &filter, r);
    unsafe {
        // The H.264 encoder wants even dimensions.
        config.setWidth((w as usize / 2 * 2).max(2));
        config.setHeight((h as usize / 2 * 2).max(2));
        config.setMinimumFrameInterval(CMTime::new(1, FPS));
        config.setShowsCursor(true);
        config.setShowMouseClicks(show_clicks);
        config.setCaptureMicrophone(audio);
    }

    let delegate = RecorderDelegate::new(shared.clone());
    let output_config = unsafe { SCRecordingOutputConfiguration::new() };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    unsafe {
        output_config.setOutputURL(&url);
        output_config.setOutputFileType(AVFileTypeQuickTimeMovie.ok_or("QuickTime unavailable")?);
        output_config.setVideoCodecType(AVVideoCodecTypeH264.ok_or("H.264 unavailable")?);
    }
    let output = unsafe {
        SCRecordingOutput::initWithConfiguration_delegate(
            SCRecordingOutput::alloc(),
            &output_config,
            ProtocolObject::from_ref(&*delegate),
        )
    };
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*delegate)),
        )
    };
    unsafe { stream.addRecordingOutput_error(&output) }.map_err(|e| e.localizedDescription().to_string())?;
    wait(|tx| unsafe { stream.startCaptureWithCompletionHandler(Some(&done_block(tx))) })?;

    // Record until asked to stop, or until ScreenCaptureKit gives up.
    let failed = shared.wait_until(None, |s| s.stop || s.error.is_some()).error.is_some();
    if failed {
        return Ok(()); // the error is already recorded
    }
    wait(|tx| unsafe { stream.stopCaptureWithCompletionHandler(Some(&done_block(tx))) })?;
    // Stopping finalises the file; the delegate reports when it's done.
    let st = shared.wait_until(Some(TIMEOUT), |s| s.finished || s.error.is_some());
    if !st.finished && st.error.is_none() {
        return Err("Recording didn't finish in time".into());
    }
    Ok(())
}
