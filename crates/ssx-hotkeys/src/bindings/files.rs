//! Writing sway/Hyprland include files, and (opt-in) wiring them into the user's config.
//!
//! Three levels, each explicit:
//!
//! 1. [`write_include_file`]: writes *our* file (`config.d/ssx.conf`, `hypr/ssx.conf`).
//!    Safe: nothing reads it until it is included. Written atomically, and skipped when
//!    the content is unchanged.
//! 2. The include line the user must add ([`WriteReport::include_line`]).
//! 3. [`install_main_include`]: appends a **marked block** containing that line to the
//!    user's main config, idempotently; [`uninstall_main_include`] removes exactly that
//!    block. The main config is edited in place (not replaced by a rename) so symlinked
//!    dotfiles keep working, and it is never *created*: a fresh `~/.config/sway/config`
//!    would shadow the system default config.
//!
//! Only the blocks between the markers are ever modified or deleted.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{chord::Chord, command::Command};

use super::{
    BindingError, CommandRunner, Dirs, Result, Target, hyprland, runner::run_checked, sway,
};

/// Opening marker of the block ssx manages in the main config.
pub const BLOCK_BEGIN: &str = "# >>> ssx hotkeys >>>";
/// Closing marker.
pub const BLOCK_END: &str = "# <<< ssx hotkeys <<<";

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> BindingError {
    let path = path.to_owned();
    move |source| BindingError::Io { action, path, source }
}

/// Where our include file lives.
pub fn include_file_path(dirs: &Dirs, target: Target) -> Option<PathBuf> {
    match target {
        Target::Sway => Some(dirs.config_home.join("sway").join("config.d").join("ssx.conf")),
        Target::Hyprland => Some(dirs.config_home.join("hypr").join("ssx.conf")),
        Target::Gnome | Target::Kde => None,
    }
}

/// The user's main config file for `target`.
pub fn main_config_path(dirs: &Dirs, target: Target) -> Option<PathBuf> {
    match target {
        Target::Sway => Some(dirs.config_home.join("sway").join("config")),
        Target::Hyprland => Some(dirs.config_home.join("hypr").join("hyprland.conf")),
        Target::Gnome | Target::Kde => None,
    }
}

/// The exact line the user adds to their main config.
pub fn include_line(dirs: &Dirs, target: Target) -> Option<String> {
    let path = include_file_path(dirs, target)?;
    Some(match target {
        Target::Sway => sway::include_line(&path),
        _ => hyprland::source_line(&path),
    })
}

/// What [`write_include_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReport {
    /// The include file.
    pub path: PathBuf,
    /// The line to add to the main config to load it.
    pub include_line: String,
    /// `false` when the file already had exactly this content.
    pub changed: bool,
}

/// Renders and writes our include file for sway or Hyprland.
pub fn write_include_file(
    dirs: &Dirs,
    target: Target,
    bindings: &[(Chord, Command)],
) -> Result<WriteReport> {
    let content = match target {
        Target::Sway => sway::render(bindings)?,
        Target::Hyprland => hyprland::render(bindings)?,
        Target::Gnome | Target::Kde => return Err(BindingError::NotAFileTarget(target)),
    };
    let (Some(path), Some(include_line)) =
        (include_file_path(dirs, target), include_line(dirs, target))
    else {
        return Err(BindingError::NotAFileTarget(target));
    };
    if fs::read_to_string(&path).is_ok_and(|existing| existing == content) {
        return Ok(WriteReport { path, include_line, changed: false });
    }
    write_atomic(&path, content.as_bytes())?;
    Ok(WriteReport { path, include_line, changed: true })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(io_err("creating", dir))?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("ssx"),
        std::process::id()
    ));
    fs::write(&tmp, bytes).map_err(io_err("writing", &tmp))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        BindingError::Io { action: "replacing", path: path.to_owned(), source: e }
    })
}

/// State of the wiring, for `ssx hotkeys status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Our include file exists.
    pub include_file_exists: bool,
    /// The main config exists.
    pub main_config_exists: bool,
    /// The main config contains our marked block.
    pub block_installed: bool,
    /// The main config loads our file by a hand-written (unmarked) line.
    pub manually_included: bool,
}

