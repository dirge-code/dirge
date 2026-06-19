//! `file://` URI ↔ filesystem path conversion with percent-encoding.
//!
//! Consolidates what previously lived in both `init.rs` and `client.rs`.
//! The encoding rule: preserve `unreserved` chars + `/` (path separator) +
//! `:` (Windows drive separators, though dirge isn't tested there).
//! Everything else gets `%XX` percent-encoded by raw byte; non-ASCII bytes
//! emit as percent-encoded UTF-8 sequences.
//!
//! Decoding is permissive: invalid `%XX` sequences pass through unchanged,
//! and the result is interpreted as a best-effort UTF-8 string (lossy on
//! ill-formed bytes).

use std::path::{Path, PathBuf};

use lsp_types::Uri;

/// Convert a path to a `file://` URI string. Always returns a string (never
/// fails); call [`path_to_file_uri`] when you need a parsed `Uri` and want
/// the parse error.
pub fn path_to_file_uri_string(path: &Path) -> String {
    let s = path.to_string_lossy();
    #[cfg(windows)]
    {
        win_path_to_uri(&s)
    }
    #[cfg(not(windows))]
    {
        let encoded = percent_encode_path(&s);
        if s.starts_with('/') {
            format!("file://{encoded}")
        } else {
            // Relative path or Windows-style. Emit with an extra `/` so the
            // result is parseable as a URI.
            format!("file:///{encoded}")
        }
    }
}

/// Windows path → `file://` URI. Handles the forms `canonicalize` and the
/// agent produce, which the generic (Unix) encoder mangled:
/// - `\\?\C:\dir\f`  (extended-length) → `file:///C:/dir/f`
/// - `C:\dir\f`                         → `file:///C:/dir/f`
/// - `\\?\UNC\srv\sh\f` / `\\srv\sh\f`  → `file://srv/sh/f`
///
/// Without this, the backslashes and the `\\?\` prefix were percent-encoded
/// (`file:///%5C%5C%3F%5CC:...`), so LSP servers never matched the opened
/// document and no diagnostics/symbols flowed back.
#[cfg(windows)]
fn win_path_to_uri(s: &str) -> String {
    // UNC share: the server name is the URI authority (no leading `/`).
    if let Some(rest) = s
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| s.strip_prefix(r"\\").filter(|_| !s.starts_with(r"\\?\")))
    {
        let body = rest.replace('\\', "/");
        return format!("file://{}", percent_encode_path(&body));
    }
    // Local volume: drop the `\\?\` verbatim prefix when present.
    let stripped = s.strip_prefix(r"\\?\").unwrap_or(s);
    let body = stripped.replace('\\', "/");
    let encoded = percent_encode_path(&body);
    if body.starts_with('/') {
        // Already an absolute slash path (e.g. a Unix-style path in a test).
        format!("file://{encoded}")
    } else {
        // Drive path (`C:/...`) or relative — needs the empty authority `/`.
        format!("file:///{encoded}")
    }
}

/// Whether `s` begins with a Windows drive prefix like `C:`.
#[cfg(windows)]
fn is_drive_prefixed(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Convert a path to a parsed [`Uri`]. Returns an I/O error wrapped around
/// the parse failure so callers can propagate it through their own error
/// chains.
pub fn path_to_file_uri(path: &Path) -> std::io::Result<Uri> {
    let s = path_to_file_uri_string(path);
    s.parse::<Uri>().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid path for file URI: {e}"),
        )
    })
}

/// Decode a `file://` URI back into a [`PathBuf`]. Returns `None` for any
/// scheme other than `file:` (e.g. `https://`).
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let trimmed = uri
        .strip_prefix("file://")
        .or_else(|| uri.strip_prefix("file:"))?;
    #[cfg(windows)]
    {
        Some(win_uri_to_path(trimmed))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(percent_decode(trimmed)))
    }
}

/// `file://`-body → Windows path. Inverse of [`win_path_to_uri`]:
/// - `/C:/dir/f` → `C:\dir\f`   (drive letter upper-cased so a server that
///    echoes a lowercase drive still matches the `canonicalize`d path)
/// - `srv/sh/f`  → `\\srv\sh\f` (UNC authority form)
/// - anything else (Unix-style absolute, relative) passes through unchanged.
#[cfg(windows)]
fn win_uri_to_path(trimmed: &str) -> PathBuf {
    let decoded = percent_decode(trimmed);
    // Drive form: leading `/` before a `X:` drive (from `file:///C:/...`).
    if let Some(rest) = decoded.strip_prefix('/')
        && is_drive_prefixed(rest)
    {
        let drive = rest.as_bytes()[0].to_ascii_uppercase() as char;
        let body = format!("{drive}{}", &rest[1..]);
        return PathBuf::from(body.replace('/', "\\"));
    }
    // UNC authority form: `srv/sh/...` (no leading slash, not a drive).
    if !decoded.starts_with('/') && decoded.contains('/') && !is_drive_prefixed(&decoded) {
        return PathBuf::from(format!(r"\\{}", decoded.replace('/', "\\")));
    }
    PathBuf::from(decoded)
}

