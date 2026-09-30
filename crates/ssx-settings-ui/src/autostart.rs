//! "Start ssx when I log in", per operating system, behind one small trait.
//!
//! | OS | Mechanism | Undo |
//! |---|---|---|
//! | Linux | XDG autostart entry `~/.config/autostart/ssx.desktop` | delete it |
//! | Windows | value `ssx` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | delete the value |
//! | macOS | LaunchAgent `~/Library/LaunchAgents/io.ssx.tray.plist` (`RunAtLoad`) | delete it |
//!
//! What is written is produced by pure functions ([`desktop_entry`], [`launch_agent_plist`],
//! [`run_key_value`]) that are tested on every platform; the OS access is a few lines behind
//! [`Autostart`], with [`FakeAutostart`] for tests and [`RunKeyStore`] so the Windows logic is
//! testable on Linux as well. Only entries carrying ssx's marker are ever removed: a file that
//! merely has the same name is reported, not deleted.
//!
//! The command that gets started is [`AutostartCommand`]: the `ssx-tray` program next to this
//! executable with `--background` (the tray app does not exist in every build yet; the UI says
//! so when the file is missing instead of pretending).

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use ssx_core::settings::atomic_write;

/// File name of the XDG autostart entry.
pub const DESKTOP_FILE_NAME: &str = "ssx.desktop";
/// Name of the value under the `Run` key.
pub const RUN_VALUE_NAME: &str = "ssx";
/// Registry key (below `HKEY_CURRENT_USER`) that holds the auto-run values.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Label (and file stem) of the macOS LaunchAgent.
pub const LAUNCH_AGENT_LABEL: &str = "io.ssx.tray";
/// The line that marks a file as written by ssx (desktop entries and plists).
pub const MARKER: &str = "X-SSX-Managed";
/// The background program.
pub const TRAY_PROGRAM: &str = "ssx-tray";
/// Its arguments when started at login.
pub const TRAY_ARGS: [&str; 1] = ["--background"];

/// What to run at login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartCommand {
    /// The program (an absolute path in practice).
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
}

impl AutostartCommand {
    /// `ssx-tray --background` next to the running executable (or bare `ssx-tray`, found on
    /// `PATH`, when the executable's folder is unknown).
    pub fn discover() -> Self {
        let exe = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok());
        let name = format!("{TRAY_PROGRAM}{}", std::env::consts::EXE_SUFFIX);
        let program = exe
            .as_deref()
            .and_then(Path::parent)
            .map_or_else(|| PathBuf::from(&name), |d| d.join(&name));
        Self { program, args: TRAY_ARGS.iter().map(ToString::to_string).collect() }
    }

    /// Whether the program exists (bare names are looked up on `PATH`).
    pub fn program_exists(&self) -> bool {
        if self.program.components().count() > 1 {
            return self.program.is_file();
        }
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&self.program).is_file()))
    }
}

/// Why changing autostart failed.
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    /// Reading or writing a file failed.
    #[error("cannot access {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A file with the entry's name exists but ssx did not create it.
    #[error(
        "{0} exists but was not created by ssx; remove or rename it yourself to let ssx manage autostart"
    )]
    NotOurs(PathBuf),
    /// The registry refused.
    #[error("cannot access the Windows registry: {0}")]
    Registry(String),
    /// The home / config folder is unknown.
    #[error("cannot find your home directory, so autostart cannot be configured")]
    NoHome,
}

/// Turns autostart on and off.
pub trait Autostart: Send + Sync + fmt::Debug {
    /// Whether ssx is set to start at login.
    fn is_enabled(&self) -> Result<bool, AutostartError>;
    /// Enables or disables it. Idempotent.
    fn set_enabled(&self, enabled: bool) -> Result<(), AutostartError>;
    /// Where the setting lives, for the help text (`~/.config/autostart/ssx.desktop`).
    fn location(&self) -> String;
}

// ---- pure generators ---------------------------------------------------------------------

/// Characters that force quoting of an `Exec=` argument (Desktop Entry Specification,
/// "The Exec key").
const EXEC_RESERVED: &[char] = &[
    ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')',
    '`',
];

/// One argument of an `Exec=` line, quoted per the Desktop Entry Specification: reserved
/// characters mean double quotes, and inside them `"`, `` ` ``, `$` and `\` are backslash
/// escaped. A literal `%` is always written `%%` (it starts a field code otherwise).
pub fn exec_quote(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    if arg.is_empty() || arg.contains(EXEC_RESERVED) {
        let mut out = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        arg
    }
}

