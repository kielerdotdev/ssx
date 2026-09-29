//! `file://` URI handling for the portal's screenshot result.
//!
//! The portal answers with a URI such as `file:///home/me/Pictures/Screenshot%20from%202025.png`.
//! Paths on Linux are arbitrary byte strings, so decoding produces an [`OsString`] built
//! from raw bytes rather than going through UTF-8 (a file name that is not valid UTF-8
//! must still be readable and deletable).

use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

/// Why a portal URI could not be turned into a local path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum UriError {
    /// Scheme is not `file:`.
    #[error("portal returned a non-file URI: {0:?}")]
    NotFile(String),
    /// A `file://host/...` URI naming a remote host.
    #[error("portal returned a file URI for remote host {0:?}")]
    RemoteHost(String),
    /// A `%` not followed by two hex digits.
    #[error("invalid percent-escape in portal URI: {0:?}")]
    BadEscape(String),
    /// Empty path, relative path or embedded NUL.
    #[error("portal URI does not contain an absolute path: {0:?}")]
    BadPath(String),
}

/// Converts a `file://` URI to a local path, decoding `%XX` escapes.
///
/// Accepts `file:///abs`, `file://localhost/abs` and the (technically malformed but seen in
/// the wild) `file:/abs`. Query strings and fragments are **not** stripped: a conforming
/// producer encodes `?` and `#` inside file names, so a literal one is part of the name.
pub(crate) fn file_uri_to_path(uri: &str) -> Result<PathBuf, UriError> {
    let trimmed = uri.trim();
    let rest = strip_scheme(trimmed).ok_or_else(|| UriError::NotFile(uri.to_owned()))?;
    let path_part = if let Some(after) = rest.strip_prefix("//") {
        // authority form: `//host/path` (host may be empty)
        let (host, path) = match after.find('/') {
            Some(i) => (&after[..i], &after[i..]),
            None => (after, ""),
        };
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return Err(UriError::RemoteHost(host.to_owned()));
        }
        path
    } else {
        rest
    };
    let bytes = percent_decode(path_part).ok_or_else(|| UriError::BadEscape(uri.to_owned()))?;
    if bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(UriError::BadPath(uri.to_owned()));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// Removes a case-insensitive `file:` prefix.
fn strip_scheme(s: &str) -> Option<&str> {
    let (scheme, rest) = s.split_at_checked(5)?;
    scheme.eq_ignore_ascii_case("file:").then_some(rest)
}

/// Decodes `%XX` escapes. `None` on a malformed escape.
pub(crate) fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, fmt::Write, os::unix::ffi::OsStrExt, path::Path};

    use super::*;

    #[test]
    fn plain_and_localhost() {
        assert_eq!(file_uri_to_path("file:///tmp/a.png"), Ok(PathBuf::from("/tmp/a.png")));
        assert_eq!(file_uri_to_path("file://localhost/tmp/a.png"), Ok(PathBuf::from("/tmp/a.png")));
        assert_eq!(file_uri_to_path("FILE:/tmp/a.png"), Ok(PathBuf::from("/tmp/a.png")));
    }

    #[test]
    fn decodes_spaces_and_utf8() {
        assert_eq!(
            file_uri_to_path("file:///home/me/Pictures/Screenshot%20from%202025-01-01.png"),
            Ok(PathBuf::from("/home/me/Pictures/Screenshot from 2025-01-01.png"))
        );
        assert_eq!(
            file_uri_to_path("file:///tmp/%C3%A4%C3%B6%20%23%25.png"),
            Ok(PathBuf::from("/tmp/äö #%.png"))
        );
    }

    #[test]
    fn non_utf8_bytes_survive() {
        let p = file_uri_to_path("file:///tmp/%FF%FE.png").unwrap();
        assert_eq!(p.as_os_str().as_bytes(), b"/tmp/\xFF\xFE.png");
        assert_eq!(p.parent(), Some(Path::new("/tmp")));
        let _ = OsStr::from_bytes(b"x");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(file_uri_to_path("https://x/y.png"), Err(UriError::NotFile(_))));
        assert!(matches!(file_uri_to_path(""), Err(UriError::NotFile(_))));
        assert!(matches!(file_uri_to_path("fil"), Err(UriError::NotFile(_))));
        assert!(matches!(
            file_uri_to_path("file://otherhost/tmp/a.png"),
            Err(UriError::RemoteHost(_))
        ));
        assert!(matches!(file_uri_to_path("file:///tmp/a%2.png"), Err(UriError::BadEscape(_))));
        assert!(matches!(file_uri_to_path("file:///tmp/a%zz.png"), Err(UriError::BadEscape(_))));
        assert!(matches!(file_uri_to_path("file:///tmp/a%"), Err(UriError::BadEscape(_))));
        assert!(matches!(file_uri_to_path("file:///tmp/a%00b"), Err(UriError::BadPath(_))));
        assert!(matches!(file_uri_to_path("file://"), Err(UriError::BadPath(_))));
        assert!(matches!(file_uri_to_path("file:relative/a.png"), Err(UriError::BadPath(_))));
        // multi-byte char straddling the scheme length must not panic
        assert!(matches!(file_uri_to_path("fileé/x"), Err(UriError::NotFile(_))));
    }

    #[test]
    fn literal_query_and_hash_are_part_of_the_name() {
        assert_eq!(file_uri_to_path("file:///tmp/a?b#c.png"), Ok(PathBuf::from("/tmp/a?b#c.png")));
    }

    #[test]
    fn decode_roundtrip_property() {
        // every byte value survives encode -> decode
        let mut enc = String::new();
        for b in 0u8..=255 {
            write!(enc, "%{b:02x}").unwrap();
        }
        let dec = percent_decode(&enc).unwrap();
        assert_eq!(dec, (0u8..=255).collect::<Vec<_>>());
    }
}
