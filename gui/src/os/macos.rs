//! macOS helpers: reaching an already running instance (the shared Unix socket), file dialogs
//! through AppleScript, the Dock icon, icon sizes.

use std::path::PathBuf;
use std::process::{Command, Stdio};

pub use super::unix::{focus_existing, serve_show_requests};

/// Show `big` (RGBA, `bs`×`bs`, not premultiplied) as the Dock icon. winit ignores window icons
/// on macOS and eframe sets the application icon only at start, so this goes to AppKit
/// directly. Main thread only (it is called from the frame); elsewhere it does nothing.
pub fn set_window_icons(_hwnd: isize, _small: &[u8], _ss: u32, big: &[u8], bs: u32) {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSBitmapFormat, NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage};
    use objc2_foundation::NSSize;

    let Some(mtm) = MainThreadMarker::new() else { return };
    if big.len() != (bs * bs * 4) as usize {
        return;
    }
    let side = bs as isize;
    // SAFETY: with null planes the image rep allocates and owns its pixel buffer, of exactly
    // side × side × 4 bytes as described; `big` has that length and is copied into it.
    unsafe {
        let Some(rep) = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side,
            side,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            side * 4,
            32,
        ) else {
            return;
        };
        let data = rep.bitmapData();
        if data.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(big.as_ptr(), data, big.len());
        let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(bs as f64, bs as f64));
        image.addRepresentation(&rep);
        NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
    }
}

/// Windows-only fix for a winit quirk; nothing to do here.
pub fn full_client_area(_hwnd: isize) {}

/// Quote `s` as an AppleScript string literal.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Pick a skin file to open, or a place to save one, with the standard macOS panels
/// (`osascript`, part of every macOS). None if cancelled.
pub fn skin_file_dialog(save: bool, name: &str) -> Option<PathBuf> {
    let script = if save {
        format!(
            "POSIX path of (choose file name with prompt \"Skin file\" default name {})",
            applescript_string(name)
        )
    } else {
        "POSIX path of (choose file with prompt \"Skin file\" of type {\"public.json\"})".to_string()
    };
    let out = Command::new("osascript")
        .args(["-e", &script])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !path.is_empty()).then(|| PathBuf::from(path))
}

/// (menu bar, Dock) icon sizes in pixels: the menu bar item is drawn at 2x for Retina
/// screens and scaled to the bar's height; the Dock wants a large image.
pub fn icon_sizes() -> (u32, u32) {
    (44, 256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_quoting() {
        assert_eq!(applescript_string(r#"my "skin".json"#), r#""my \"skin\".json""#);
        assert_eq!(applescript_string(r"a\b"), r#""a\\b""#);
    }
}
