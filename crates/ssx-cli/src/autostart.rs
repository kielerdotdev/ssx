//! Starting `ssx-app` at login: `ssx daemon autostart enable|disable|status`.
//!
//! One mechanism per platform, each the standard, user-level, reversible one:
//!
//! | Platform | What is written |
//! |---|---|
//! | Linux (and other Unix) | XDG autostart entry `$XDG_CONFIG_HOME/autostart/ssx.desktop` |
//! | Windows | the value `ssx` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
//! | macOS | a LaunchAgent `~/Library/LaunchAgents/io.ssx.app.plist` (loaded at the next login) |
//!
//! The text generators are pure and tested for every platform on every OS; the real
//! registry is behind [`RunKey`] (a fake is used in tests; the Windows implementation is
//! compile-checked only). Files ssx writes carry a marker, and `enable`/`disable` never touch a
//! file of the same name that does not: a hand-made entry is the user's.
//!
//! (`ssx-settings-ui` may grow its own "start at login" switch; both write exactly the same
//! entry, so they agree.)

use std::{
    fmt,
    path::{Path, PathBuf},
};

/// The marker line ssx puts in files it writes.
pub const MARKER: &str =
    "Managed by ssx (`ssx daemon autostart`); remove with `ssx daemon autostart disable`.";

/// The registry value name and the LaunchAgent label.
pub const NAME: &str = "ssx";

/// The registry key of per-user autostart values (relative to `HKCU`).
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// LaunchAgent label / file stem.
pub const LAUNCH_LABEL: &str = "io.ssx.app";

/// Which mechanism applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// XDG autostart.
    Linux,
    /// `HKCU\...\Run`.
    Windows,
    /// LaunchAgent.
    MacOs,
}

impl Platform {
    /// The platform this binary runs on.
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

/// Quotes one argument for a Desktop Entry `Exec` line (Desktop Entry spec: double quotes,
/// with `"`, `` ` ``, `$` and `\` backslash-escaped, and `%` doubled).
pub fn desktop_exec_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The XDG autostart entry.
pub fn desktop_entry(app: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         # {MARKER}\n\
         Type=Application\n\
         Name=ssx\n\
         Comment=Screenshots, screen recording and uploads\n\
         Exec={}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        desktop_exec_quote(&app.to_string_lossy())
    )
}

/// XML text escaping for the plist.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The LaunchAgent plist.
pub fn launch_agent(app: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- {MARKER} -->\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LAUNCH_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{}</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>ProcessType</key>\n\
         \t<string>Interactive</string>\n\
         </dict>\n\
         </plist>\n",
        xml_escape(&app.to_string_lossy())
    )
}

/// The value written under [`RUN_KEY`]: the quoted path (the shell splits on spaces
/// otherwise). Paths containing a double quote cannot be quoted and are refused.
pub fn run_value(app: &Path) -> Result<String, AutostartError> {
    let p = app.to_string_lossy();
    if p.contains('"') {
        return Err(AutostartError::Unquotable(app.to_path_buf()));
    }
    Ok(format!("\"{p}\""))
}

/// Where the Linux entry goes.
pub fn linux_entry_path(xdg_config_home: Option<&Path>, home: &Path) -> PathBuf {
    xdg_config_home
        .filter(|p| p.is_absolute())
        .map_or_else(|| home.join(".config"), Path::to_path_buf)
        .join("autostart")
        .join("ssx.desktop")
}

/// Where the macOS agent goes.
pub fn mac_agent_path(home: &Path) -> PathBuf {
    home.join("Library").join("LaunchAgents").join(format!("{LAUNCH_LABEL}.plist"))
}

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    /// The program path cannot be expressed in the platform's format.
    #[error("the path {0} contains a double quote and cannot be used for autostart")]
    Unquotable(PathBuf),
    /// A file with our name exists but was not written by ssx.
    #[error("{0} exists but was not written by ssx; not touching it")]
    Foreign(PathBuf),
    /// No home directory.
    #[error("cannot find your home directory (HOME is not set)")]
    NoHome,
    /// File-system or registry trouble.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> AutostartError {
    let what = what.into();
    move |source| AutostartError::Io { what, source }
}

