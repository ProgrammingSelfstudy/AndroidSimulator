//! Shows the guest's screen in a native window and sends mouse/keyboard input back.
//!
//! The video path is deliberately thin: scrcpy's H.264 packets are wrapped in `CMSampleBuffer`s
//! and handed to an `AVSampleBufferDisplayLayer`, which decodes on the hardware decoder and
//! presents immediately. No pixel data ever touches our code.
#[cfg(target_os = "macos")]
mod cm;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod stream;
#[cfg(target_os = "macos")]
mod window;

#[cfg(target_os = "macos")]
pub use window::{Display, DisplayConfig};

/// Which dropped files we accept.
pub mod drop {
    use std::path::{Path, PathBuf};

    /// `.apk` files only (case-insensitive). Split APKs/bundles need `adb install-multiple`.
    pub fn is_apk(p: &Path) -> bool {
        p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("apk"))
    }

    /// Decode a `file://` URL string (as found on the pasteboard) to a path. Handles
    /// `file://localhost/...` and percent-escapes. Returns `None` for non-file URLs and for
    /// file-reference URLs (`/.file/id=...`), which only AppKit can resolve.
    pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
        let rest = url.strip_prefix("file://")?;
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        if !rest.starts_with('/') || rest.starts_with("/.file/") {
            return None;
        }
        let bytes = rest.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        Some(PathBuf::from(String::from_utf8(out).ok()?))
    }

    pub fn only_apks(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
        paths.into_iter().filter(|p| is_apk(p)).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn accepts_apks_only() {
            assert!(is_apk(Path::new("/x/app.apk")));
            assert!(is_apk(Path::new("/x/APP.APK")));
            assert!(!is_apk(Path::new("/x/app.apks")));
            assert!(!is_apk(Path::new("/x/app.zip")));
            assert!(!is_apk(Path::new("/x/apk")));
            let v = only_apks(["/a.apk", "/b.txt", "/c.APK"].map(PathBuf::from));
            assert_eq!(v, vec![PathBuf::from("/a.apk"), PathBuf::from("/c.APK")]);
        }

        #[test]
        fn decodes_file_urls() {
            let f = file_url_to_path;
            assert_eq!(f("file:///Users/me/a.apk"), Some(PathBuf::from("/Users/me/a.apk")));
            assert_eq!(f("file://localhost/Users/me/a.apk"), Some(PathBuf::from("/Users/me/a.apk")));
            assert_eq!(f("file:///Users/me/My%20App%20%281%29.apk"), Some(PathBuf::from("/Users/me/My App (1).apk")));
            assert_eq!(f("file:///Users/me/%E5%BA%94%E7%94%A8.apk"), Some(PathBuf::from("/Users/me/应用.apk")));
            // references and non-file schemes are left to AppKit / rejected
            assert_eq!(f("file:///.file/id=6571367.2048"), None);
            assert_eq!(f("https://example.com/a.apk"), None);
            // trailing/truncated escapes don't panic
            assert_eq!(f("file:///a%2"), Some(PathBuf::from("/a%2")));
        }
    }
}

/// Pure mapping helpers (host-testable).
pub mod geometry {
    /// Map a point in a view of `view_w x view_h` (origin top-left) to device pixels of a video
    /// `vw x vh`, clamped to the frame. Returns `None` for degenerate sizes.
    pub fn view_to_device(x: f64, y: f64, view_w: f64, view_h: f64, vw: u32, vh: u32) -> Option<(i32, i32)> {
        if view_w <= 0.0 || view_h <= 0.0 || vw == 0 || vh == 0 {
            return None;
        }
        let dx = (x / view_w * vw as f64).floor().clamp(0.0, vw as f64 - 1.0);
        let dy = (y / view_h * vh as f64).floor().clamp(0.0, vh as f64 - 1.0);
        Some((dx as i32, dy as i32))
    }

    /// Window content size (points) that fits `vw x vh` on screen while keeping its aspect.
    pub fn fit_content(vw: u32, vh: u32, max_h: f64) -> (f64, f64) {
        if vw == 0 || vh == 0 {
            return (360.0, 640.0);
        }
        let aspect = vw as f64 / vh as f64;
        // Prefer logical points = pixels / 2 (typical Retina phone mirroring), capped by screen.
        let mut h = (vh as f64 / 2.0).min(max_h);
        h = h.max(320.0);
        (h * aspect, h)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_and_clamps() {
            assert_eq!(view_to_device(0.0, 0.0, 360.0, 640.0, 720, 1280), Some((0, 0)));
            assert_eq!(view_to_device(180.0, 320.0, 360.0, 640.0, 720, 1280), Some((360, 640)));
            assert_eq!(view_to_device(360.0, 640.0, 360.0, 640.0, 720, 1280), Some((719, 1279)));
            assert_eq!(view_to_device(-5.0, 9999.0, 360.0, 640.0, 720, 1280), Some((0, 1279)));
            assert_eq!(view_to_device(1.0, 1.0, 0.0, 640.0, 720, 1280), None);
            assert_eq!(view_to_device(1.0, 1.0, 360.0, 640.0, 0, 1280), None);
        }

        #[test]
        fn fits_aspect() {
            let (w, h) = fit_content(720, 1280, 2000.0);
            assert!((w / h - 720.0 / 1280.0).abs() < 1e-9);
            assert_eq!(h, 640.0);
            let (_, h) = fit_content(720, 1280, 500.0);
            assert_eq!(h, 500.0);
        }
    }
}