/// Inspects the current wiring.
pub fn status(dirs: &Dirs, target: Target) -> Result<Status> {
    let (Some(include), Some(main), Some(line)) = (
        include_file_path(dirs, target),
        main_config_path(dirs, target),
        include_line(dirs, target),
    ) else {
        return Ok(Status {
            include_file_exists: false,
            main_config_exists: false,
            block_installed: false,
            manually_included: false,
        });
    };
    let text = match fs::read_to_string(&main) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err("reading", &main)(e)),
    };
    let block = text.as_deref().is_some_and(|t| find_block(t).is_some());
    let unmarked = text.as_deref().is_some_and(|t| {
        let without = find_block(t).map_or_else(|| t.to_owned(), |(s, e)| format!("{}{}", &t[..s], &t[e..]));
        without.lines().any(|l| l.trim() == line)
    });
    Ok(Status {
        include_file_exists: include.exists(),
        main_config_exists: text.is_some(),
        block_installed: block,
        manually_included: unmarked,
    })
}

/// Byte range of the managed block *including* the blank line ssx put before it and the
/// newline after the end marker, so removing it restores the original text.
fn find_block(text: &str) -> Option<(usize, usize)> {
    let begin = text.lines().scan(0usize, |pos, l| {
        let start = *pos;
        *pos += l.len() + 1;
        Some((start, l))
    });
    let mut start = None;
    let mut end = None;
    let mut offset_end = 0;
    for (pos, line) in begin {
        if start.is_none() && line.trim_end() == BLOCK_BEGIN {
            start = Some(pos);
        } else if start.is_some() && line.trim_end() == BLOCK_END {
            end = Some(pos);
            offset_end = pos + line.len();
            break;
        }
    }
    let (s, _) = (start?, end?);
    let mut e = offset_end;
    if text.as_bytes().get(e) == Some(&b'\r') {
        e += 1;
    }
    if text.as_bytes().get(e) == Some(&b'\n') {
        e += 1;
    }
    // Also swallow the single blank separator line we inserted before the block.
    let mut s = s;
    if text[..s].ends_with("\n\n") {
        s -= 1;
    }
    Some((s, e.min(text.len())))
}

/// What an install/uninstall did to the main config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MainConfigChange {
    /// The block was appended (install) or removed (uninstall).
    Changed,
    /// Nothing to do: already in the requested state.
    Unchanged,
    /// The user already includes our file by hand; left alone.
    AlreadyIncludedManually,
}

/// Appends the marked block to the main config (**explicit opt-in**). Idempotent.
///
/// Fails with [`BindingError::MainConfigMissing`] rather than creating the main config.
pub fn install_main_include(dirs: &Dirs, target: Target) -> Result<MainConfigChange> {
    let (Some(main), Some(line)) = (main_config_path(dirs, target), include_line(dirs, target))
    else {
        return Err(BindingError::NotAFileTarget(target));
    };
    let text = match fs::read_to_string(&main) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(BindingError::MainConfigMissing(main));
        }
        Err(e) => return Err(io_err("reading", &main)(e)),
    };
    if find_block(&text).is_some() {
        return Ok(MainConfigChange::Unchanged);
    }
    if text.lines().any(|l| l.trim() == line) {
        return Ok(MainConfigChange::AlreadyIncludedManually);
    }
    let mut new = text.clone();
    if !new.is_empty() {
        if !new.ends_with('\n') {
            new.push('\n');
        }
        new.push('\n'); // blank separator, removed again by uninstall
    }
    new.push_str(&format!(
        "{BLOCK_BEGIN}\n# Managed by ssx; `ssx hotkeys uninstall` removes this block.\n{line}\n{BLOCK_END}\n"
    ));
    fs::write(&main, new).map_err(io_err("writing", &main))?;
    Ok(MainConfigChange::Changed)
}

/// Removes the marked block from the main config. Idempotent; a missing main config is
/// not an error.
pub fn uninstall_main_include(dirs: &Dirs, target: Target) -> Result<MainConfigChange> {
    let Some(main) = main_config_path(dirs, target) else {
        return Ok(MainConfigChange::Unchanged);
    };
    let text = match fs::read_to_string(&main) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(MainConfigChange::Unchanged),
        Err(e) => return Err(io_err("reading", &main)(e)),
    };
    let Some((s, e)) = find_block(&text) else { return Ok(MainConfigChange::Unchanged) };
    let new = format!("{}{}", &text[..s], &text[e..]);
    fs::write(&main, new).map_err(io_err("writing", &main))?;
    Ok(MainConfigChange::Changed)
}