/// The autostart registry value (`HKCU\...\Run`), abstracted for tests.
pub trait RunKey: fmt::Debug + Send + Sync {
    /// The current value.
    fn get(&self) -> std::io::Result<Option<String>>;
    /// Sets the value.
    fn set(&self, value: &str) -> std::io::Result<()>;
    /// Removes the value; `false` if it did not exist.
    fn remove(&self) -> std::io::Result<bool>;
}

/// An in-memory [`RunKey`].
#[derive(Debug, Default)]
pub struct MemoryRunKey(std::sync::Mutex<Option<String>>);

impl RunKey for MemoryRunKey {
    fn get(&self) -> std::io::Result<Option<String>> {
        Ok(self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone())
    }
    fn set(&self, value: &str) -> std::io::Result<()> {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value.to_owned());
        Ok(())
    }
    fn remove(&self) -> std::io::Result<bool> {
        Ok(self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().is_some())
    }
}

/// The real registry value. Compile-checked only.
#[cfg(windows)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsRunKey;

#[cfg(windows)]
mod windows_key {
    use std::io;

    use super::{NAME, RUN_KEY, RunKey, WindowsRunKey};

    fn conv<T>(r: windows_registry::Result<T>) -> io::Result<T> {
        r.map_err(|e| {
            let code = e.code().0 as u32;
            // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND
            if code == 0x8007_0002 || code == 0x8007_0003 {
                io::Error::new(io::ErrorKind::NotFound, e.message())
            } else {
                io::Error::other(e.message())
            }
        })
    }

