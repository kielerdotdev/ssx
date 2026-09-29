//! Injectable inputs of the pattern engine.
//!
//! Everything non-deterministic (wall clock, randomness, machine/user names, the
//! auto-increment counter file) sits behind a small trait so that rendering is a pure
//! function of its context. Tests use [`FixedClock`], [`SeededRng`], [`StaticEnv`] and
//! [`MemoryCounter`]; the app uses the `System*` and [`FileCounter`] implementations.

use std::{
    collections::BTreeMap,
    fmt::Debug,
    fs::OpenOptions,
    hash::{BuildHasher, Hasher},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use chrono::{DateTime, FixedOffset, Local};
use fs4::fs_std::FileExt;

/// Source of "now". Returns a timestamp that already carries the UTC offset to display,
/// which is how tests inject arbitrary time zones and DST transitions.
pub trait Clock: Send + Sync + Debug {
    /// The current instant in the offset that `%h`, `%d`, … should be rendered in.
    fn now(&self) -> DateTime<FixedOffset>;
}

/// The wall clock in the machine's local time zone.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<FixedOffset> {
        Local::now().fixed_offset()
    }
}

/// A clock frozen at one instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub DateTime<FixedOffset>);

impl FixedClock {
    /// Parses an RFC 3339 timestamp such as `2024-03-09T14:05:06.789+01:00`.
    pub fn from_rfc3339(s: &str) -> Result<Self, chrono::ParseError> {
        DateTime::parse_from_rfc3339(s).map(Self)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0
    }
}

/// Source of randomness for `%ra`, `%rn`, `%guid`, ….
///
/// Not cryptographic: file names are not secrets. (Anything that must be unguessable, such
/// as an upload token, must not come from here.)
pub trait Rng: Send + Sync + Debug {
    /// The next 64 random bits.
    fn next_u64(&self) -> u64;

    /// A uniformly distributed value in `0..n` (`0` when `n == 0`).
    fn below(&self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Lemire's multiply-shift; bias is < 2^-64 * n which is irrelevant here.
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }
}

const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// `SplitMix64` over an atomic, so `&self` access is thread safe and lock free.
fn splitmix64(state: &AtomicU64) -> u64 {
    let s = state.fetch_add(GOLDEN_GAMMA, Ordering::Relaxed).wrapping_add(GOLDEN_GAMMA);
    let mut z = s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic generator for tests: the same seed yields the same sequence.
#[derive(Debug)]
pub struct SeededRng(AtomicU64);

impl SeededRng {
    /// Creates a generator from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(AtomicU64::new(seed))
    }
}

impl Rng for SeededRng {
    fn next_u64(&self) -> u64 {
        splitmix64(&self.0)
    }
}

/// A generator seeded from the process's hash randomness and the clock.
#[derive(Debug)]
pub struct SystemRng(AtomicU64);

impl SystemRng {
    /// Creates a generator with a fresh random seed.
    pub fn new() -> Self {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        h.write_u64(nanos);
        Self(AtomicU64::new(h.finish()))
    }
}

impl Default for SystemRng {
    fn default() -> Self {
        Self::new()
    }
}

impl Rng for SystemRng {
    fn next_u64(&self) -> u64 {
        splitmix64(&self.0)
    }
}

/// Machine/user identity and the few files a pattern may read (`%rf{path}`).
pub trait Env: Send + Sync + Debug {
    /// Login name (`%un`).
    fn user_name(&self) -> String;
    /// Domain / workgroup (`%uln`); falls back to the machine name where there is none.
    fn user_domain(&self) -> String;
    /// Host name (`%cn`).
    fn machine_name(&self) -> String;
    /// Reads a text file as lines (`%rf{path}`).
    fn read_lines(&self, path: &Path) -> io::Result<Vec<String>>;
}

/// The real environment, read from environment variables and the file system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnv;

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
}

impl Env for SystemEnv {
    fn user_name(&self) -> String {
        first_env(&["USERNAME", "USER", "LOGNAME"]).unwrap_or_else(|| "user".to_owned())
    }

    fn user_domain(&self) -> String {
        first_env(&["USERDOMAIN"]).unwrap_or_else(|| self.machine_name())
    }

    fn machine_name(&self) -> String {
        if let Some(n) = first_env(&["COMPUTERNAME", "HOSTNAME"]) {
            return n;
        }
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "localhost".to_owned())
    }

    fn read_lines(&self, path: &Path) -> io::Result<Vec<String>> {
        Ok(std::fs::read_to_string(path)?.lines().map(str::to_owned).collect())
    }
}

