//! Shared decode/normalize/re-encode seam for the in-place file-mutation
//! tools (`edit`, `edit_lines`, `apply_patch` update).
//!
//! Every one of these tools used to hand-roll the same three steps —
//! decode the on-disk bytes, normalize line endings to `\n` for matching,
//! re-encode on write-back — and the copies drifted. Each divergence was a
//! bug: `from_utf8_lossy` silently turning non-UTF-8 bytes into `U+FFFD`
//! far from the edit (dirge-yga0), a leading BOM left on line 1 so its hash
//! never matched what `read` showed (dirge-2hqv), and `has_crlf = any CRLF`
//! flipping every `\n` in a mixed-ending file to `\r\n` (dirge-k32l).
//!
//! Consolidating the seam here (dirge-ol03) means a mutator decodes with
//! [`decode_for_edit`], operates on the LF-normalized [`SourceText::content`],
//! then calls [`SourceText::reencode`] — it can't skip BOM handling or pick a
//! different line-ending policy by accident.

/// The line-ending shape of a file, detected at decode time so write-back can
/// restore it instead of blanket-normalizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    /// Only `\n` (or an empty/single-line file). Write back as-is.
    Lf,
    /// Only `\r\n`. Write back with every `\n` re-expanded to `\r\n`.
    Crlf,
    /// Both present — the file is already inconsistent. We can't preserve
    /// per-line endings through an edit that reshapes lines, so write back
    /// with the dominant ending and report the normalization.
    Mixed,
}

/// A file decoded for in-place editing: LF-normalized, BOM-stripped text plus
/// the metadata needed to faithfully re-encode it on write-back.
#[derive(Debug)]
pub(crate) struct SourceText {
    /// The editable text: valid UTF-8, any leading BOM removed, all line
    /// endings collapsed to `\n`. This is what the model reasoned about via
    /// `read`, so matching/hashing/splicing all happen against it.
    pub(crate) content: String,
    ending: LineEnding,
    /// Number of `\r\n` sequences seen — used to pick the dominant ending for
    /// a [`LineEnding::Mixed`] file.
    crlf_count: usize,
    /// Number of bare `\n` (not part of a `\r\n`) seen.
    lf_count: usize,
    /// True if the file began with a UTF-8 BOM (`U+FEFF`); re-prepended on
    /// write so we don't strip a marker the user's toolchain expects.
    had_bom: bool,
}

/// Decode on-disk bytes for an in-place edit.
///
/// Returns `Err` with a model-facing message when the bytes are not valid
/// UTF-8 — refusing is safer than `from_utf8_lossy`, which would rewrite the
/// whole file with `U+FFFD` in place of bytes the edit never touched
/// (dirge-yga0). A leading BOM is stripped so line 1 matches what `read`
/// showed (dirge-2hqv), and line endings are detected (dirge-k32l).
pub(crate) fn decode_for_edit(bytes: &[u8]) -> Result<SourceText, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        "file is not valid UTF-8; refusing to edit so pre-existing non-UTF-8 bytes aren't \
         corrupted into replacement characters. Use bash (sed/awk/perl) for binary or \
         legacy-encoded files."
            .to_string()
    })?;

    // Strip a single leading BOM, matching read.rs so line-1 hashes align.
    let (had_bom, text) = match text.strip_prefix('\u{FEFF}') {
        Some(rest) => (true, rest),
        None => (false, text),
    };

    // Count endings without allocating: every "\r\n" is a CRLF; every '\n'
    // not immediately preceded by '\r' is a bare LF.
    let crlf_count = text.matches("\r\n").count();
    let total_lf = text.matches('\n').count();
    let lf_count = total_lf - crlf_count;
    let ending = match (crlf_count, lf_count) {
        (0, _) => LineEnding::Lf,
        (_, 0) => LineEnding::Crlf,
        _ => LineEnding::Mixed,
    };

    let content = if crlf_count > 0 {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    };

    Ok(SourceText {
        content,
        ending,
        crlf_count,
        lf_count,
        had_bom,
    })
}

/// The line-ending policy applied to a write-back, surfaced to the model when
/// it isn't a plain round-trip.
pub(crate) struct Reencoded {
    /// The bytes to write (as a `String` — BOM and CRLF are both valid UTF-8).
    pub(crate) text: String,
    /// A human note when the write normalized a mixed-ending file, else `None`.
    pub(crate) note: Option<String>,
}

