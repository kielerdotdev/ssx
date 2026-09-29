//! The `.desktop` entry that authorises an application to use KWin's screenshot interface.
//!
//! `org.kde.KWin.ScreenShot2` is a *restricted interface*: KWin serves it only to programs
//! whose desktop file lists it in `X-KDE-DBUS-Restricted-Interfaces`, and it identifies
//! the caller by matching the executable path against that file's `Exec=` line. Since the
//! path is only known at install time, this module renders the entry text and leaves
//! placing it (`~/.local/share/applications/`, or `/usr/share/applications/` in a
//! package) to the installer.

/// The interface KWin restricts.
pub const KWIN_SCREENSHOT_INTERFACE: &str = "org.kde.KWin.ScreenShot2";

/// The exact desktop-file line that grants access. Packagers can add it to an existing
/// entry instead of installing a separate one.
pub const KWIN_RESTRICTED_INTERFACES_LINE: &str =
    "X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2";

/// Why a desktop entry could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DesktopEntryError {
    /// `Exec` must be the absolute path of the running executable: KWin compares paths.
    #[error("executable path {0:?} is not absolute")]
    RelativeExec(String),
    /// A newline or NUL would corrupt the key-file format.
    #[error("{0} contains a control character")]
    ControlCharacter(&'static str),
}

/// Renders a desktop entry that lets the program at `exec_path` use
/// `org.kde.KWin.ScreenShot2`.
///
/// `exec_path` must be absolute (use [`std::env::current_exe`]); `app_name` is the
/// display name. The entry is `NoDisplay=true` so it does not clutter menus; if the
/// application already ships a launcher, add [`KWIN_RESTRICTED_INTERFACES_LINE`] to that
/// instead.
///
/// After installing the file the application must be (re)started, and on some Plasma
/// versions a new login or `kbuildsycoca6` run is needed before KWin sees it.
pub fn kwin_desktop_entry(app_name: &str, exec_path: &str) -> Result<String, DesktopEntryError> {
    if app_name.chars().any(char::is_control) {
        return Err(DesktopEntryError::ControlCharacter("application name"));
    }
    if exec_path.chars().any(char::is_control) {
        return Err(DesktopEntryError::ControlCharacter("executable path"));
    }
    if !exec_path.starts_with('/') {
        return Err(DesktopEntryError::RelativeExec(exec_path.to_owned()));
    }
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name={app_name}\n\
         Comment=Screenshot access for {app_name}\n\
         Exec={exec}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         {KWIN_RESTRICTED_INTERFACES_LINE}\n",
        exec = quote_exec(exec_path),
    ))
}

/// Quotes an `Exec=` argument per the Desktop Entry spec: double quotes, with `"`, `` ` ``,
/// `$` and `\` backslash-escaped (and the whole result's backslashes doubled again by the
/// key-file string escaping), and `%` doubled so it is not a field code.
fn quote_exec(path: &str) -> String {
    let needs_quotes = path.chars().any(|c| " \t\n\"'\\><~|&;$*?#()`%".contains(c));
    if !needs_quotes {
        return path.to_owned();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_authorising_line_and_exec() {
        let e = kwin_desktop_entry("ssx", "/usr/bin/ssx").unwrap();
        assert!(e.starts_with("[Desktop Entry]\n"));
        assert!(e.contains("\nExec=/usr/bin/ssx\n"));
        assert!(e.contains("\nX-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2\n"));
        assert!(e.contains("\nNoDisplay=true\n"));
        assert!(e.ends_with('\n'));
    }

    #[test]
    fn quotes_awkward_paths() {
        let e = kwin_desktop_entry("ssx", "/opt/my apps/ssx").unwrap();
        assert!(e.contains("\nExec=\"/opt/my apps/ssx\"\n"));
        let e = kwin_desktop_entry("ssx", "/opt/100%/ssx").unwrap();
        assert!(e.contains("\nExec=\"/opt/100%%/ssx\"\n"));
        let e = kwin_desktop_entry("ssx", "/opt/a$b/ssx").unwrap();
        assert!(e.contains("\nExec=\"/opt/a\\\\$b/ssx\"\n"));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            kwin_desktop_entry("ssx", "ssx"),
            Err(DesktopEntryError::RelativeExec("ssx".into()))
        );
        assert!(kwin_desktop_entry("ss\nx", "/usr/bin/ssx").is_err());
        assert!(kwin_desktop_entry("ssx", "/usr/bin/ssx\nExec=/bin/sh").is_err());
        assert!(kwin_desktop_entry("ssx", "").is_err());
    }
}