/// Percent-encode `path` per RFC 3986. Slashes are preserved (path
/// separators). Conforms to `unreserved` + `/` + `:`.
pub fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        let safe =
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':');
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Permissive percent-decoder. Invalid `%XX` sequences pass through as-is.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_value(bytes[i + 1]);
            let lo = hex_value(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_chars_pass_through_unchanged() {
        let p = Path::new("/tmp/proj_v1.0-rc/main.rs");
        let s = path_to_file_uri_string(p);
        assert_eq!(s, "file:///tmp/proj_v1.0-rc/main.rs");
    }

    // Regression: paths containing URI-significant characters must be
    // percent-encoded. A `#` would otherwise terminate the path early and
    // produce a fragment.
    #[test]
    fn regression_special_chars_are_percent_encoded() {
        let p = Path::new("/tmp/proj #1/main rs");
        let s = path_to_file_uri_string(p);
        assert!(s.contains("%23"), "must encode '#' as %23: {s}");
        assert!(s.contains("%20"), "must encode space as %20: {s}");
    }

    #[test]
    fn round_trip_preserves_path() {
        for p in &[
            "/tmp/a/b/c.rs",
            "/tmp/with spaces/main.rs",
            "/tmp/with#hash/main.rs",
            "/tmp/with?q/main.rs",
        ] {
            let path = PathBuf::from(p);
            let uri = path_to_file_uri_string(&path);
            let decoded = uri_to_path(&uri).unwrap();
            assert_eq!(decoded, path, "round-trip failed for {p}");
        }
    }

    #[test]
    fn non_file_uri_returns_none() {
        assert!(uri_to_path("https://example.com").is_none());
        assert!(uri_to_path("not a uri").is_none());
    }

    #[test]
    fn invalid_percent_escape_passes_through() {
        // Stray `%` followed by non-hex must not panic; emit as-is.
        let s = percent_decode("hello%zz world");
        assert_eq!(s, "hello%zz world");
    }

    #[test]
    fn parses_to_lsp_types_uri() {
        let uri = path_to_file_uri(Path::new("/tmp/main.rs")).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/main.rs");
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_and_plain_drive_paths_to_uri() {
        assert_eq!(
            path_to_file_uri_string(Path::new(r"\\?\C:\Users\me\Bad.dfy")),
            "file:///C:/Users/me/Bad.dfy"
        );
        assert_eq!(
            path_to_file_uri_string(Path::new(r"C:\proj\a b\main.dfy")),
            "file:///C:/proj/a%20b/main.dfy"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_uri_round_trips() {
        let p = PathBuf::from(r"C:\Users\me\Spec.dfy");
        let uri = path_to_file_uri_string(&p);
        assert_eq!(uri, "file:///C:/Users/me/Spec.dfy");
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[cfg(windows)]
    #[test]
    fn windows_lowercase_drive_uri_normalizes_to_uppercase() {
        // A server echoing a lowercase drive must still resolve to the
        // canonical (upper-cased) path dirge opened.
        assert_eq!(
            uri_to_path("file:///c:/Users/me/Spec.dfy").unwrap(),
            PathBuf::from(r"C:\Users\me\Spec.dfy")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_path_round_trips() {
        let uri = path_to_file_uri_string(Path::new(r"\\server\share\f.dfy"));
        assert_eq!(uri, "file://server/share/f.dfy");
        assert_eq!(
            uri_to_path(&uri).unwrap(),
            PathBuf::from(r"\\server\share\f.dfy")
        );
    }

    #[test]
    fn multibyte_utf8_percent_encodes_per_byte() {
        let p = Path::new("/tmp/🦀.rs");
        let s = path_to_file_uri_string(p);
        // 🦀 is F0 9F A6 80 in UTF-8 → four %XX escapes.
        assert!(s.contains("%F0%9F%A6%80"), "got: {s}");
        // Round-trips.
        let decoded = uri_to_path(&s).unwrap();
        assert_eq!(decoded, p);
    }
}
