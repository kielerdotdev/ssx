# ssx-upload

Async uploaders for ssx: ShareX `.sxcu` custom uploaders, Imgur, S3-compatible object
stores, generic HTTP PUT/POST, URL shorteners, a local/no-op uploader, and an OAuth2 helper.
Pure Rust TLS (rustls with the **ring** provider and the platform certificate verifier); no
OpenSSL, no aws-lc, no system libraries.

## Public API

```rust
#[async_trait]
pub trait Uploader: Send + Sync {
    fn name(&self) -> &str;
    fn supports(&self, kind: UploadKind) -> bool;
    async fn upload(&self, req: &UploadRequest, ctx: &UploadContext) -> Result<UploadResult, UploadError>;
}
#[async_trait]
pub trait UrlShortener: Send + Sync {
    fn name(&self) -> &str;
    async fn shorten(&self, url: &str, ctx: &UploadContext) -> Result<String, UploadError>;
}
```

| Type | Purpose |
|---|---|
| `UploadRequest { source: UploadSource, kind: UploadKind, filename }` | `UploadSource::Path(PathBuf)` is streamed from disk, `UploadSource::Bytes { data, filename, mime }` is in memory. `UploadKind` is `Image \| Text \| File \| Video \| Url`. Constructors: `from_path`, `from_bytes`, `text`, `url`. |
| `UploadResult { url, thumbnail_url, deletion_url, raw_response, uploader_name, extra }` | `raw_response` is capped at `RAW_RESPONSE_LIMIT` (64 KiB). `extra` carries uploader specific data (Imgur `id`/`deletehash`, S3 `key`/`etag`, `original_url` after shortening). |
| `UploadContext { progress, cancel, http, secrets }` | `ProgressSink` (bytes sent / total; the crate throttles callbacks to one per 50 ms and always delivers the final one), a `tokio_util` `CancellationToken`, a shared `reqwest::Client` (`build_http_client()`), and a `SecretStore`. |
| `UploadError` | `Network`, `Http { status, message, body_snippet, retry_after }`, `RateLimited`, `Auth`, `Cancelled`, `InvalidResponse`, `Config`, `Unsupported`, `Io`; `is_retryable()` and `retry_after()`. |
| `RetryingUploader::new(Arc<dyn Uploader>, RetryPolicy)` | Exponential backoff with jitter (`None`/`Full`/`Equal`), honours `Retry-After` (seconds or HTTP date, refuses to sleep longer than `max_retry_after`), retries only retryable errors and only when `UploadSource::is_replayable()`, sleeping is cancellable. |
| `SecretStore` / `InMemorySecretStore` | `get`/`set`/`delete` by key. Uploader configs hold key *names*, never secrets. A keyring backed store lives elsewhere. |

Design notes:

* **Streaming**: bodies are produced in 64 KiB chunks with an exact `Content-Length` (no
  chunked encoding, which many hosts and S3 reject). Peak memory is independent of file size
  (tested with a sparse 200 MB file; the RSS high-water mark does not move).
* **Cancellation**: every request races the token; dropping the request aborts the socket, so
  cancelling a stalled multi-GB upload returns immediately.
* `reqwest::Client::new()` **panics** in this workspace (no default crypto provider by
  design). Always use `ssx_upload::build_http_client()` (or `http::client_builder()`).

### Modules

| Module | Feature | What |
|---|---|---|
| `sxcu` | `sxcu` (default) | `.sxcu` model, template language, JSONPath/XPath/regex, `SxcuUploader` (also a `UrlShortener`). |
| `imgur` | always | `ImgurUploader` (anonymous `Client-ID` or bearer token, image and video, deletehash to deletion URL). |
| `s3`, `sigv4` | `s3` (default) | `S3Uploader` and the in-crate AWS Signature V4. |
| `http_uploader` | always | Config-driven PUT/POST (raw or multipart), secret-backed auth, explicit result URL source. |
| `shorten` | always | `HttpShortener` (is.gd, v.gd, TinyURL presets, or any GET/POST endpoint) and `ShorteningUploader` (fails open by default). |
| `local` | always | `LocalUploader`: no-op (`file://` / `local://` URLs) or copy into a folder, for tests and "shared folder" destinations. |
| `oauth` | `oauth` (default) | Loopback + PKCE authorization-code flow, refresh, persistence. |
| `nameparser` | always | ShareX `%y-%mo-%d`, `%rn{8}` codes. |

### S3 notes