/// A fully scripted environment for tests.
#[derive(Debug, Default, Clone)]
pub struct StaticEnv {
    /// Value of `%un`.
    pub user: String,
    /// Value of `%uln`.
    pub domain: String,
    /// Value of `%cn`.
    pub machine: String,
    /// Virtual files served to `%rf{path}`.
    pub files: BTreeMap<PathBuf, Vec<String>>,
}

impl Env for StaticEnv {
    fn user_name(&self) -> String {
        self.user.clone()
    }
    fn user_domain(&self) -> String {
        self.domain.clone()
    }
    fn machine_name(&self) -> String {
        self.machine.clone()
    }
    fn read_lines(&self, path: &Path) -> io::Result<Vec<String>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such virtual file"))
    }
}

/// Source of the `%i` auto-increment number.
pub trait CounterStore: Send + Sync + Debug {
    /// Atomically increments the counter and returns the **new** value (first call: 1).
    fn next(&self) -> io::Result<u64>;
}

/// A process-local counter.
#[derive(Debug, Default)]
pub struct MemoryCounter(AtomicU64);

impl MemoryCounter {
    /// Creates a counter whose next value will be `last + 1`.
    pub fn starting_after(last: u64) -> Self {
        Self(AtomicU64::new(last))
    }
}

impl CounterStore for MemoryCounter {
    fn next(&self) -> io::Result<u64> {
        Ok(self.0.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

/// A counter persisted in a small text file and guarded by an OS advisory lock, so
/// several ssx processes (tray app + CLI invocations from the shell menu) never hand out
/// the same number.
#[derive(Debug, Clone)]
pub struct FileCounter {
    path: PathBuf,
}

impl FileCounter {
    /// A counter stored at `path` (created on first use).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The backing file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the current value without incrementing (0 if the file is missing).
    pub fn peek(&self) -> io::Result<u64> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(s.trim().parse().unwrap_or(0)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }
}

impl CounterStore for FileCounter {
    fn next(&self) -> io::Result<u64> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;
        file.lock_exclusive()?;
        let result = (|| {
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            let current = match text.trim() {
                "" => 0,
                t => t.parse::<u64>().unwrap_or_else(|_| {
                    // A hand-edited or torn file must not brick screenshot naming: start over.
                    // Unique-file creation still protects against overwriting anything.
                    tracing::warn!(path = %self.path.display(), "corrupt counter file; resetting to 0");
                    0
                }),
            };
            let next = current.saturating_add(1);
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(next.to_string().as_bytes())?;
            file.sync_data()?;
            Ok(next)
        })();
        // Unlock explicitly so an error is not lost, but never mask the primary result.
        let _ = FileExt::unlock(&file);
        result
    }
}

/// Per-name data that comes from the capture rather than the environment.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NameInputs {
    /// Title of the captured / foreground window (`%t`).
    pub window_title: Option<String>,
    /// Process name of that window (`%pn`).
    pub process_name: Option<String>,
    /// Image width in pixels (`%width`).
    pub width: Option<u32>,
    /// Image height in pixels (`%height`).
    pub height: Option<u32>,
}

/// What to do with `%token`s the engine does not know.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum UnknownTokens {
    /// Leave them in the output verbatim (ShareX behaviour, and the default).
    #[default]
    Keep,
    /// Drop them.
    Remove,
    /// Fail with [`PatternError::UnknownToken`](super::PatternError::UnknownToken).
    Error,
}

/// How the rendered text will be used; selects the sanitising rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// A single file name: every path separator is stripped.
    FileName,
    /// A relative folder path: `/` and `\` separate components, each sanitised; `.`/`..`
    /// components are dropped so a pattern can never escape its base folder.
    FilePath,
    /// Free text: no sanitising, `%n` is a newline.
    Text,
}

/// Rendering knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    /// Unknown-token behaviour.
    pub unknown: UnknownTokens,
    /// Truncate the rendered *file name stem* to this many grapheme clusters
    /// (ShareX `MaxNameLength`). `None` = unlimited (the OS limit still applies).
    pub max_name_len: Option<usize>,
    /// Truncate `%t` to this many grapheme clusters (ShareX `MaxTitleLength`).
    pub max_title_len: Option<usize>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { unknown: UnknownTokens::Keep, max_name_len: None, max_title_len: Some(50) }
    }
}

/// Everything [`Pattern::render`](super::Pattern::render) needs.
#[derive(Debug, Clone, Copy)]
pub struct PatternContext<'a> {
    /// Time source.
    pub clock: &'a dyn Clock,
    /// Randomness source.
    pub rng: &'a dyn Rng,
    /// Machine / user identity.
    pub env: &'a dyn Env,
    /// `%i` counter.
    pub counter: &'a dyn CounterStore,
    /// Capture-derived inputs.
    pub inputs: &'a NameInputs,
    /// Rendering options.
    pub options: RenderOptions,
}