/// Removes the block *and* deletes our include file.
pub fn uninstall(dirs: &Dirs, target: Target) -> Result<()> {
    uninstall_main_include(dirs, target)?;
    if let Some(path) = include_file_path(dirs, target) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("removing", &path)(e)),
        }
    }
    Ok(())
}

/// Asks the running compositor to reload its config (`swaymsg reload` / `hyprctl reload`).
/// Explicit and separate: reloading can flash the screen and reset transient state.
pub fn reload(runner: &dyn CommandRunner, target: Target) -> Result<()> {
    let (program, args, hint) = match target {
        Target::Sway => ("swaymsg", vec!["reload".to_owned()], "is sway running and installed?"),
        Target::Hyprland => ("hyprctl", vec!["reload".to_owned()], "is Hyprland running and installed?"),
        Target::Gnome | Target::Kde => return Ok(()),
    };
    run_checked(runner, program, &args, hint).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::{runner::fake::Fake, tests::fixture};

    fn temp(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("ssx-hotkeys-files-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Dirs::under(&root)
    }

    fn write_main(dirs: &Dirs, target: Target, text: &str) -> PathBuf {
        let p = main_config_path(dirs, target).unwrap();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn paths_follow_the_documented_locations() {
        let d = Dirs::under("/r");
        assert_eq!(include_file_path(&d, Target::Sway).unwrap(), PathBuf::from("/r/.config/sway/config.d/ssx.conf"));
        assert_eq!(include_file_path(&d, Target::Hyprland).unwrap(), PathBuf::from("/r/.config/hypr/ssx.conf"));
        assert_eq!(include_line(&d, Target::Sway).unwrap(), "include /r/.config/sway/config.d/ssx.conf");
        assert_eq!(include_line(&d, Target::Hyprland).unwrap(), "source = /r/.config/hypr/ssx.conf");
        assert!(include_file_path(&d, Target::Gnome).is_none());
    }

    #[test]
    fn include_file_is_written_once_then_left_alone() {
        for target in [Target::Sway, Target::Hyprland] {
            let d = temp("write");
            let r1 = write_include_file(&d, target, &fixture()).unwrap();
            assert!(r1.changed);
            let text = fs::read_to_string(&r1.path).unwrap();
            assert!(text.contains("Generated by ssx"));
            let mtime = fs::metadata(&r1.path).unwrap().modified().unwrap();
            let r2 = write_include_file(&d, target, &fixture()).unwrap();
            assert!(!r2.changed, "identical content must not be rewritten");
            assert_eq!(fs::metadata(&r2.path).unwrap().modified().unwrap(), mtime);
            let mut other = fixture();
            other.pop();
            assert!(write_include_file(&d, target, &other).unwrap().changed);
            // No stray temp files.
            let leftovers: Vec<_> = fs::read_dir(r1.path.parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
                .collect();
            assert!(leftovers.is_empty());
        }
    }

    #[test]
    fn install_appends_block_idempotently_and_uninstall_restores_bytes() {
        for target in [Target::Sway, Target::Hyprland] {
            for original in ["set $mod Mod4\nbindsym $mod+Return exec foot\n", "no trailing newline", ""] {
                let d = temp("block");
                let main = write_main(&d, target, original);
                assert_eq!(install_main_include(&d, target).unwrap(), MainConfigChange::Changed);
                let installed = fs::read_to_string(&main).unwrap();
                assert!(installed.starts_with(original));
                assert_eq!(installed.matches(BLOCK_BEGIN).count(), 1);
                assert!(installed.contains(&include_line(&d, target).unwrap()));
                // Idempotent.
                assert_eq!(install_main_include(&d, target).unwrap(), MainConfigChange::Unchanged);
                assert_eq!(fs::read_to_string(&main).unwrap(), installed);
                assert!(status(&d, target).unwrap().block_installed);
                // Remove: byte-identical (modulo a newline added to an unterminated file).
                assert_eq!(uninstall_main_include(&d, target).unwrap(), MainConfigChange::Changed);
                let restored = fs::read_to_string(&main).unwrap();
                let want = if original.is_empty() || original.ends_with('\n') { original.to_owned() } else { format!("{original}\n") };
                assert_eq!(restored, want);
                assert_eq!(uninstall_main_include(&d, target).unwrap(), MainConfigChange::Unchanged);
                assert!(!status(&d, target).unwrap().block_installed);
            }
        }
    }

    #[test]
    fn user_text_around_and_after_the_block_is_preserved() {
        let d = temp("around");
        let main = write_main(&d, Target::Sway, "before\n");
        install_main_include(&d, Target::Sway).unwrap();
        let mut text = fs::read_to_string(&main).unwrap();
        text.push_str("after the block\nbindsym x y\n");
        fs::write(&main, &text).unwrap();
        uninstall_main_include(&d, Target::Sway).unwrap();
        assert_eq!(fs::read_to_string(&main).unwrap(), "before\nafter the block\nbindsym x y\n");
    }

    #[test]
    fn crlf_config_is_handled() {
        let d = temp("crlf");
        let main = write_main(&d, Target::Hyprland, "a = 1\r\nb = 2\r\n");
        install_main_include(&d, Target::Hyprland).unwrap();
        assert_eq!(install_main_include(&d, Target::Hyprland).unwrap(), MainConfigChange::Unchanged);
        uninstall_main_include(&d, Target::Hyprland).unwrap();
        assert!(fs::read_to_string(&main).unwrap().starts_with("a = 1\r\nb = 2\r\n"));
    }

    #[test]
    fn hand_written_include_is_respected() {
        let d = temp("manual");
        let line = include_line(&d, Target::Sway).unwrap();
        let main = write_main(&d, Target::Sway, &format!("x\n{line}\n"));
        assert_eq!(install_main_include(&d, Target::Sway).unwrap(), MainConfigChange::AlreadyIncludedManually);
        assert_eq!(fs::read_to_string(&main).unwrap(), format!("x\n{line}\n"));
        let st = status(&d, Target::Sway).unwrap();
        assert!(st.manually_included && !st.block_installed);
        // Uninstall never touches text outside the markers.
        assert_eq!(uninstall_main_include(&d, Target::Sway).unwrap(), MainConfigChange::Unchanged);
    }

    #[test]
    fn missing_main_config_is_never_created() {
        let d = temp("missing");
        let err = install_main_include(&d, Target::Sway).unwrap_err();
        assert!(matches!(err, BindingError::MainConfigMissing(_)), "{err}");
        assert!(!main_config_path(&d, Target::Sway).unwrap().exists());
        assert_eq!(uninstall_main_include(&d, Target::Sway).unwrap(), MainConfigChange::Unchanged);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_main_config_keeps_its_link() {
        let d = temp("symlink");
        let real = d.config_home.join("dotfiles").join("sway-config");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "orig\n").unwrap();
        let link = main_config_path(&d, Target::Sway).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        install_main_include(&d, Target::Sway).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(fs::read_to_string(&real).unwrap().contains(BLOCK_BEGIN));
        uninstall_main_include(&d, Target::Sway).unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "orig\n");
    }

    #[test]
    fn full_uninstall_removes_block_and_include_file() {
        let d = temp("full");
        write_main(&d, Target::Hyprland, "keep\n");
        let r = write_include_file(&d, Target::Hyprland, &fixture()).unwrap();
        install_main_include(&d, Target::Hyprland).unwrap();
        uninstall(&d, Target::Hyprland).unwrap();
        assert!(!r.path.exists());
        assert_eq!(fs::read_to_string(main_config_path(&d, Target::Hyprland).unwrap()).unwrap(), "keep\n");
        uninstall(&d, Target::Hyprland).unwrap(); // idempotent
    }

    #[test]
    fn reload_runs_the_compositor_tool() {
        let f = Fake::default();
        reload(&f, Target::Sway).unwrap();
        reload(&f, Target::Hyprland).unwrap();
        assert_eq!(f.calls_text(), ["swaymsg reload", "hyprctl reload"]);
        reload(&f, Target::Gnome).unwrap();
        assert_eq!(f.calls.borrow().len(), 2);
        f.canned.borrow_mut().insert("swaymsg reload".into(), crate::bindings::RunOutput::failed("no ipc"));
        let e = reload(&f, Target::Sway).unwrap_err();
        assert!(e.to_string().contains("no ipc"), "{e}");
    }
}
