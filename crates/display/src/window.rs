//! The AppKit side: window, video view, input events, and the manual event pump.
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use moniqi_scrcpy as sc;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSDragOperation, NSDraggingInfo,
    NSEvent, NSEventMask, NSEventModifierFlags, NSModalResponseOK, NSOpenPanel, NSPasteboard,
    NSPasteboardTypeFileURL, NSScreen, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSArray, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::drop::{file_url_to_path, only_apks};
use crate::geometry::{fit_content, view_to_device};
use crate::input::{option_shortcut, printable, special_key, Shortcut};
use crate::stream;

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

pub struct DisplayConfig {
    pub video_addr: String,
    pub control_addr: String,
    pub title: String,
}

/// State shared between the main thread (events) and the video thread.
pub struct Shared {
    pub control: Mutex<Option<std::net::TcpStream>>,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub connected: AtomicBool,
    pub quit: AtomicBool,
    /// Sample buffers handed to the display layer.
    pub frames: AtomicU64,
    /// Control messages written (touch, key, scroll, text).
    pub inputs: AtomicU64,
    /// APK files dropped on the window, waiting for the owner to install them.
    dropped: Mutex<Vec<PathBuf>>,
    /// One-line message for the owner to show (e.g. a rejected drop).
    notice: Mutex<Option<String>>,
    pressed: AtomicBool,
    layer: Retained<AnyObject>,
}