Path-style or virtual-hosted (`Auto` picks path-style for custom endpoints and dotted bucket
names over HTTPS), custom endpoint, region, `x-amz-acl`, storage class, `Cache-Control`,
`Content-Disposition`, arbitrary signed headers, key template (`%y/%mo/%rn{6}_{filename}`),
public URL template (`https://cdn.example.com/{key}`). Presets: `S3Config::aws`,
`cloudflare_r2` (region `auto`), `backblaze_b2`, `wasabi`, `minio`. Payload signing is
`UNSIGNED-PAYLOAD` over HTTPS and a one-pass streaming SHA-256 of the body over HTTP
(`PayloadSigning` overrides). SigV4 is checked against AWS's published test-suite vectors and
the S3 documentation examples (see `src/sigv4.rs` tests); the mock-server tests additionally
re-derive the signature of the request the server actually received.
`aws-chunked` streaming signatures are not implemented (not needed: HTTPS uses unsigned
payloads, HTTP hashes first).

### OAuth notes

`OAuthClient::authorize()` binds `127.0.0.1:0`, opens the URL through an injected
`BrowserOpener`, verifies `state` (constant time; forged callbacks are answered with 400 and
ignored), checks the `Host` header (DNS rebinding), exchanges the code with the PKCE verifier,
stores tokens under `oauth.<name>.tokens`. `access_token()` refreshes at most once under
concurrency (single flight) and keeps the old refresh token when the provider does not rotate
it. `OAuthConfig::imgur` disables PKCE (Imgur ignores it).

### SFTP / FTP (not implemented, TODO)

SFTP needs either libssh2 (C) or a large pure-Rust stack (`russh` + `russh-sftp`); FTP(S) is
small but needs TLS session reuse for many servers. Decision pending. The design already fits:
implement `Uploader` (stream the `UploadSource` through `BodyPlan`-style chunking, report to
`ctx.progress`, honour `ctx.cancel`, resolve passwords through `ctx.secrets`). The 64 KiB chunk
helpers in `body.rs` are reusable for that.

## `.sxcu` compatibility

Specified by ShareX's source (`ShareX.UploadersLib/CustomUploader`:
`CustomUploaderItem.cs`, `ShareXSyntaxParser.cs`, `ShareXCustomUploaderSyntaxParser.cs`,
`Functions/*`, `NameParser.cs`), read from the upstream repository. The getsharex.com docs
site was not reachable from the build sandbox; where the prose docs and the source could
disagree, the source was followed.

### File format