/// The XDG autostart entry.
pub fn desktop_entry(cmd: &AutostartCommand) -> String {
    let mut exec = exec_quote(&cmd.program.to_string_lossy());
    for a in &cmd.args {
        exec.push(' ');
        exec.push_str(&exec_quote(a));
    }
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=ssx\n\
         Comment=Screenshots, recordings and uploads\n\
         Exec={exec}\n\
         Icon=ssx\n\
         Terminal=false\n\
         Categories=Utility;\n\
         X-GNOME-Autostart-enabled=true\n\
         {MARKER}=true\n"
    )
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The macOS LaunchAgent property list.
pub fn launch_agent_plist(label: &str, cmd: &AutostartCommand) -> String {
    let mut args = format!("    <string>{}</string>\n", xml_escape(&cmd.program.to_string_lossy()));
    for a in &cmd.args {
        args.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         {args}\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>ProcessType</key>\n\
         \x20 <string>Interactive</string>\n\
         \x20 <key>{MARKER}</key>\n\
         \x20 <true/>\n\
         </dict>\n\
         </plist>\n",
        xml_escape(label)
    )
}

/// One argument of a Windows command line (`CommandLineToArgvW` rules): quoted when it has
/// spaces or quotes, backslashes before a quote doubled.
pub fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// The data of the `Run` value: the program (always quoted, paths often contain spaces)
/// followed by the arguments.
pub fn run_key_value(cmd: &AutostartCommand) -> String {
    let mut out = format!("\"{}\"", cmd.program.to_string_lossy().replace('"', ""));
    for a in &cmd.args {
        out.push(' ');
        out.push_str(&windows_quote(a));
    }
    out
}

// ---- file based (Linux, macOS) -----------------------------------------------------------

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> AutostartError + '_ {
    |source| AutostartError::Io { path: path.to_path_buf(), source }
}

fn read_optional(path: &Path) -> Result<Option<String>, AutostartError> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// A single managed file (desktop entry or plist).
#[derive(Debug, Clone)]
struct ManagedFile {
    path: PathBuf,
    content: String,
}

impl ManagedFile {
    fn is_enabled(&self) -> Result<bool, AutostartError> {
        Ok(read_optional(&self.path)?.is_some_and(|t| t.contains(MARKER)))
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), AutostartError> {
        let existing = read_optional(&self.path)?;
        if let Some(t) = &existing
            && !t.contains(MARKER)
        {
            return Err(AutostartError::NotOurs(self.path.clone()));
        }
        if enabled {
            if existing.as_deref() == Some(self.content.as_str()) {
                return Ok(());
            }
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir).map_err(io_err(dir))?;
            }
            atomic_write(&self.path, self.content.as_bytes()).map_err(io_err(&self.path))
        } else if existing.is_some() {
            std::fs::remove_file(&self.path).map_err(io_err(&self.path))
        } else {
            Ok(())
        }
    }
}

/// Linux: `<config home>/autostart/ssx.desktop`.
#[derive(Debug, Clone)]
pub struct XdgAutostart(ManagedFile);

impl XdgAutostart {
    /// Writes below `config_home` (normally `~/.config`).
    pub fn new(config_home: &Path, cmd: &AutostartCommand) -> Self {
        Self(ManagedFile {
            path: config_home.join("autostart").join(DESKTOP_FILE_NAME),
            content: desktop_entry(cmd),
        })
    }

    /// `$XDG_CONFIG_HOME` or `~/.config`.
    pub fn from_env(cmd: &AutostartCommand) -> Result<Self, AutostartError> {
        let abs = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_absolute());
        let home = abs("XDG_CONFIG_HOME")
            .or_else(|| abs("HOME").map(|h| h.join(".config")))
            .ok_or(AutostartError::NoHome)?;
        Ok(Self::new(&home, cmd))
    }
}

impl Autostart for XdgAutostart {
    fn is_enabled(&self) -> Result<bool, AutostartError> {
        self.0.is_enabled()
    }
    fn set_enabled(&self, enabled: bool) -> Result<(), AutostartError> {
        self.0.set_enabled(enabled)
    }
    fn location(&self) -> String {
        self.0.path.display().to_string()
    }
}

