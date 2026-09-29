//! `multipart/form-data` framing that composes with the streaming [`BodyPlan`].
//!
//! reqwest's own multipart support would work but cannot report progress and hides the
//! total length; we need both, so we frame the parts ourselves: text fields and the file
//! part header form the *prefix*, the closing boundary is the *suffix*, and the file itself
//! is streamed in between. Field names and file names are escaped the way browsers do
//! (WHATWG HTML §4.10.21.8): `"`, CR and LF become percent escapes.

use bytes::Bytes;
use rand::Rng as _;

use crate::body::{BodyPlan, Payload};

/// A file part description.
#[derive(Debug, Clone)]
pub struct FilePart {
    /// Form field name.
    pub field: String,
    /// File name shown to the server.
    pub filename: String,
    /// Part `Content-Type`.
    pub mime: String,
    /// The bytes.
    pub payload: Payload,
}

/// Percent-escape the characters that would break out of a quoted header parameter.
fn escape_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("%22"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out
}

/// Generate a boundary that cannot plausibly occur in the payload.
fn new_boundary() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    let mut b = String::from("----ssx");
    for _ in 0..32 {
        b.push(ALPHABET[rng.random_range(0..ALPHABET.len())] as char);
    }
    b
}

/// Build the `Content-Type` header value and body plan for a multipart request.
pub fn plan(fields: &[(String, String)], file: Option<FilePart>) -> (String, BodyPlan) {
    plan_with_boundary(&new_boundary(), fields, file)
}

/// Like [`plan`] with a caller supplied boundary (deterministic tests).
pub fn plan_with_boundary(
    boundary: &str,
    fields: &[(String, String)],
    file: Option<FilePart>,
) -> (String, BodyPlan) {
    let mut prefix: Vec<u8> = Vec::new();
    for (name, value) in fields {
        prefix.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                escape_param(name)
            )
            .as_bytes(),
        );
        prefix.extend_from_slice(value.as_bytes());
        prefix.extend_from_slice(b"\r\n");
    }
    let content_type = format!("multipart/form-data; boundary={boundary}");
    if let Some(f) = file {
        {
            prefix.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                    escape_param(&f.field),
                    escape_param(&f.filename),
                    f.mime.replace(['\r', '\n'], "")
                )
                .as_bytes(),
            );
            let suffix = format!("\r\n--{boundary}--\r\n");
            (content_type, BodyPlan::framed(Bytes::from(prefix), f.payload, Bytes::from(suffix)))
        }
    } else {
        prefix.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (content_type, BodyPlan::framed(Bytes::from(prefix), Payload::Empty, Bytes::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes_and_newlines() {
        assert_eq!(escape_param("a\"b\r\nc"), "a%22b%0D%0Ac");
        assert_eq!(escape_param("naïve ☃.png"), "naïve ☃.png");
    }

    #[test]
    fn length_accounts_for_all_framing() {
        let (ct, plan) = plan_with_boundary(
            "B",
            &[("k".into(), "v".into())],
            Some(FilePart {
                field: "file".into(),
                filename: "a.txt".into(),
                mime: "text/plain".into(),
                payload: Payload::Memory(Bytes::from_static(b"12345")),
            }),
        );
        assert_eq!(ct, "multipart/form-data; boundary=B");
        let expected = "--B\r\nContent-Disposition: form-data; name=\"k\"\r\n\r\nv\r\n\
                        --B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\n\
                        12345\r\n--B--\r\n";
        assert_eq!(plan.content_length(), expected.len() as u64);
    }

    #[test]
    fn boundaries_differ() {
        assert_ne!(new_boundary(), new_boundary());
    }
}
