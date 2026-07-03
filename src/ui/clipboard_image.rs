//! Read an image from the system clipboard by shelling out to the
//! platform's clipboard tool. PNG only (v1 design); no Cargo clipboard
//! dependency. The user must have the tool installed — a missing tool
//! or a clipboard without an image yields `None` (graceful), so the
//! paste binding can fall back to plain text.
//!
//! - macOS: `pngpaste <tmpfile>` (writes the clipboard image to a file)
//! - Linux/Wayland: `wl-paste -t image/png`
//! - Linux/X11: `xclip -selection clipboard -t image/png -o`

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
    // `pngpaste` has no stdout mode; write to a temp file, read it,
    // then remove it. Non-zero exit (no image on clipboard / tool
    // missing) => None.
    let path = std::env::temp_dir().join(format!(
        "dirge-clip-{}",
        crate::agent::runner::uuid_v4_simple()
    ));
    let status = Command::new("pngpaste").arg(&path).status().ok()?;
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

#[cfg(not(any(target_os = "macos", unix)))]
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