/// macOS: `~/Library/LaunchAgents/io.ssx.tray.plist`, started at the next login.
#[derive(Debug, Clone)]
pub struct MacLaunchAgent(ManagedFile);

impl MacLaunchAgent {
    /// Writes below `home`.
    pub fn new(home: &Path, cmd: &AutostartCommand) -> Self {
        Self(ManagedFile {
            path: home.join("Library/LaunchAgents").join(format!("{LAUNCH_AGENT_LABEL}.plist")),
            content: launch_agent_plist(LAUNCH_AGENT_LABEL, cmd),
        })
    }

    /// Uses `$HOME`.
    pub fn from_env(cmd: &AutostartCommand) -> Result<Self, AutostartError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or(AutostartError::NoHome)?;
        Ok(Self::new(&home, cmd))
    }
}

impl Autostart for MacLaunchAgent {
    fn is_enabled(&self) -> Result<bool, AutostartError> {
        self.0.is_enabled()
    }
    fn set_enabled(&self, enabled: bool) -> Result<(), AutostartError> {
        self.0.set_enabled(enabled)
    }
    fn location(&self) -> String {
        self.0.path.display().to_string()
    }
}

// ---- registry based (Windows) ------------------------------------------------------------

/// The one registry value autostart needs. Real on Windows, in memory elsewhere.
pub trait RunKeyStore: Send + Sync + fmt::Debug {
    /// The current data of the value, if it exists.
    fn get(&self) -> Result<Option<String>, AutostartError>;
    /// Creates or replaces the value.
    fn set(&self, data: &str) -> Result<(), AutostartError>;
    /// Removes the value if present.
    fn remove(&self) -> Result<(), AutostartError>;
}

/// A [`RunKeyStore`] in memory (tests, and non-Windows builds).
#[derive(Debug, Default)]
pub struct MemoryRunKey(Mutex<Option<String>>);

impl MemoryRunKey {
    /// The stored data.
    pub fn value(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
    /// Pre-seeds the value (a foreign entry called `ssx`).
    pub fn seed(&self, data: &str) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(data.to_owned());
    }
}

impl RunKeyStore for MemoryRunKey {
    fn get(&self) -> Result<Option<String>, AutostartError> {
        Ok(self.value())
    }
    fn set(&self, data: &str) -> Result<(), AutostartError> {
        self.seed(data);
        Ok(())
    }
    fn remove(&self) -> Result<(), AutostartError> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }
}

/// Windows autostart on top of a [`RunKeyStore`]. The value is "ours" when it starts with
/// the quoted program path we would write, so a foreign `ssx` value is not clobbered
/// silently (it is reported as not ours).
#[derive(Debug)]
pub struct RegistryAutostart<S: RunKeyStore> {
    store: S,
    value: String,
    program_prefix: String,
}

impl<S: RunKeyStore> RegistryAutostart<S> {
    /// Manages the value for `cmd` in `store`.
    pub fn new(store: S, cmd: &AutostartCommand) -> Self {
        let value = run_key_value(cmd);
        let program_prefix = format!("\"{}\"", cmd.program.to_string_lossy().replace('"', ""));
        Self { store, value, program_prefix }
    }

    /// The backing store.
    pub fn store(&self) -> &S {
        &self.store
    }

    fn ours(&self, data: &str) -> bool {
        // An older ssx may have been installed elsewhere: any value that launches a program
        // called `ssx-tray[.exe]` counts as ours.
        data.starts_with(&self.program_prefix)
            || data.to_ascii_lowercase().contains(&format!("{TRAY_PROGRAM}."))
            || data.to_ascii_lowercase().contains(&format!("{TRAY_PROGRAM}\""))
    }
}

impl<S: RunKeyStore> Autostart for RegistryAutostart<S> {
    fn is_enabled(&self) -> Result<bool, AutostartError> {
        Ok(self.store.get()?.is_some_and(|d| self.ours(&d)))
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), AutostartError> {
        let existing = self.store.get()?;
        if let Some(d) = &existing
            && !self.ours(d)
        {
            return Err(AutostartError::NotOurs(PathBuf::from(format!(
                "HKCU\\{RUN_KEY}\\{RUN_VALUE_NAME}"
            ))));
        }
        if enabled {
            if existing.as_deref() != Some(self.value.as_str()) {
                self.store.set(&self.value)?;
            }
            Ok(())
        } else if existing.is_some() {
            self.store.remove()
        } else {
            Ok(())
        }
    }

