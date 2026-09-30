//! Thunar custom actions: entries in `~/.config/Thunar/uca.xml`.
//!
//! `uca.xml` is *shared, user-edited state* (Thunar's own "Configure custom actions" dialog
//! writes it), so we must never regenerate it. Instead we parse it with `quick-xml` only to
//! learn the byte spans of the `<action>` elements and then splice text: our entries are
//! inserted before `</actions>` and removed again by span. Everything else - unknown elements,
//! comments, attribute order, whitespace, other actions - is preserved byte for byte, and an
//! install followed by an uninstall restores the original file exactly.
//!
//! Entries are identified by `<unique-id>` starting with `ssx-shell-`.
//!
//! Thunar splits `<command>` with GLib shell rules (`g_shell_parse_argv`) *before* expanding
//! `%F`, so each path becomes exactly one argv element; our executable is single-quoted.
//! Thunar only reads `uca.xml` at start-up: restart with `thunar -q`.

use std::fs;
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::action::{Action, FilterKind};
use crate::context::{Context, Platform};
use crate::error::{Result, ShellError};
use crate::fsutil::{prune_empty_dirs, write_atomic};
use crate::integration::{Description, Detection, InstallOutcome, Integration, UninstallOutcome};
use crate::quote::{shell_word_with_field_codes, xml_escape_text};

const ID_PREFIX: &str = "ssx-shell-";

const SKELETON_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n";
const SKELETON_TAIL: &str = "</actions>\n";

/// Thunar integration.
#[derive(Debug, Clone, Copy, Default)]
pub struct Thunar;

impl Thunar {
    fn path(ctx: &Context) -> PathBuf {
        ctx.config_home.join("Thunar").join("uca.xml")
    }
}

