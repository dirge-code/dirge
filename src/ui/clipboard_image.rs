//! Read an image from the system clipboard by shelling out to the
//! platform's clipboard tool. PNG only (v1 design); no Cargo clipboard
//! dependency. The user must have the tool installed — a missing tool
//! or a clipboard without an image yields `None` (graceful), so the
//! paste binding can fall back to plain text.
//!
//! - macOS: `osascript` reads the clipboard as `«class PNGf»` and writes
//!   it to a temp file — ships with every macOS, no `brew install pngpaste`
//! - Linux/Wayland: `wl-paste -t image/png`
//! - Linux/X11: `xclip -selection clipboard -t image/png -o`
//! - Windows: PowerShell + `System.Windows.Forms` clipboard, saved as PNG
//!   to a temp file (no native clipboard CLI ships with Windows).

#[cfg(any(unix, windows))]
use std::process::Command;

/// Hard cap on a pasted image (20 MiB). Matches the design doc and
/// keeps a giant screenshot from blowing the transcript.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// A clipboard image: raw PNG bytes + MIME type. `media_type` is
/// always `"image/png"` in v1.
pub struct ClipImage {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
}

/// Read a PNG from the clipboard. Returns `None` if no image is
/// present, the platform tool is missing, or the payload exceeds
/// [`MAX_IMAGE_BYTES`].
pub fn read_clipboard_image() -> Option<ClipImage> {
    let bytes = read_png_bytes()?;
    if !is_within_size_limit(&bytes) {
        return None;
    }
    Some(ClipImage {
        bytes,
        media_type: "image/png",
    })
}

/// True iff `bytes` is non-empty and within the 20 MiB cap. Split out
/// so the bound is unit-testable without a real clipboard.
pub fn is_within_size_limit(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES
}

#[cfg(target_os = "macos")]
fn read_png_bytes() -> Option<Vec<u8>> {
    // AppleScript coerces the clipboard to PNG (`«class PNGf»`) and
    // writes it to a temp file, which we read and remove. `osascript`
    // is a system binary present on every macOS, so there's nothing to
    // install (unlike `pngpaste`). The clipboard is coerced *before* the
    // file is opened, so a clipboard with no image errors out before any
    // temp file is created; any non-zero exit => None (text-paste
    // fallback).
    let path = std::env::temp_dir().join(format!(
        "dirge-clip-{}.png",
        crate::agent::runner::uuid_v4_simple()
    ));
    // `path` is embedded in an AppleScript string literal; escape the two
    // chars that are special there. A UUID temp path contains neither,
    // but escape defensively since the id still reaches the script.
    let as_path = path.to_str()?.replace('\\', "\\\\").replace('"', "\\\"");
    let status = Command::new("osascript")
        .args([
            "-e",
            "set thePng to (the clipboard as «class PNGf»)",
            "-e",
            &format!("set fh to open for access (POSIX file \"{as_path}\") with write permission"),
            "-e",
            "set eof fh to 0",
            "-e",
            "write thePng to fh",
            "-e",
            "close access fh",
        ])
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    bytes
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read_png_bytes() -> Option<Vec<u8>> {
    // Wayland first (newer), then X11.
    if let Some(b) = capture_stdout(&["wl-paste", "-t", "image/png"]) {
        return Some(b);
    }
    capture_stdout(&["xclip", "-selection", "clipboard", "-t", "image/png", "-o"])
}

#[cfg(all(unix, not(target_os = "macos")))]
fn capture_stdout(cmd: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(cmd[0]).args(&cmd[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

#[cfg(windows)]
fn read_png_bytes() -> Option<Vec<u8>> {
    // Windows ships no clipboard CLI, so shell out to PowerShell + the
    // .NET `System.Windows.Forms.Clipboard`. `-STA` is required: OLE
    // clipboard access throws from an MTA thread. `GetImage()` returns
    // null when no image is present (=> None, text-paste fallback).
    let path = std::env::temp_dir().join(format!(
        "dirge-clip-{}.png",
        crate::agent::runner::uuid_v4_simple()
    ));
    // Embed the path in a PowerShell single-quoted string; the only
    // special char there is `'` itself (escaped by doubling).
    let ps_path = path.to_str()?.replace('\'', "''");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         $i = [System.Windows.Forms.Clipboard]::GetImage(); \
         if (-not $i) {{ exit 1 }}; \
         $i.Save('{ps_path}', [System.Drawing.Imaging.ImageFormat]::Png); \
         exit 0"
    );
    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            script.as_str(),
        ])
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    bytes
}

#[cfg(not(any(target_os = "macos", unix, windows)))]
fn read_png_bytes() -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_rejected() {
        assert!(!is_within_size_limit(&[]));
    }

    #[test]
    fn small_payload_accepted() {
        assert!(is_within_size_limit(&[1, 2, 3]));
    }

    #[test]
    fn exactly_at_cap_accepted() {
        let bytes = vec![0u8; MAX_IMAGE_BYTES];
        assert!(is_within_size_limit(&bytes));
    }

    #[test]
    fn over_cap_rejected() {
        let bytes = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(!is_within_size_limit(&bytes));
    }
}