    fn location(&self) -> String {
        format!("HKCU\\{RUN_KEY}\\{RUN_VALUE_NAME}")
    }
}

#[cfg(windows)]
mod windows_impl {
    use windows_registry::CURRENT_USER;

    use super::{AutostartError, RUN_KEY, RUN_VALUE_NAME, RunKeyStore};

    /// The real `HKCU\...\Run` value. Compile-checked on Linux only.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct WindowsRunKey;

    fn err(e: &windows_registry::Error) -> AutostartError {
        AutostartError::Registry(e.message())
    }

    fn not_found(e: &windows_registry::Error) -> bool {
        // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND)
        let code = e.code().0 as u32;
        code == 0x8007_0002 || code == 0x8007_0003
    }

    impl RunKeyStore for WindowsRunKey {
        fn get(&self) -> Result<Option<String>, AutostartError> {
            let key = match CURRENT_USER.open(RUN_KEY) {
                Ok(k) => k,
                Err(e) if not_found(&e) => return Ok(None),
                Err(e) => return Err(err(&e)),
            };
            match key.get_string(RUN_VALUE_NAME) {
                Ok(v) => Ok(Some(v)),
                Err(e) if not_found(&e) => Ok(None),
                Err(e) => Err(err(&e)),
            }
        }

        fn set(&self, data: &str) -> Result<(), AutostartError> {
            let key = CURRENT_USER.create(RUN_KEY).map_err(|e| err(&e))?;
            key.set_string(RUN_VALUE_NAME, data).map_err(|e| err(&e))
        }

        fn remove(&self) -> Result<(), AutostartError> {
            let key = match CURRENT_USER.options().read().write().open(RUN_KEY) {
                Ok(k) => k,
                Err(e) if not_found(&e) => return Ok(()),
                Err(e) => return Err(err(&e)),
            };
            match key.remove_value(RUN_VALUE_NAME) {
                Ok(()) => Ok(()),
                Err(e) if not_found(&e) => Ok(()),
                Err(e) => Err(err(&e)),
            }
        }
    }
}

#[cfg(windows)]
pub use windows_impl::WindowsRunKey;

// ---- selection and a fake ----------------------------------------------------------------

