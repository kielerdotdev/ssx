//! AWS Signature Version 4, implemented in-crate (no AWS SDK, no OpenSSL).
//!
//! Only header-based signing is implemented (`Authorization` header), which is what S3
//! `PutObject` needs. The low-level [`sign`] takes the request in *wire form* (path and
//! query exactly as they will be sent) so it can be verified against AWS's published test
//! vectors byte for byte; [`crate::s3`] builds on it.
//!
//! Canonicalisation rules that differ between S3 and every other service, and that
//! implementations commonly get wrong:
//! * **S3**: the path is *not* normalised (`a/../b` stays) and is URI-encoded exactly once
//!   (the wire path is already encoded, so it is decoded and re-encoded to normalise the
//!   hex case).
//! * **Other services**: dot segments and duplicate slashes are normalised and the path is
//!   encoded a second time (a wire `%20` becomes `%2520`).
//! * Query pairs are decoded, re-encoded with the unreserved set, then sorted by encoded
//!   name and value; a bare `lifecycle` becomes `lifecycle=`.
//! * Header values are trimmed and inner runs of spaces collapse to one.
//!
//! HMAC-SHA256 is a dozen lines over `sha2`, so it is written out (and tested against
//! RFC 4231) rather than pulling another dependency.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// Payload hash placeholder meaning "the body is not covered by the signature".
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// SHA-256 of the empty string (hex).
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// Access credentials.
#[derive(Clone)]
pub struct Credentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// STS session token, sent (and signed) as `x-amz-security-token`.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// What to sign.
#[derive(Debug, Clone)]
pub struct SigningInput<'a> {
    /// HTTP method, upper case.
    pub method: &'a str,
    /// Request path exactly as it will be sent (starting with `/`).
    pub path: &'a str,
    /// Query string without the leading `?` (may be empty).
    pub query: &'a str,
    /// Headers to sign (name, value). Must include `host`; names are lower-cased here.
    pub headers: &'a [(String, String)],
    /// Lower-case hex SHA-256 of the body, or [`UNSIGNED_PAYLOAD`].
    pub payload_hash: &'a str,
    /// Region, e.g. `us-east-1`.
    pub region: &'a str,
    /// Service, e.g. `s3`.
    pub service: &'a str,
    /// Signing time (must equal the `x-amz-date` header).
    pub time: DateTime<Utc>,
    /// Apply S3 canonicalisation rules.
    pub s3: bool,
}

/// Result of [`sign`], including intermediates for debugging and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The canonical request.
    pub canonical_request: String,
    /// The string to sign.
    pub string_to_sign: String,
    /// Lower-case hex signature.
    pub signature: String,
    /// `a;b;c` list of signed header names.
    pub signed_headers: String,
    /// Full `Authorization` header value.
    pub authorization: String,
}

/// `YYYYMMDD'T'HHMMSS'Z'`.
pub fn amz_date(t: DateTime<Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// SigV4 URI encoding: everything but unreserved characters (and `/` when `keep_slash`).
fn uri_encode(input: &[u8], keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input {
        if is_unreserved(b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16));
            let lo = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16));
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Remove `.` and `..` segments and collapse repeated slashes (non-S3 services).
fn normalize_path(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    let mut out = String::from("/");
    out.push_str(&stack.join("/"));
    if path.ends_with('/') && !stack.is_empty() {
        out.push('/');
    }
    out
}

/// The canonical URI for `path` (wire form).
pub fn canonical_uri(path: &str, s3: bool) -> String {
    let path = if path.is_empty() { "/" } else { path };
    if s3 {
        uri_encode(&percent_decode(path), true)
    } else {
        // Encoding the wire path again is what "encode twice" means.
        uri_encode(normalize_path(path).as_bytes(), true)
    }
}

/// The canonical query string for `query` (wire form, no leading `?`).
pub fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (uri_encode(&percent_decode(k), false), uri_encode(&percent_decode(v), false))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut list: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    // Repeated names are joined with commas in the order given.
    let mut merged: Vec<(String, String)> = Vec::new();
    for (k, v) in list {
        match merged.last_mut() {
            Some((lk, lv)) if *lk == k => {
                lv.push(',');
                lv.push_str(&v);
            }
            _ => merged.push((k, v)),
        }
    }
    let canonical: String = merged.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = merged.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    (canonical, signed)
}