impl SourceText {
    /// Re-encode LF-normalized `new_content` back into the file's original
    /// shape: restore a stripped BOM and the detected line ending.
    ///
    /// A [`LineEnding::Mixed`] file can't have its per-line endings preserved
    /// once an edit reshapes the lines, so it's normalized to whichever ending
    /// dominated the original and the caller gets a `note` to surface.
    pub(crate) fn reencode(&self, new_content: &str) -> Reencoded {
        let (body, note) = match self.ending {
            LineEnding::Lf => (new_content.to_string(), None),
            LineEnding::Crlf => (new_content.replace('\n', "\r\n"), None),
            LineEnding::Mixed => {
                let crlf_dominant = self.crlf_count >= self.lf_count;
                let (body, style) = if crlf_dominant {
                    (new_content.replace('\n', "\r\n"), "CRLF")
                } else {
                    (new_content.to_string(), "LF")
                };
                (
                    body,
                    Some(format!(
                        "note: file had mixed line endings ({} CRLF, {} LF); \
                         normalized to {}.",
                        self.crlf_count, self.lf_count, style
                    )),
                )
            }
        };
        let text = if self.had_bom {
            format!("\u{FEFF}{body}")
        } else {
            body
        };
        Reencoded { text, note }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lf_file_round_trips_unchanged() {
        let src = decode_for_edit(b"a\nb\nc\n").unwrap();
        assert_eq!(src.content, "a\nb\nc\n");
        let out = src.reencode(&src.content);
        assert_eq!(out.text, "a\nb\nc\n");
        assert!(out.note.is_none());
    }

    #[test]
    fn crlf_file_normalizes_for_editing_and_restores_on_write() {
        let src = decode_for_edit(b"a\r\nb\r\nc\r\n").unwrap();
        // Model sees LF-only content.
        assert_eq!(src.content, "a\nb\nc\n");
        // Write-back restores CRLF everywhere, no note (pure file).
        let out = src.reencode("a\nB\nc\n");
        assert_eq!(out.text, "a\r\nB\r\nc\r\n");
        assert!(out.note.is_none());
    }

    // dirge-yga0: non-UTF-8 input is refused, not lossily mangled.
    #[test]
    fn non_utf8_bytes_are_refused() {
        // 0xFF is never valid UTF-8.
        let err = decode_for_edit(b"ok\n\xffbad\n").unwrap_err();
        assert!(err.contains("not valid UTF-8"), "got: {err}");
    }

    // dirge-2hqv: a leading BOM is stripped for editing/hashing and
    // re-prepended on write, so line-1 content matches what read showed.
    #[test]
    fn leading_bom_is_stripped_then_restored() {
        let src = decode_for_edit("\u{FEFF}first\nsecond\n".as_bytes()).unwrap();
        // Line 1 no longer carries the BOM — matches read.rs.
        assert_eq!(src.content, "first\nsecond\n");
        let out = src.reencode("FIRST\nsecond\n");
        assert_eq!(out.text, "\u{FEFF}FIRST\nsecond\n");
    }

    #[test]
    fn bom_with_crlf_restores_both() {
        let src = decode_for_edit("\u{FEFF}a\r\nb\r\n".as_bytes()).unwrap();
        assert_eq!(src.content, "a\nb\n");
        let out = src.reencode("a\nb\n");
        assert_eq!(out.text, "\u{FEFF}a\r\nb\r\n");
        assert!(out.note.is_none());
    }

    // dirge-k32l: a mixed-ending file is no longer wholesale-flipped to CRLF.
    // The dominant ending wins and the caller gets a note.
    #[test]
    fn mixed_endings_normalize_to_dominant_lf_with_note() {
        // 3 LF-only lines, 1 CRLF → LF dominates.
        let src = decode_for_edit(b"a\nb\nc\r\nd\n").unwrap();
        assert_eq!(src.content, "a\nb\nc\nd\n");
        let out = src.reencode("a\nb\nc\nd\n");
        // Not flipped to all-CRLF (the old bug); stays LF.
        assert_eq!(out.text, "a\nb\nc\nd\n");
        let note = out.note.expect("mixed file should report normalization");
        assert!(note.contains("mixed line endings"), "got: {note}");
        assert!(note.contains("LF"), "got: {note}");
    }

    #[test]
    fn mixed_endings_normalize_to_dominant_crlf_with_note() {
        // 3 CRLF, 1 LF → CRLF dominates.
        let src = decode_for_edit(b"a\r\nb\r\nc\r\nd\n").unwrap();
        assert_eq!(src.content, "a\nb\nc\nd\n");
        let out = src.reencode("a\nb\nc\nd\n");
        assert_eq!(out.text, "a\r\nb\r\nc\r\nd\r\n");
        assert!(out.note.unwrap().contains("CRLF"));
    }
}