| Field | Status | Notes |
|---|---|---|
| `Version` | supported | Files at or below 12.3.1 are rejected like ShareX; at or below 13.7.1 the legacy `$function$` syntax is migrated (including ShareX's quirk of dropping a backslash and the next character). A missing `Version` is accepted with a warning (ShareX refuses). |
| `Name` | supported | Display name falls back to the request host. |
| `DestinationType` | supported | Flags `ImageUploader`, `TextUploader`, `FileUploader`, `URLShortener`, `URLSharingService`, as `"A, B"` string, array of names, or number. Empty means "any compatible kind". Video uses `FileUploader`. |
| `RequestMethod` | supported | `GET POST PUT PATCH DELETE`, default `POST`. |
| `RequestURL` | supported | Templates with `{filename}`/`{input}` percent-encoded; `https://` prefixed when no scheme; a `?query` (outside `{}`) is moved into `Parameters` at load, as ShareX does. |
| `Parameters` | supported | Query parameters, `%` codes then templates, percent-encoded on send. |
| `Headers` | supported | `%` codes then templates. A `Content-Type` header overrides the body's. |
| `Body` | supported | `None`, `MultipartFormData`, `FormURLEncoded`, `JSON`, `XML`, `Binary`. Files need `MultipartFormData` (+`FileFormName`) or `Binary`, otherwise the uploader reports it does not support the kind. |
| `Arguments` | supported | Multipart / URL-encoded fields, in file order; the file part is last. |
| `FileFormName` | supported | Text uploads with a `FileFormName` are sent as a file part too. |
| `Data` | supported | JSON/XML body. Like ShareX only `%` codes and `{input}`/`{filename}` (JSON/XML-escaped) are substituted, other `{...}` stay literal. |
| `URL`, `ThumbnailURL`, `DeletionURL` | supported | Response templates; empty `URL` means the whole (trimmed) response. `{outputbox:...}` URLs may be empty. |
| `ErrorMessage` | supported | Evaluated on non-2xx and attached to the error (`Http.message`, or `Auth.message` for 401/403). |
| unknown keys | preserved | Kept in `CustomUploader::extra` and written back. |
| key / enum case | tolerant | `requesturl`, `"put"`, `"multipartformdata"` all load. Numbers in string maps are stringified, `null` means unset. |
| round trip | supported | `to_json_string()` writes ShareX field order and omits defaults; every fixture round-trips in tests. |
| `validate()` / `check()` | ssx addition | Static errors (missing `RequestURL`, unknown function, too few arguments, response-only function in a request field, files with a body that cannot carry them, missing `FileFormName`, bad scheme, bad header names) and warnings (missing `Version`/`DestinationType`, GET with body, invalid JSON `Data`, ignored fields). |

Success is any 2xx; redirects are followed (`{responseurl}` is the final URL).

### Template functions

| Syntax | Status | Notes |
|---|---|---|
| `{input}` | supported | Text/URL payload; empty for files and in response templates; percent-encoded in `RequestURL` and response templates. |
| `{filename}` | supported | Same encoding rule. |
| `{base64:text}` | supported | UTF-8 to standard base64; empty in, empty out. |
| `{random:a\|b\|c}` | supported | Needs at least 2 arguments. |
| `{select:a\|b}` | supported, non-interactive by default | Picks the first non-empty option unless an `Interaction` is injected. |
| `{inputbox}`, `{inputbox:title}`, `{inputbox:title\|default}`, alias `{prompt:...}` | supported, non-interactive by default | Returns the default text unless an `Interaction` is injected. |
| `{outputbox:text}`, `{outputbox:title\|text}` | supported, non-interactive by default | Logs at info level unless an `Interaction` is injected; yields "". |
| `{response}`, `{responseurl}` | supported | Response templates only (static validation rejects them elsewhere). |
| `{header:Name}` | supported | Case-insensitive; multiple values joined with `, `. |
| `{json:path}`, `{json:input\|path}` | supported | JSONPath per Goessner as accepted by Newtonsoft `SelectToken`: `$`, `.a`, `['a']`, `[n]`, negative index, `[*]`, `..`, slices, unions, filters `[?(@.a == 'x' && @.b > 1)]`. First match wins. Script expressions `[(...)]` and `=~` are reported as unsupported. Strings as-is; numbers/bools by value (`true`, where ShareX prints `True`); null is empty; objects/arrays yield compact JSON (ShareX throws). |
| `{xml:xpath}`, `{xml:input\|xpath}` | supported | XPath 1.0 via `sxd-xpath`; first node in document order, string value; non-node results (`count()`) are stringified. |
| `{regex:pattern}`, `{regex:pattern\|group}`, `{regex:input\|pattern\|group}` | supported | `fancy-regex`: lookbehind/lookahead, backreferences, named groups `(?<n>..)`; group by number or name; a missing group is empty (.NET `Group.Empty`). Backtracking is bounded. Argument-count rules match upstream (2 arguments means `pattern\|group`). |
| `\{` `\}` `\|` `\:` `\\` | supported | Backslash escapes the next character anywhere. |
| nesting | supported | Arguments may contain calls; depth limited to 48. |
| name parser `%y %yy %mo %mon %mon2 %d %h %mi %s %ms %pm %wy %w %w2 %unix %un %uln %cn %guid %GUID %n` | supported | Applied to `Parameters`, `Headers`, `Arguments`, `Data` (never `RequestURL`). English month/day names. |
| `%rn %ra %rna %rx %rX %radjective %ranimal %remoji %rf{file}` and `{n}` counts | supported | Counts capped at 4096. Word lists are small built-ins. |
| `%i %ia %iA %ib %iB %iAa %iaA %ix %iX` (+`{n}`/`{base,pad}`) | supported | Counter is per `SxcuUploader` instance (upstream restarts at 1 on every parse). |
| `%t %pn %width %height` | partial | `%width`/`%height` expand to empty and `%t`/`%pn` stay literal unless supplied through `NameParser` (same as upstream when unset). |

Deliberate deviations from upstream: a stray `}` or `|` outside a call is literal (upstream
silently truncates the text there); month/day names are English; the `json` bool/object
behaviour above; `%` repeat counts are capped; missing `Version` is tolerated.

**Functions that do not exist.** ShareX's custom uploader syntax has no `md5`, `sha1`, `sha256`,
`{outputfolder}` or `{mime}` functions (checked against `Functions/*.cs`), so none are
implemented; using them yields an "unknown function" validation error rather than being
silently ignored. If ShareX adds functions, extend `template::FUNCTIONS`, the single table used
by evaluation and validation.

### Verified

Fixtures in `tests/fixtures/` (18 files modelled on imgur, catbox, 0x0.st, uguu/pomf, zipline,
sxcu.net, lensdump, chevereto, x0.at, a bearer-token JSON API, an XML API, an HTML scraper,
is.gd, transfer.sh, dpaste, pixeldrain, a legacy `$` file and a redirecting host) each run
against a local mock server. `cargo test -p ssx-upload` needs no network.