// SAFETY: the only non-Sync field is the display layer, and the only operations performed on
// it off the main thread are `status`/`flush`/`enqueueSampleBuffer:`, which AVFoundation
// documents as thread-safe.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    pub fn layer(&self) -> &AnyObject {
        &self.layer
    }

    fn send(&self, msg: Vec<u8>) {
        if let Some(c) = self.control.lock().unwrap().as_mut() {
            if c.write_all(&msg).is_ok() {
                self.inputs.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn position(&self, view: &NSView, event: &NSEvent) -> Option<sc::Position> {
        let (vw, vh) = (self.width.load(Ordering::Relaxed), self.height.load(Ordering::Relaxed));
        let p = view.convertPoint_fromView(event.locationInWindow(), None);
        let b = view.bounds();
        let (x, y) = view_to_device(p.x, p.y, b.size.width, b.size.height, vw, vh)?;
        Some(sc::Position { x, y, width: vw as u16, height: vh as u16 })
    }
}

fn debug_enabled() -> bool {
    std::env::var_os("MONIQI_DEBUG").is_some()
}

/// File paths on a pasteboard (drag or general). Tries, in order: `public.file-url` items
/// (decoded here; AppKit resolves file-reference URLs), then the legacy filename list.
fn paths_from_pasteboard(pb: &NSPasteboard) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(items) = pb.pasteboardItems() {
        for item in items.iter() {
            // SAFETY: NSPasteboardTypeFileURL is a valid pasteboard type constant.
            let Some(s) = (unsafe { item.stringForType(NSPasteboardTypeFileURL) }) else { continue };
            let s = s.to_string();
            let path = file_url_to_path(&s).or_else(|| {
                // `file:///.file/id=...`: only NSURL can turn a reference into a real path.
                let url = NSURL::URLWithString(&NSString::from_str(&s))?;
                url.filePathURL()?.path().map(|p| PathBuf::from(p.to_string()))
            });
            if let Some(p) = path {
                out.push(p);
            }
        }
    }
    if out.is_empty() {
        #[allow(deprecated)]
        // SAFETY: legacy but still-populated pasteboard type; the value is an NSArray of NSString.
        let list = unsafe { pb.propertyListForType(objc2_app_kit::NSFilenamesPboardType) };
        if let Some(arr) = list.and_then(|l| l.downcast::<NSArray>().ok()) {
            out.extend(arr.iter().filter_map(|o| o.downcast::<NSString>().ok()).map(|s| PathBuf::from(s.to_string())));
        }
    }
    if debug_enabled() {
        let types: Vec<String> = pb
            .types()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        eprintln!("display: pasteboard types={types:?} paths={out:?}");
    }
    out
}

fn dragged_paths(sender: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    paths_from_pasteboard(&sender.draggingPasteboard())
}

/// Hand APKs to the owner; say why anything else was ignored.
fn accept_paths(shared: &Shared, paths: Vec<PathBuf>) -> bool {
    let total = paths.len();
    let apks = only_apks(paths.clone());
    if apks.is_empty() {
        let first = paths.first().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        *shared.notice.lock().unwrap() = Some(match first {
            Some(n) => format!("not an .apk file: {n}"),
            None => "nothing to install: no file in the drop".to_string(),
        });
        return false;
    }
    if apks.len() < total {
        *shared.notice.lock().unwrap() = Some(format!("installing {} of {total} files (others are not .apk)", apks.len()));
    }
    shared.dropped.lock().unwrap().extend(apks);
    true
}

struct ViewIvars {
    shared: Arc<Shared>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "MoniqiVideoView"]
    #[ivars = ViewIvars]
    struct VideoView;

    impl VideoView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // Top-left origin, matching device coordinates.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            self.touch(e, sc::ACTION_DOWN);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            self.touch(e, sc::ACTION_MOVE);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            self.touch(e, sc::ACTION_UP);
        }

        // Right click = Back, middle click = Home (scrcpy convention).
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _e: &NSEvent) {
            self.key_tap(sc::keycode::BACK);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, _e: &NSEvent) {
            self.key_tap(sc::keycode::HOME);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, e: &NSEvent) {
            let shared = &self.ivars().shared;
            let Some(pos) = shared.position(self, e) else { return };
            // Precise (trackpad) deltas are in points; wheels in lines. Normalise to notches.
            let k = if e.hasPreciseScrollingDeltas() { 0.05 } else { 1.0 };
            let v = (e.scrollingDeltaY() * k) as f32;
            let h = (e.scrollingDeltaX() * k) as f32;
            shared.send(sc::scroll(pos, h, v, 0));
        }

        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            // Show the copy cursor for any file; a non-APK gets an explanation on drop instead
            // of a silent "not allowed" cursor.
            if dragged_paths(sender).is_empty() {
                NSDragOperation::None
            } else {
                NSDragOperation::Copy
            }
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_for_drag(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            true
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            accept_paths(&self.ivars().shared, dragged_paths(sender))
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            self.key(e, true);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, e: &NSEvent) {
            self.key(e, false);
        }
    }
);