/// Byte span of one `<action>` element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionSpan {
    pub start: usize,
    pub end: usize,
    pub unique_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Root {
    /// `<actions> ... </actions>`; `close` is the offset of `</actions>`.
    Open { close: usize },
    /// `<actions/>` at these offsets.
    SelfClosed { start: usize, end: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scan {
    pub root: Root,
    pub actions: Vec<ActionSpan>,
}

fn parse_err(path: &Path, reason: impl ToString) -> ShellError {
    ShellError::Parse { path: path.to_path_buf(), reason: reason.to_string() }
}

/// Locates the root and the `<action>` children of a `uca.xml` document.
pub(crate) fn scan(xml: &str, path: &Path) -> Result<Scan> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0_usize;
    let mut root: Option<Root> = None;
    let mut actions = Vec::new();
    let mut cur: Option<ActionSpan> = None;
    let mut in_uid = false;
    let mut uid = String::new();
    loop {
        let before = reader.buffer_position() as usize;
        let ev = reader.read_event().map_err(|e| parse_err(path, e))?;
        let after = reader.buffer_position() as usize;
        match ev {
            Event::Start(e) => {
                let name = e.name();
                match (depth, name.into_inner()) {
                    (0, "actions") => root = Some(Root::Open { close: usize::MAX }),
                    (0, other) => {
                        return Err(parse_err(
                            path,
                            format!("root element is <{}>, expected <actions>", other),
                        ));
                    }
                    (1, "action") => {
                        cur = Some(ActionSpan { start: before, end: after, unique_id: None })
                    }
                    (2, "unique-id") if cur.is_some() => {
                        in_uid = true;
                        uid.clear();
                    }
                    _ => {}
                }
                depth += 1;
            }
            Event::Empty(e) => match (depth, e.name().into_inner()) {
                (0, "actions") => root = Some(Root::SelfClosed { start: before, end: after }),
                (0, other) => {
                    return Err(parse_err(
                        path,
                        format!("root element is <{}>, expected <actions>", other),
                    ));
                }
                (1, "action") => {
                    actions.push(ActionSpan { start: before, end: after, unique_id: None })
                }
                _ => {}
            },
            Event::Text(t) if in_uid => uid.push_str(&t),
            Event::End(e) => {
                depth = depth.checked_sub(1).ok_or_else(|| parse_err(path, "unbalanced tags"))?;
                match (depth, e.name().into_inner()) {
                    (0, "actions") => {
                        if let Some(Root::Open { close }) = root.as_mut() {
                            *close = before;
                        }
                    }
                    (1, "action") => {
                        if let Some(mut span) = cur.take() {
                            span.end = after;
                            actions.push(span);
                        }
                    }
                    (2, "unique-id") if in_uid => {
                        in_uid = false;
                        if let Some(span) = cur.as_mut() {
                            span.unique_id = Some(uid.trim().to_owned());
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let root = match root {
        Some(Root::Open { close }) if close == usize::MAX => {
            return Err(parse_err(path, "missing </actions>"));
        }
        Some(r) => r,
        None => return Err(parse_err(path, "no <actions> root element")),
    };
    if depth != 0 {
        return Err(parse_err(path, "unbalanced tags"));
    }
    Ok(Scan { root, actions })
}

/// The `<action>` element text for one action (no trailing newline).
pub(crate) fn action_block(ctx: &Context, a: &Action) -> Result<String> {
    let exe = shell_word_with_field_codes(ctx.exe_str()?);
    let args: Vec<String> = a.exec_args.iter().map(|x| shell_word_with_field_codes(x)).collect();
    let code = if a.multi_select { "%F" } else { "%f" };
    let command = format!("{exe} {} -- {code}", args.join(" "));
    let conditions = match a.filter.kind {
        FilterKind::Any => {
            "\t<directories/>\n\t<audio-files/>\n\t<image-files/>\n\t<other-files/>\n\t<text-files/>\n\t<video-files/>\n"
        }
        FilterKind::Images => "\t<image-files/>\n",
        FilterKind::Videos => "\t<video-files/>\n",
        FilterKind::Custom => {
            "\t<directories/>\n\t<audio-files/>\n\t<image-files/>\n\t<other-files/>\n\t<text-files/>\n\t<video-files/>\n"
        }
    };
    let patterns = if matches!(a.filter.kind, FilterKind::Custom) && !a.filter.extensions.is_empty()
    {
        a.filter.extensions.iter().map(|e| format!("*.{e}")).collect::<Vec<_>>().join(";")
    } else {
        "*".to_owned()
    };
    Ok(format!(
        "<action>\n\
         \t<icon>{icon}</icon>\n\
         \t<name>{name}</name>\n\
         \t<unique-id>{ID_PREFIX}{id}</unique-id>\n\
         \t<command>{command}</command>\n\
         \t<description>{desc}</description>\n\
         \t<patterns>{patterns}</patterns>\n\
         {conditions}\
         </action>",
        icon = xml_escape_text(&a.icon),
        name = xml_escape_text(&a.label),
        id = a.id,
        command = xml_escape_text(&command),
        desc = xml_escape_text(&a.description),
        patterns = xml_escape_text(&patterns),
    ))
}

fn ours(span: &ActionSpan) -> bool {
    span.unique_id.as_deref().is_some_and(|id| id.starts_with(ID_PREFIX))
}

/// A text edit: replace `start..end` with `text`.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

fn apply(mut xml: String, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    for e in edits {
        xml.replace_range(e.start..e.end, &e.text);
    }
    xml
}

/// Start of the line containing `pos` if only blanks precede `pos` on it, else `pos`.
fn line_start_if_blank(xml: &str, pos: usize) -> usize {
    let before = &xml[..pos];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    if before[line_start..].chars().all(|c| c == ' ' || c == '\t') { line_start } else { pos }
}

/// Computes the new document with our blocks merged in. Pure function; unit-tested.
pub(crate) fn merge(xml: Option<&str>, blocks: &[(String, String)], path: &Path) -> Result<String> {
    let Some(xml) = xml.filter(|x| !x.trim().is_empty()) else {
        let mut out = String::from(SKELETON_HEAD);
        for (_, b) in blocks {
            out.push_str(b);
            out.push('\n');
        }
        out.push_str(SKELETON_TAIL);
        return Ok(out);
    };
    let sc = scan(xml, path)?;
    let mut edits = Vec::new();
    let mut appended = String::new();
    for (id, block) in blocks {
        let want_id = format!("{ID_PREFIX}{id}");
        let mut matching = sc.actions.iter().filter(|s| s.unique_id.as_deref() == Some(&want_id));
        match matching.next() {
            Some(first) => {
                if &xml[first.start..first.end] != block {
                    edits.push(Edit { start: first.start, end: first.end, text: block.clone() });
                }
                // Duplicate ids (hand-edited file): drop the extras.
                for dup in matching {
                    edits.push(removal(xml, dup));
                }
            }
            None => {
                appended.push_str(block);
                appended.push('\n');
            }
        }
    }
    // Stale entries of ours (an action that no longer exists).
    for s in sc.actions.iter().filter(|s| ours(s)) {
        let known = blocks
            .iter()
            .any(|(id, _)| s.unique_id.as_deref() == Some(&format!("{ID_PREFIX}{id}")));
        if !known {
            edits.push(removal(xml, s));
        }
    }
    match sc.root {
        Root::Open { close } => {
            if !appended.is_empty() {
                let at = line_start_if_blank(xml, close);
                let lead = if at > 0 && !xml[..at].ends_with('\n') { "\n" } else { "" };
                edits.push(Edit { start: at, end: at, text: format!("{lead}{appended}") });
            }
        }
        Root::SelfClosed { start, end } => {
            edits.push(Edit { start, end, text: format!("<actions>\n{appended}</actions>") });
        }
    }
    Ok(apply(xml.to_owned(), edits))
}

/// Edit removing `span` plus its line break when it occupies whole lines.
fn removal(xml: &str, span: &ActionSpan) -> Edit {
    let at_line_start = span.start == 0 || xml[..span.start].ends_with('\n');
    let mut end = span.end;
    if at_line_start {
        if xml[end..].starts_with("\r\n") {
            end += 2;
        } else if xml[end..].starts_with('\n') {
            end += 1;
        }
    }
    Edit { start: span.start, end, text: String::new() }
}

/// Document with all of our entries removed; `None` if nothing changes.
pub(crate) fn strip(xml: &str, path: &Path) -> Result<Option<String>> {
    let sc = scan(xml, path)?;
    let edits: Vec<Edit> = sc.actions.iter().filter(|s| ours(s)).map(|s| removal(xml, s)).collect();
    if edits.is_empty() {
        return Ok(None);
    }
    Ok(Some(apply(xml.to_owned(), edits)))
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ShellError::io("reading", path)(e)),
    }
}

/// Follow a symlinked `uca.xml` (dotfile managers) instead of replacing the link.
fn write_target(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

fn existing_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).map_or(0o644, |m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o644
    }
}

fn blocks(ctx: &Context) -> Result<Vec<(String, String)>> {
    ctx.actions.iter().map(|a| Ok((a.id.clone(), action_block(ctx, a)?))).collect()
}

impl Integration for Thunar {
    fn id(&self) -> &'static str {
        "thunar"
    }

    fn name(&self) -> &'static str {
        "Thunar (Xfce)"
    }

    fn platform(&self) -> Platform {
        Platform::Linux
    }

    fn detect(&self, ctx: &Context) -> Detection {
        if ctx.has_binary("thunar") {
            Detection::found("`thunar` found in PATH")
        } else if ctx.config_home.join("Thunar").is_dir() {
            Detection::found("~/.config/Thunar exists")
        } else {
            Detection::missing("Thunar is not installed")
        }
    }

    fn install(&self, ctx: &Context) -> Result<InstallOutcome> {
        let path = Self::path(ctx);
        let target = write_target(&path);
        let old = read(&target)?;
        let new = merge(old.as_deref(), &blocks(ctx)?, &path)?;
        if old.as_deref() == Some(new.as_str()) {
            return Ok(InstallOutcome::AlreadyPresent);
        }
        let mode = existing_mode(&target);
        write_atomic(&target, new.as_bytes(), mode)?;
        let had_ours = match &old {
            Some(o) if !o.trim().is_empty() => scan(o, &path)?.actions.iter().any(ours),
            _ => false,
        };
        Ok(if had_ours { InstallOutcome::Updated } else { InstallOutcome::Installed })
    }

    fn uninstall(&self, ctx: &Context) -> Result<UninstallOutcome> {
        let path = Self::path(ctx);
        let target = write_target(&path);
        let Some(old) = read(&target)? else { return Ok(UninstallOutcome::NotPresent) };
        let Some(new) = strip(&old, &path)? else { return Ok(UninstallOutcome::NotPresent) };
        let empty_skeleton = format!("{SKELETON_HEAD}{SKELETON_TAIL}");
        if new == empty_skeleton {
            // We created this file; leave no trace.
            fs::remove_file(&target).map_err(ShellError::io("removing", &target))?;
            if let Some(dir) = target.parent() {
                prune_empty_dirs(dir, ctx);
            }
        } else {
            let mode = existing_mode(&target);
            write_atomic(&target, new.as_bytes(), mode)?;
        }
        Ok(UninstallOutcome::Removed)
    }

    fn is_installed(&self, ctx: &Context) -> Result<bool> {
        let path = Self::path(ctx);
        let Some(xml) = read(&write_target(&path))? else { return Ok(false) };
        if xml.trim().is_empty() {
            return Ok(false);
        }
        Ok(merge(Some(&xml), &blocks(ctx)?, &path)? == xml)
    }

    fn describe(&self, ctx: &Context) -> Description {
        Description::paths("thunar", "Thunar custom actions (merged into uca.xml)", &[Self::path(ctx)])
            .note("existing custom actions are preserved; only entries with unique-id `ssx-shell-*` are ours")
            .note("Thunar reads uca.xml at start-up: run `thunar -q` and reopen it")
    }
}