/// Compute the SigV4 signature for `input`.
pub fn sign(creds: &Credentials, input: &SigningInput<'_>) -> Signed {
    let (canonical_headers, signed_headers) = canonical_headers(input.headers);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        canonical_uri(input.path, input.s3),
        canonical_query(input.query),
        canonical_headers,
        signed_headers,
        input.payload_hash
    );
    let date_stamp = input.time.format("%Y%m%d").to_string();
    let scope = format!("{date_stamp}/{}/{}/aws4_request", input.region, input.service);
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{scope}\n{}",
        amz_date(input.time),
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(format!("AWS4{}", creds.secret_access_key).as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, input.region.as_bytes());
    let k_service = hmac_sha256(&k_region, input.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );
    Signed { canonical_request, string_to_sign, signature, signed_headers, authorization }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn time(s: &str) -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ").expect("time").and_utc()
    }

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn hmac_matches_rfc4231() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Key longer than the block size is hashed first (RFC 4231 case 6).
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    struct Vector {
        name: &'static str,
        method: &'static str,
        path: &'static str,
        query: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
        token: Option<&'static str>,
        creq: &'static str,
        authz: &'static str,
    }

    /// Vectors from the AWS Signature V4 test suite ("aws4_testsuite"): credentials
    /// AKIDEXAMPLE / wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY, us-east-1, service "service",
    /// 20150830T123600Z. The suite passes the request line as written, so paths are given
    /// unencoded and are encoded exactly once here.
    fn suite() -> Vec<Vector> {
        let hx = ("host", "example.amazonaws.com");
        let dt = ("x-amz-date", "20150830T123600Z");
        vec![
            Vector {
                name: "get-vanilla",
                method: "GET",
                path: "/",
                query: "",
                headers: vec![hx, dt],
                body: "",
                token: None,
                creq: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                authz: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            },
            Vector {
                name: "get-header-value-trim",
                method: "GET",
                path: "/",
                query: "",
                headers: vec![hx, ("my-header1", "  value1"), ("my-header2", "\"a   b   c\""), dt],
                body: "",
                token: None,
                creq: "GET\n/\n\nhost:example.amazonaws.com\nmy-header1:value1\nmy-header2:\"a b c\"\nx-amz-date:20150830T123600Z\n\nhost;my-header1;my-header2;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                authz: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;my-header1;my-header2;x-amz-date, Signature=acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736",
            },
            Vector {
                name: "get-vanilla-query-order-key-case",
                method: "GET",
                path: "/",
                query: "Param2=value2&Param1=value1",
                headers: vec![hx, dt],
                body: "",
                token: None,
                creq: "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                authz: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
            },
            Vector {
                name: "get-utf8",
                method: "GET",
                path: "/ሴ",
                query: "",
                headers: vec![hx, dt],
                body: "",
                token: None,
                creq: "GET\n/%E1%88%B4\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                authz: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85",
            },
            Vector {
                name: "post-vanilla",
                method: "POST",
                path: "/",
                query: "",
                headers: vec![hx, dt],
                body: "",
                token: None,
                creq: "POST\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                authz: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
            },
            Vector {
                name: "post-x-www-form-urlencoded",
                method: "POST",
                path: "/",
                query: "",
                headers: vec![("content-type", "application/x-www-form-urlencoded"), hx, dt],
                body: "Param1=value1",
                token: None,
                creq: "POST\n/\n\ncontent-type:application/x-www-form-urlencoded\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\ncontent-type;host;x-amz-date\n9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
                authz: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a",
            },
            Vector {
                name: "get-vanilla-with-session-token",
                method: "GET",
                path: "/",
                query: "",
                headers: vec![hx, dt],
                body: "",
                token: Some("6e86291e8372ff2a2260956d9b8aae1d763fbf194267"),
                creq: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\nx-amz-security-token:6e86291e8372ff2a2260956d9b8aae1d763fbf194267\n\nhost;x-amz-date;x-amz-security-token\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                authz: "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8",
            },
        ]
    }

    #[test]
    fn official_test_suite_vectors() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        for v in suite() {
            let mut headers = h(&v.headers);
            if let Some(t) = v.token {
                headers.push(("x-amz-security-token".into(), t.into()));
            }
            let out = sign(
                &creds,
                &SigningInput {
                    method: v.method,
                    path: v.path,
                    query: v.query,
                    headers: &headers,
                    payload_hash: &sha256_hex(v.body.as_bytes()),
                    region: "us-east-1",
                    service: "service",
                    time: time("20150830T123600Z"),
                    s3: false,
                },
            );
            assert_eq!(out.canonical_request, v.creq, "{}: canonical request", v.name);
            assert_eq!(out.authorization, v.authz, "{}: authorization", v.name);
        }
    }

    fn s3_creds() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    fn s3_sign(method: &str, path: &str, query: &str, headers: &[(&str, &str)], payload_hash: &str) -> Signed {
        sign(
            &s3_creds(),
            &SigningInput {
                method,
                path,
                query,
                headers: &h(headers),
                payload_hash,
                region: "us-east-1",
                service: "s3",
                time: time("20130524T000000Z"),
                s3: true,
            },
        )
    }

    /// Examples from "Signature Calculations for the Authorization Header: Transferring
    /// Payload in a Single Chunk" in the S3 API reference.
    #[test]
    fn s3_documentation_examples() {
        let host = ("host", "examplebucket.s3.amazonaws.com");
        let date = ("x-amz-date", "20130524T000000Z");

        let get = s3_sign(
            "GET",
            "/test.txt",
            "",
            &[host, ("range", "bytes=0-9"), ("x-amz-content-sha256", EMPTY_SHA256), date],
            EMPTY_SHA256,
        );
        assert_eq!(get.signature, "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
        assert_eq!(get.signed_headers, "host;range;x-amz-content-sha256;x-amz-date");

        let body_hash = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(body_hash, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let put = s3_sign(
            "PUT",
            "/test%24file.text",
            "",
            &[
                host,
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-content-sha256", &body_hash),
                date,
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ],
            &body_hash,
        );
        assert_eq!(put.signature, "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");

        let lifecycle =
            s3_sign("GET", "/", "lifecycle", &[host, ("x-amz-content-sha256", EMPTY_SHA256), date], EMPTY_SHA256);
        assert_eq!(lifecycle.signature, "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543");

        let list = s3_sign(
            "GET",
            "/",
            "max-keys=2&prefix=J",
            &[host, ("x-amz-content-sha256", EMPTY_SHA256), date],
            EMPTY_SHA256,
        );
        assert_eq!(list.signature, "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }

    #[test]
    fn s3_and_generic_path_rules_differ() {
        assert_eq!(canonical_uri("/a b/../c//d", true), "/a%20b/../c//d", "S3 does not normalise");
        assert_eq!(canonical_uri("/a%20b/c", true), "/a%20b/c", "S3 encodes once");
        assert_eq!(canonical_uri("/a%20b/../c//d/", false), "/c/d/", "generic normalises");
        assert_eq!(canonical_uri("/a%20b", false), "/a%2520b", "generic double-encodes");
        assert_eq!(canonical_uri("", true), "/");
        assert_eq!(canonical_uri("/a+b~c", true), "/a%2Bb~c");
    }

    #[test]
    fn canonical_query_rules() {
        assert_eq!(canonical_query("b=2&a=1&a=0"), "a=0&a=1&b=2");
        assert_eq!(canonical_query("lifecycle"), "lifecycle=");
        assert_eq!(canonical_query("k=a%20b&j=%7E"), "j=~&k=a%20b");
        assert_eq!(canonical_query("x=a+b"), "x=a%2Bb");
        assert_eq!(canonical_query(""), "");
        assert_eq!(canonical_query("A=1&a=1"), "A=1&a=1", "sorting is by encoded bytes, upper case first");
    }

    #[test]
    fn duplicate_headers_are_merged() {
        let out = sign(
            &s3_creds(),
            &SigningInput {
                method: "GET",
                path: "/",
                query: "",
                headers: &h(&[("host", "h"), ("X-A", " 1 "), ("x-a", "2")]),
                payload_hash: EMPTY_SHA256,
                region: "r",
                service: "s",
                time: time("20130524T000000Z"),
                s3: false,
            },
        );
        assert!(out.canonical_request.contains("\nx-a:1,2\n"), "{}", out.canonical_request);
    }

    #[test]
    fn credentials_debug_hides_secrets() {
        let s = format!("{:?}", Credentials { access_key_id: "AK".into(), secret_access_key: "SECRET".into(), session_token: Some("TOK".into()) });
        assert!(!s.contains("SECRET") && !s.contains("TOK"), "{s}");
    }
}
