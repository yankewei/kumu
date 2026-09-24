//! macOS Dock icon.
//!
//! `cargo run` starts a bare Mach-O executable, not a `.app` bundle, so there
//! is no `Info.plist` for AppKit to read a `CFBundleIconFile` from and the Dock
//! falls back to the generic executable icon. GPUI exposes no API to change
//! that, so we set kumu's own `.icns` on the shared `NSApplication` at launch,
//! which updates the Dock even for an unbundled process.
//!
//! A shipped `.app` bundle still wins: AppKit only uses this while the process
//! has no bundle icon of its own.

/// Sets the Dock icon from the embedded `assets/AppIcon.icns`.
///
/// Must be called on the main thread (AppKit requirement).
#[cfg(target_os = "macos")]
pub fn install() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(bytes) = crate::assets::app_icon() else {
        eprintln!("[ui] dock icon asset AppIcon.icns missing");
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("[ui] dock icon must be set on the main thread");
        return;
    };
    let data = NSData::with_bytes(&bytes);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        eprintln!("[ui] dock icon failed to decode");
        return;
    };
    // A non-nil image argument is required; `setApplicationIconImage:` is the
    // only reason this block is unsafe.
    unsafe {
        NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
    }
}

/// No Dock icon to set off macOS.
#[cfg(not(target_os = "macos"))]
pub fn install() {}