    impl RunKey for WindowsRunKey {
        fn get(&self) -> io::Result<Option<String>> {
            let key = match conv(windows_registry::CURRENT_USER.open(RUN_KEY)) {
                Ok(k) => k,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            match conv(key.get_string(NAME)) {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn set(&self, value: &str) -> io::Result<()> {
            let key = conv(windows_registry::CURRENT_USER.create(RUN_KEY))?;
            conv(key.set_string(NAME, value))
        }

        fn remove(&self) -> io::Result<bool> {
            let key = match conv(windows_registry::CURRENT_USER.create(RUN_KEY)) {
                Ok(k) => k,
                Err(e) => return Err(e),
            };
            let existed = self.get()?.is_some();
            if existed {
                conv(key.remove_value(NAME))?;
            }
            Ok(existed)
        }
    }
}

/// The context autostart works in.
#[derive(Debug)]
pub struct Context {
    /// Which mechanism.
    pub platform: Platform,
    /// The home directory.
    pub home: PathBuf,
    /// `XDG_CONFIG_HOME`.
    pub xdg_config_home: Option<PathBuf>,
    /// The registry value (Windows).
    pub run_key: Box<dyn RunKey>,
}

impl Context {
    /// The real environment.
    pub fn system() -> Result<Self, AutostartError> {
        let platform = Platform::current();
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        #[cfg(windows)]
        let run_key: Box<dyn RunKey> = Box::new(WindowsRunKey);
        #[cfg(not(windows))]
        let run_key: Box<dyn RunKey> = Box::new(MemoryRunKey::default());
        Ok(Self {
            platform,
            home: match (home, platform) {
                (Some(h), _) => h,
                (None, Platform::Windows) => PathBuf::new(),
                (None, _) => return Err(AutostartError::NoHome),
            },
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            run_key,
        })
    }

    fn file(&self) -> Option<PathBuf> {
        match self.platform {
            Platform::Linux => Some(linux_entry_path(self.xdg_config_home.as_deref(), &self.home)),
            Platform::MacOs => Some(mac_agent_path(&self.home)),
            Platform::Windows => None,
        }
    }
}

/// What `status` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Set up; runs this command at login.
    Enabled {
        /// The command (path or quoted path).
        command: String,
        /// Where it is stored (file or registry key).
        location: String,
    },
    /// Not set up.
    Disabled,
    /// A file of our name exists that ssx did not write.
    Foreign {
        /// The file.
        path: PathBuf,
    },
}

fn read_marked(path: &Path) -> std::io::Result<Option<(String, bool)>> {
    match std::fs::read_to_string(path) {
        Ok(t) => {
            let ours = t.contains(MARKER);
            Ok(Some((t, ours)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn command_of(text: &str) -> String {
    // `Exec=...` of the desktop entry, or the first `<string>` after ProgramArguments.
    text.lines()
        .find_map(|l| l.strip_prefix("Exec=").map(str::to_owned))
        .or_else(|| {
            let after = text.split("<key>ProgramArguments</key>").nth(1)?;
            let start = after.find("<string>")? + "<string>".len();
            let end = after[start..].find("</string>")? + start;
            Some(after[start..end].to_owned())
        })
        .unwrap_or_default()
}

/// Looks at what is set up.
pub fn status(ctx: &Context) -> Result<State, AutostartError> {
    if let Some(path) = ctx.file() {
        return match read_marked(&path).map_err(io(format!("reading {}", path.display())))? {
            None => Ok(State::Disabled),
            Some((_, false)) => Ok(State::Foreign { path }),
            Some((text, true)) => Ok(State::Enabled {
                command: command_of(&text),
                location: path.display().to_string(),
            }),
        };
    }
    Ok(match ctx.run_key.get().map_err(io("reading the registry"))? {
        Some(v) => State::Enabled { command: v, location: format!(r"HKCU\{RUN_KEY}\{NAME}") },
        None => State::Disabled,
    })
}

/// Sets autostart up to run `app`. Idempotent: running it again rewrites our own entry.
pub fn enable(ctx: &Context, app: &Path) -> Result<State, AutostartError> {
    match ctx.platform {
        Platform::Windows => {
            let value = run_value(app)?;
            ctx.run_key.set(&value).map_err(io("writing the registry"))?;
        }
        Platform::Linux | Platform::MacOs => {
            let path = ctx.file().ok_or(AutostartError::NoHome)?;
            if let Some((_, false)) =
                read_marked(&path).map_err(io(format!("reading {}", path.display())))?
            {
                return Err(AutostartError::Foreign(path));
            }
            let text = if ctx.platform == Platform::Linux {
                desktop_entry(app)
            } else {
                launch_agent(app)
            };
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(io(format!("creating {}", dir.display())))?;
            }
            ssx_core::settings::atomic_write(&path, text.as_bytes())
                .map_err(io(format!("writing {}", path.display())))?;
        }
    }
    status(ctx)
}

/// Removes what [`enable`] added. `Ok(false)` if there was nothing to remove.
pub fn disable(ctx: &Context) -> Result<bool, AutostartError> {
    if let Some(path) = ctx.file() {
        return match read_marked(&path).map_err(io(format!("reading {}", path.display())))? {
            None => Ok(false),
            Some((_, false)) => Err(AutostartError::Foreign(path)),
            Some((_, true)) => {
                std::fs::remove_file(&path).map_err(io(format!("removing {}", path.display())))?;
                Ok(true)
            }
        };
    }
    ctx.run_key.remove().map_err(io("removing the registry value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(platform: Platform, dir: &Path) -> Context {
        Context {
            platform,
            home: dir.to_path_buf(),
            xdg_config_home: None,
            run_key: Box::new(MemoryRunKey::default()),
        }
    }

    #[test]
    fn desktop_exec_quoting_follows_the_spec() {
        assert_eq!(desktop_exec_quote("/usr/bin/ssx-app"), "\"/usr/bin/ssx-app\"");
        assert_eq!(desktop_exec_quote("/opt/my apps/ssx-app"), "\"/opt/my apps/ssx-app\"");
        assert_eq!(
            desktop_exec_quote(r#"/a "b" $c `d` \e 100%"#),
            r#""/a \"b\" \$c \`d\` \\e 100%%""#
        );
    }

    #[test]
    fn the_desktop_entry_is_a_valid_autostart_entry() {
        let e = desktop_entry(Path::new("/home/u/.local/bin/ssx-app"));
        assert!(e.starts_with("[Desktop Entry]\n"));
        for want in [
            "Type=Application",
            "Exec=\"/home/u/.local/bin/ssx-app\"",
            "Terminal=false",
            "X-GNOME-Autostart-enabled=true",
            MARKER,
        ] {
            assert!(e.contains(want), "missing {want:?} in\n{e}");
        }
        assert_eq!(e.matches("Exec=").count(), 1);
    }

    #[test]
    fn the_plist_is_escaped_and_loads_at_login() {
        let p = launch_agent(Path::new("/Applications/ssx & co/ssx-app"));
        assert!(p.contains("<string>/Applications/ssx &amp; co/ssx-app</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains(&format!("<string>{LAUNCH_LABEL}</string>")));
        assert!(p.starts_with("<?xml"));
        assert_eq!(
            xml_escape("<a href=\"x\">&</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;"
        );
    }

    #[test]
    fn registry_values_are_quoted_and_quotes_are_refused() {
        assert_eq!(
            run_value(Path::new(r"C:\Program Files\ssx\ssx-app.exe")).unwrap(),
            r#""C:\Program Files\ssx\ssx-app.exe""#
        );
        assert!(matches!(
            run_value(Path::new("C:\\a\"b\\x.exe")),
            Err(AutostartError::Unquotable(_))
        ));
    }

    #[test]
    fn paths_follow_the_platform_conventions() {
        let home = Path::new("/home/u");
        assert_eq!(
            linux_entry_path(None, home),
            Path::new("/home/u/.config/autostart/ssx.desktop")
        );
        assert_eq!(
            linux_entry_path(Some(Path::new("/x/cfg")), home),
            Path::new("/x/cfg/autostart/ssx.desktop")
        );
        assert_eq!(
            linux_entry_path(Some(Path::new("relative")), home),
            Path::new("/home/u/.config/autostart/ssx.desktop"),
            "a relative XDG_CONFIG_HOME is ignored, as the spec says"
        );
        assert_eq!(
            mac_agent_path(home),
            Path::new("/home/u/Library/LaunchAgents/io.ssx.app.plist")
        );
    }

    #[test]
    fn linux_enable_status_disable_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(Platform::Linux, d.path());
        assert_eq!(status(&c).unwrap(), State::Disabled);
        assert!(!disable(&c).unwrap());
        let s = enable(&c, Path::new("/opt/ssx/ssx-app")).unwrap();
        let State::Enabled { command, location } = s else { panic!("{s:?}") };
        assert_eq!(command, "\"/opt/ssx/ssx-app\"");
        assert!(location.ends_with(".config/autostart/ssx.desktop"), "{location}");
        // Idempotent, and re-enabling with a new path rewrites our own entry.
        let s = enable(&c, Path::new("/opt/new/ssx-app")).unwrap();
        assert!(matches!(s, State::Enabled { command, .. } if command.contains("/opt/new/")));
        assert!(disable(&c).unwrap());
        assert_eq!(status(&c).unwrap(), State::Disabled);
        assert!(!d.path().join(".config/autostart/ssx.desktop").exists());
    }

    #[test]
    fn a_foreign_file_is_never_overwritten_or_removed() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(Platform::Linux, d.path());
        let path = linux_entry_path(None, d.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[Desktop Entry]\nExec=my-own-launcher\n").unwrap();
        assert!(matches!(status(&c).unwrap(), State::Foreign { .. }));
        assert!(matches!(enable(&c, Path::new("/x")), Err(AutostartError::Foreign(_))));
        assert!(matches!(disable(&c), Err(AutostartError::Foreign(_))));
        assert!(std::fs::read_to_string(&path).unwrap().contains("my-own-launcher"));
    }

    #[test]
    fn macos_writes_a_launch_agent() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(Platform::MacOs, d.path());
        let s = enable(&c, Path::new("/Applications/ssx.app/Contents/MacOS/ssx-app")).unwrap();
        assert!(
            matches!(&s, State::Enabled { command, .. } if command.ends_with("ssx-app")),
            "{s:?}"
        );
        assert!(mac_agent_path(d.path()).is_file());
        assert!(disable(&c).unwrap());
        assert!(!mac_agent_path(d.path()).exists());
    }

    #[test]
    fn windows_uses_the_run_value() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(Platform::Windows, d.path());
        assert_eq!(status(&c).unwrap(), State::Disabled);
        let s = enable(&c, Path::new(r"C:\ssx\ssx-app.exe")).unwrap();
        let State::Enabled { command, location } = s else { panic!() };
        assert_eq!(command, r#""C:\ssx\ssx-app.exe""#);
        assert!(location.contains("CurrentVersion\\Run\\ssx"), "{location}");
        assert!(disable(&c).unwrap());
        assert!(!disable(&c).unwrap(), "already gone");
        assert!(matches!(enable(&c, Path::new("C:\\a\"b")), Err(AutostartError::Unquotable(_))));
    }

    #[test]
    fn command_extraction_handles_both_formats() {
        assert_eq!(command_of("[Desktop Entry]\nExec=\"/a b\"\n"), "\"/a b\"");
        assert_eq!(command_of(&launch_agent(Path::new("/x/ssx-app"))), "/x/ssx-app");
        assert_eq!(command_of("nothing"), "");
    }
}