impl VideoView {
    fn new(mtm: MainThreadMarker, frame: NSRect, shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { shared });
        // SAFETY: standard NSView initialiser on a freshly allocated instance.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn touch(&self, e: &NSEvent, action: u8) {
        let shared = &self.ivars().shared;
        let Some(pos) = shared.position(self, e) else { return };
        match action {
            sc::ACTION_DOWN => shared.pressed.store(true, Ordering::Relaxed),
            sc::ACTION_UP => {
                if !shared.pressed.swap(false, Ordering::Relaxed) {
                    return;
                }
            }
            _ => {
                if !shared.pressed.load(Ordering::Relaxed) {
                    return;
                }
            }
        }
        let pressure = if action == sc::ACTION_UP { 0.0 } else { 1.0 };
        let buttons = if action == sc::ACTION_UP { 0 } else { 1 };
        shared.send(sc::touch(action, sc::POINTER_FINGER, pos, pressure, buttons));
    }

    fn open_panel(&self) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(true);
        panel.setMessage(Some(&NSString::from_str("Choose APK file(s) to install")));
        // Modal panel on the main thread, which is where key events arrive.
        let resp = panel.runModal();
        if resp != NSModalResponseOK {
            return;
        }
        let paths: Vec<PathBuf> = panel
            .URLs()
            .iter()
            .filter_map(|u| u.path().map(|p| PathBuf::from(p.to_string())))
            .collect();
        accept_paths(&self.ivars().shared, paths);
    }

    fn key_tap(&self, code: u32) {
        let shared = &self.ivars().shared;
        shared.send(sc::key(sc::KEY_DOWN, code, 0, 0));
        shared.send(sc::key(sc::KEY_UP, code, 0, 0));
    }

    fn key(&self, e: &NSEvent, down: bool) {
        let shared = &self.ivars().shared;
        let flags = e.modifierFlags();
        let code = e.keyCode();

        if flags.contains(NSEventModifierFlags::Command) {
            if !down {
                return;
            }
            match code {
                // Cmd+W / Cmd+Q close the window (there is no menu bar to do it for us).
                13 | 12 => shared.quit.store(true, Ordering::Relaxed),
                // Cmd+O: choose APK(s) to install.
                31 => self.open_panel(),
                // Cmd+V: install APK(s) copied in Finder.
                9 => {
                    let pb = NSPasteboard::generalPasteboard();
                    let paths = paths_from_pasteboard(&pb);
                    if !paths.is_empty() {
                        accept_paths(shared, paths);
                    }
                }
                _ => {}
            }
            return;
        }
        if flags.contains(NSEventModifierFlags::Option) {
            if down && !e.isARepeat() {
                match option_shortcut(code) {
                    Some(Shortcut::Key(k)) => self.key_tap(k),
                    Some(Shortcut::Rotate) => shared.send(sc::rotate_device()),
                    None => {}
                }
            }
            return;
        }
        if let Some(k) = special_key(code) {
            let action = if down { sc::KEY_DOWN } else { sc::KEY_UP };
            shared.send(sc::key(action, k, if e.isARepeat() { 1 } else { 0 }, 0));
            return;
        }
        if down && !flags.contains(NSEventModifierFlags::Control) {
            if let Some(chars) = e.characters() {
                let s = chars.to_string();
                if let Some(t) = printable(&s) {
                    shared.send(sc::text(t));
                }
            }
        }
    }
}

pub struct Display {
    app: Retained<NSApplication>,
    window: Retained<NSWindow>,
    shared: Arc<Shared>,
    applied_size: (u32, u32),
    max_height: f64,
}

impl Display {
    /// Create the window and start the video thread. Must be called on the main thread.
    pub fn new(cfg: DisplayConfig) -> Result<Self> {
        let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("display must be created on the main thread"))?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.finishLaunching();
        app.activate();

        let cls = AnyClass::get(c"AVSampleBufferDisplayLayer")
            .ok_or_else(|| anyhow!("AVSampleBufferDisplayLayer not available"))?;
        // SAFETY: standard alloc/init of a CALayer subclass.
        let layer: Retained<AnyObject> = unsafe { msg_send![msg_send![cls, alloc], init] };
        let gravity = NSString::from_str("AVLayerVideoGravityResize");
        // SAFETY: the selector takes an NSString.
        unsafe {
            let _: () = msg_send![&*layer, setVideoGravity: &*gravity];
        }

        let shared = Arc::new(Shared {
            control: Mutex::new(None),
            width: AtomicU32::new(720),
            height: AtomicU32::new(1280),
            connected: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            inputs: AtomicU64::new(0),
            dropped: Mutex::new(Vec::new()),
            notice: Mutex::new(None),
            pressed: AtomicBool::new(false),
            layer,
        });