/// The autostart implementation for the operating system this binary was built for.
pub fn system(cmd: &AutostartCommand) -> Arc<dyn Autostart> {
    #[cfg(windows)]
    {
        Arc::new(RegistryAutostart::new(WindowsRunKey, cmd))
    }
    #[cfg(target_os = "macos")]
    {
        match MacLaunchAgent::from_env(cmd) {
            Ok(a) => Arc::new(a),
            Err(_) => Arc::new(Unavailable(AutostartError::NoHome.to_string())),
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        match XdgAutostart::from_env(cmd) {
            Ok(a) => Arc::new(a),
            Err(_) => Arc::new(Unavailable(AutostartError::NoHome.to_string())),
        }
    }
}

/// Autostart cannot be configured here (no home directory); every change fails with the reason.
#[derive(Debug)]
#[cfg_attr(windows, allow(dead_code))] // only the Unix `system()` builds it
struct Unavailable(String);

impl Autostart for Unavailable {
    fn is_enabled(&self) -> Result<bool, AutostartError> {
        Ok(false)
    }
    fn set_enabled(&self, _: bool) -> Result<(), AutostartError> {
        Err(AutostartError::Registry(self.0.clone()))
    }
    fn location(&self) -> String {
        "unavailable".to_owned()
    }
}

/// An in-memory [`Autostart`] for tests and screenshots.
#[derive(Debug, Default)]
pub struct FakeAutostart {
    enabled: Mutex<bool>,
    fail: Mutex<Option<String>>,
    calls: Mutex<Vec<bool>>,
}

impl FakeAutostart {
    /// Starts with autostart on or off.
    pub fn new(enabled: bool) -> Self {
        Self { enabled: Mutex::new(enabled), ..Self::default() }
    }

    /// Makes every `set_enabled` fail with `reason`.
    pub fn fail_with(&self, reason: &str) {
        *self.fail.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason.to_owned());
    }

    /// The values `set_enabled` was called with.
    pub fn calls(&self) -> Vec<bool> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Autostart for FakeAutostart {
    fn is_enabled(&self) -> Result<bool, AutostartError> {
        Ok(*self.enabled.lock().unwrap_or_else(PoisonError::into_inner))
    }
    fn set_enabled(&self, enabled: bool) -> Result<(), AutostartError> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner).push(enabled);
        if let Some(r) = self.fail.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            return Err(AutostartError::Registry(r));
        }
        *self.enabled.lock().unwrap_or_else(PoisonError::into_inner) = enabled;
        Ok(())
    }
    fn location(&self) -> String {
        "~/.config/autostart/ssx.desktop".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> AutostartCommand {
        AutostartCommand {
            program: PathBuf::from("/opt/ssx/ssx-tray"),
            args: vec!["--background".into()],
        }
    }

    #[test]
    fn desktop_entry_is_a_valid_autostart_entry() {
        let t = desktop_entry(&cmd());
        assert!(t.starts_with("[Desktop Entry]\n"));
        for line in [
            "Type=Application",
            "Exec=/opt/ssx/ssx-tray --background",
            "Terminal=false",
            "X-GNOME-Autostart-enabled=true",
            "X-SSX-Managed=true",
        ] {
            assert!(t.lines().any(|l| l == line), "missing {line:?} in\n{t}");
        }
        assert!(t.ends_with('\n'));
        assert!(!t.contains("\n\n"), "no blank lines inside the group");
    }

    #[test]
    fn exec_quoting_follows_the_spec() {
        assert_eq!(exec_quote("/usr/bin/ssx"), "/usr/bin/ssx");
        assert_eq!(exec_quote("/opt/My Apps/ssx"), "\"/opt/My Apps/ssx\"");
        assert_eq!(exec_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(exec_quote("$HOME"), "\"\\$HOME\"");
        assert_eq!(exec_quote("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(exec_quote("100%"), "100%%");
        assert_eq!(exec_quote(""), "\"\"");
        assert_eq!(exec_quote("it's"), "\"it's\"");
    }

    #[test]
    fn exec_line_with_spaces_and_metacharacters() {
        let c = AutostartCommand {
            program: PathBuf::from("/opt/My Apps/ssx-tray"),
            args: vec!["--name=a b".into(), "$(rm -rf ~)".into()],
        };
        let t = desktop_entry(&c);
        assert!(
            t.contains("Exec=\"/opt/My Apps/ssx-tray\" \"--name=a b\" \"\\$(rm -rf ~)\"\n"),
            "{t}"
        );
    }

    #[test]
    fn plist_is_well_formed_and_escaped() {
        let c = AutostartCommand {
            program: PathBuf::from("/Applications/ssx & co/<tray>"),
            args: vec!["--background".into()],
        };
        let t = launch_agent_plist(LAUNCH_AGENT_LABEL, &c);
        assert!(t.contains("<string>io.ssx.tray</string>"));
        assert!(t.contains("<string>/Applications/ssx &amp; co/&lt;tray&gt;</string>"));
        assert!(t.contains("<key>RunAtLoad</key>"));
        assert!(t.contains(MARKER));
        // balanced dict/array/plist tags
        for tag in ["plist", "dict", "array"] {
            assert_eq!(
                t.matches(&format!("<{tag}")).count(),
                t.matches(&format!("</{tag}>")).count(),
                "{tag}"
            );
        }
        assert!(!t.contains("& co"), "raw ampersand would break the XML");
    }

    #[test]
    fn windows_quoting() {
        assert_eq!(windows_quote("--background"), "--background");
        assert_eq!(windows_quote("a b"), "\"a b\"");
        assert_eq!(windows_quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(windows_quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(windows_quote(""), "\"\"");
    }

    #[test]
    fn run_key_value_quotes_the_program_always() {
        let c = AutostartCommand {
            program: PathBuf::from(r"C:\Program Files\ssx\ssx-tray.exe"),
            args: vec!["--background".into()],
        };
        assert_eq!(run_key_value(&c), r#""C:\Program Files\ssx\ssx-tray.exe" --background"#);
    }

    #[test]
    fn xdg_autostart_enable_disable_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let a = XdgAutostart::new(dir.path(), &cmd());
        assert!(!a.is_enabled().unwrap());
        a.set_enabled(true).unwrap();
        let file = dir.path().join("autostart/ssx.desktop");
        assert!(file.is_file());
        assert!(a.is_enabled().unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), desktop_entry(&cmd()));
        a.set_enabled(true).unwrap(); // idempotent
        a.set_enabled(false).unwrap();
        assert!(!file.exists());
        assert!(!a.is_enabled().unwrap());
        a.set_enabled(false).unwrap(); // idempotent
        assert!(a.location().ends_with("autostart/ssx.desktop"));
    }

    #[test]
    fn xdg_refuses_to_touch_a_foreign_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("autostart/ssx.desktop");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "[Desktop Entry]\nName=mine\n").unwrap();
        let a = XdgAutostart::new(dir.path(), &cmd());
        assert!(!a.is_enabled().unwrap());
        assert!(matches!(a.set_enabled(true), Err(AutostartError::NotOurs(_))));
        assert!(matches!(a.set_enabled(false), Err(AutostartError::NotOurs(_))));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "[Desktop Entry]\nName=mine\n");
    }

    #[test]
    fn xdg_updates_a_stale_entry() {
        let dir = tempfile::tempdir().unwrap();
        let old = AutostartCommand { program: PathBuf::from("/old/ssx-tray"), args: vec![] };
        XdgAutostart::new(dir.path(), &old).set_enabled(true).unwrap();
        XdgAutostart::new(dir.path(), &cmd()).set_enabled(true).unwrap();
        let t = std::fs::read_to_string(dir.path().join("autostart/ssx.desktop")).unwrap();
        assert!(t.contains("/opt/ssx/ssx-tray --background") && !t.contains("/old/"));
    }

    #[test]
    fn mac_launch_agent_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let a = MacLaunchAgent::new(dir.path(), &cmd());
        a.set_enabled(true).unwrap();
        let f = dir.path().join("Library/LaunchAgents/io.ssx.tray.plist");
        assert!(f.is_file() && a.is_enabled().unwrap());
        a.set_enabled(false).unwrap();
        assert!(!f.exists());
    }

    #[test]
    fn registry_autostart_on_the_memory_store() {
        let c = AutostartCommand {
            program: PathBuf::from(r"C:\ssx\ssx-tray.exe"),
            args: vec!["--background".into()],
        };
        let a = RegistryAutostart::new(MemoryRunKey::default(), &c);
        assert!(!a.is_enabled().unwrap());
        a.set_enabled(true).unwrap();
        assert_eq!(a.store().value().as_deref(), Some(r#""C:\ssx\ssx-tray.exe" --background"#));
        assert!(a.is_enabled().unwrap());
        a.set_enabled(false).unwrap();
        assert_eq!(a.store().value(), None);
        assert!(a.location().starts_with("HKCU\\Software\\Microsoft"));
    }

    #[test]
    fn registry_autostart_does_not_clobber_a_foreign_value() {
        let a = RegistryAutostart::new(MemoryRunKey::default(), &cmd());
        a.store().seed(r#""C:\Other\thing.exe""#);
        assert!(!a.is_enabled().unwrap());
        assert!(matches!(a.set_enabled(true), Err(AutostartError::NotOurs(_))));
        assert_eq!(a.store().value().as_deref(), Some(r#""C:\Other\thing.exe""#));
    }

    #[test]
    fn registry_autostart_recognises_an_older_install_path() {
        let a = RegistryAutostart::new(MemoryRunKey::default(), &cmd());
        a.store().seed(r#""D:\old\ssx-tray.exe" --background"#);
        assert!(a.is_enabled().unwrap());
        a.set_enabled(true).unwrap();
        assert_eq!(a.store().value().as_deref(), Some(run_key_value(&cmd()).as_str()));
    }

    #[test]
    fn fake_records_calls_and_can_fail() {
        let f = FakeAutostart::new(false);
        f.set_enabled(true).unwrap();
        assert!(f.is_enabled().unwrap());
        f.fail_with("denied");
        let e = f.set_enabled(false).unwrap_err().to_string();
        assert!(e.contains("denied"));
        assert!(f.is_enabled().unwrap(), "state unchanged on failure");
        assert_eq!(f.calls(), [true, false]);
    }

    #[test]
    fn discover_points_next_to_this_executable() {
        let c = AutostartCommand::discover();
        assert!(c.program.file_name().unwrap().to_string_lossy().starts_with("ssx-tray"));
        assert_eq!(c.args, ["--background"]);
    }
}
