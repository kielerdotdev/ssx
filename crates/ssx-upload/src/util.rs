//! Small helpers shared by several uploaders.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Everything except the RFC 3986 unreserved characters is percent-encoded, like ShareX's
/// `URLHelpers.URLEncode`.
const URL_ENCODE_SET: &AsciiSet =
    &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Percent-encode `s` (UTF-8, uppercase hex, unreserved characters untouched).
pub fn url_encode(s: &str) -> String {
    utf8_percent_encode(s, URL_ENCODE_SET).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_sharex_unreserved_set() {
        assert_eq!(url_encode("aZ09-._~"), "aZ09-._~");
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(url_encode(""), "");
    }
}
