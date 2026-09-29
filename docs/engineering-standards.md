# Engineering standards

These apply to every crate in the workspace. They exist so that independently written
crates fit together and stay maintainable.

## Contracts
* `crates/ssx-types` (geometry, `Frame`, `Monitor`, `WindowInfo`) and `crates/ssx-capture`
  (the `CaptureBackend` trait) are the shared contracts. Read them before writing code.
  Do **not** change them from a feature branch; if you need a change, describe it in your
  final report.
* All coordinates are **physical pixels on the virtual desktop** (origin may be negative).
* `Frame` always carries stride, pixel format and colour space. HDR frames are
  `Rgba16F` + `ScRgbLinear` (1.0 = 80 nits) and must have `sdr_white_nits` set.

## Code quality
* Rust 2024, MSRV in the workspace `Cargo.toml`. `cargo fmt` clean (`rustfmt.toml`).
* `cargo clippy -p <crate> --all-targets` produces **zero warnings** (workspace lints are
  clippy pedantic; the few allowed exceptions live in the root `Cargo.toml`). Do not add
  blanket `#![allow]`s; a targeted `#[allow(..)]` needs a one-line reason.
* No `unwrap()`/`expect()`/`panic!`/indexing that can panic on external input in
  non-test code. `expect` is allowed only for invariants proven by construction, with the
  reason in the message.
* `#![forbid(unsafe_code)]` unless the crate must call FFI (Windows APIs). Where `unsafe`
  is unavoidable keep each block minimal and put a `// SAFETY:` comment on it.
* Public items have doc comments. Modules start with a `//!` comment explaining purpose and
  non-obvious decisions (not what the code says, but *why*).
* Errors: `thiserror` enums per crate, with actionable messages. Never stringly-type an
  error that a caller might want to match on.
* Platform code lives behind `cfg`. **Every crate must still compile (as an empty or
  reduced crate) on Linux, Windows and macOS**, so the workspace builds everywhere.
* No blocking the UI thread is possible from a library: long operations take a
  cancellation token / return promptly. No global mutable state.
* Logging via `tracing`; never `println!` in libraries.
* Keep dependencies lean and well maintained. Prefer pure Rust. Check licences are
  GPL-3-compatible (MIT/Apache-2.0/BSD/MPL are fine). Verify a crate's API on docs.rs
  before using it rather than guessing.

## Testing
* Unit tests next to the code, integration tests in `tests/`. Test edge cases and failure
  paths, not just the happy path. Prefer property-style tests for parsers/maths.
* Tests must be deterministic and hermetic (temp dirs, local servers, no internet).
* Tests that need an external program (Xvfb, sway, a D-Bus session) must **skip cleanly**
  with a printed reason when it is missing, and must run in CI where it is installed.
* Do not claim something works unless you ran it. Report what was verified and how, and
  what could *not* be verified (e.g. code that only runs on real Windows).

## Working in a worktree
* Set a private target dir and limit parallelism so agents do not starve each other:
  `export CARGO_TARGET_DIR=<scratchpad>/targets/<your-crate> CARGO_BUILD_JOBS=2`.
* Only touch your own crate directories (adding dependencies to your own `Cargo.toml` is
  fine). Do not edit the root `Cargo.toml`, `Cargo.lock` or other people's crates.
* Commit on your branch with clear messages; **do not push**.