        let max_height = NSScreen::mainScreen(mtm)
            .map(|s| s.visibleFrame().size.height * 0.9)
            .unwrap_or(900.0);
        let (w, h) = fit_content(720, 1280, max_height);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));

        let view = VideoView::new(mtm, frame, shared.clone());
        // SAFETY: NSPasteboardTypeFileURL is a valid pasteboard type constant.
        view.registerForDraggedTypes(&NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]));
        // Layer-hosting view: set the layer first, then opt in.
        // SAFETY: `setLayer:` takes a CALayer; ours is an AVSampleBufferDisplayLayer.
        unsafe {
            let _: () = msg_send![&*view, setLayer: &*shared.layer];
        }
        view.setWantsLayer(true);

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // SAFETY: standard NSWindow initialiser.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: we keep the window alive ourselves; AppKit must not free it on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(&cfg.title));
        window.setContentAspectRatio(NSSize::new(720.0, 1280.0));
        window.setContentView(Some(&view));
        window.center();
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));

        stream::spawn(shared.clone(), cfg.video_addr, cfg.control_addr);

        Ok(Self { app, window, shared, applied_size: (720, 1280), max_height })
    }

    /// Process window-system events for up to `timeout`. This also services the main run
    /// loop, so Virtualization.framework callbacks keep arriving while the window is shown.
    pub fn pump(&mut self, timeout: Duration) {
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let until = NSDate::dateWithTimeIntervalSinceNow(left.as_secs_f64());
            // SAFETY: NSDefaultRunLoopMode is a valid mode constant.
            let ev = unsafe {
                self.app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&until),
                    NSDefaultRunLoopMode,
                    true,
                )
            };
            match ev {
                Some(e) => self.app.sendEvent(&e),
                None => break,
            }
            if Instant::now() >= end {
                break;
            }
        }
        self.app.updateWindows();
        self.follow_video_size();
    }

    /// Resize the window to match the guest's current video size (rotation, etc.).
    fn follow_video_size(&mut self) {
        let size = (self.shared.width.load(Ordering::Relaxed), self.shared.height.load(Ordering::Relaxed));
        if size == self.applied_size || size.0 == 0 || size.1 == 0 {
            return;
        }
        self.applied_size = size;
        let (w, h) = fit_content(size.0, size.1, self.max_height);
        self.window.setContentAspectRatio(NSSize::new(size.0 as f64, size.1 as f64));
        self.window.setContentSize(NSSize::new(w, h));
    }

    /// True once the user closed the window (or pressed Cmd+W / Cmd+Q).
    pub fn is_closed(&self) -> bool {
        self.shared.quit.load(Ordering::Relaxed) || (!self.window.isVisible() && !self.window.isMiniaturized())
    }

    /// APKs dropped on the window since the last call.
    pub fn take_dropped(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.shared.dropped.lock().unwrap())
    }

    /// A message worth showing the user (rejected drop, partial install), once.
    pub fn take_notice(&self) -> Option<String> {
        self.shared.notice.lock().unwrap().take()
    }

    pub fn set_title(&self, title: &str) {
        self.window.setTitle(&NSString::from_str(title));
    }

    pub fn is_minimized(&self) -> bool {
        self.window.isMiniaturized()
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    /// One-line health summary: frames enqueued and the layer's rendering status
    /// (0 unknown, 1 rendering, 2 failed — with the AVFoundation error).
    pub fn stats(&self) -> String {
        let layer = self.shared.layer();
        // SAFETY: plain property reads on the display layer.
        let (status, err): (isize, Option<Retained<AnyObject>>) =
            unsafe { (msg_send![layer, status], msg_send![layer, error]) };
        let err = err
            .map(|e| {
                let d: Retained<NSString> = unsafe { msg_send![&*e, localizedDescription] };
                d.to_string()
            })
            .unwrap_or_default();
        let (w, h) = (self.shared.width.load(Ordering::Relaxed), self.shared.height.load(Ordering::Relaxed));
        format!(
            "connected={} video={}x{} frames={} inputs={} layer_status={}{}",
            self.is_connected(),
            w,
            h,
            self.shared.frames.load(Ordering::Relaxed),
            self.shared.inputs.load(Ordering::Relaxed),
            status,
            if err.is_empty() { String::new() } else { format!(" error={err}") }
        )
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
    }
}
