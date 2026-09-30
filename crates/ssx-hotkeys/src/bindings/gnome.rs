//! GNOME: custom keybindings through `gsettings`.
//!
//! GNOME Settings > Keyboard > Custom Shortcuts stores each shortcut as a *relocatable*
//! schema instance: `org.gnome.settings-daemon.plugins.media-keys.custom-keybinding` at a
//! path such as `/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/`,
//! with keys `name`, `command` and `binding`, and lists those paths in the
//! `custom-keybindings` key of `org.gnome.settings-daemon.plugins.media-keys`. This module
//! reproduces exactly that, under paths we own (`.../ssx-<slug>/`), so it works on every
//! GNOME version (unlike the GlobalShortcuts portal, which needs GNOME 48+).
//!
//! * [`commands`] returns the exact `gsettings` invocations; [`script`] renders them as a
//!   shell script for users who prefer to run them by hand.
//! * [`apply`] runs them through a [`CommandRunner`], **merging** into the existing
//!   `custom-keybindings` list (other entries and their order are untouched) and skipping
//!   writes when the values already match, so it is idempotent.
//! * [`remove`] deletes only the entries whose path component starts with `ssx-`.
//!
//! `command` is parsed by GLib's `g_shell_parse_argv` (no shell), for which POSIX
//! single-quoting is correct; the GVariant string wrapping around it is done by
//! [`gvariant_string`]. GNOME's own bindings (notably `Print` for the screenshot UI) take
//! precedence over or clash with custom ones; pick keys GNOME does not use.

use crate::{
    chord::Chord,
    command::{Command, gvariant_string, gvariant_string_array},
};

use super::{BindingError, CommandRunner, Result, runner::run_checked, slugs, validate};

/// Schema holding the list of custom keybinding paths.
pub const SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
/// Relocatable schema of a single custom keybinding.
pub const CUSTOM_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
/// Key in [`SCHEMA`] listing the custom keybinding paths.
pub const LIST_KEY: &str = "custom-keybindings";
/// Common prefix of every custom keybinding path.
pub const PATH_PREFIX: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/";
/// Prefix of the path component of entries owned by ssx.
pub const OWNED_PREFIX: &str = "ssx-";

const HINT: &str = "install GNOME's settings schemas (gsettings-desktop-schemas / gnome-settings-daemon) and run this inside a GNOME session";

/// One custom keybinding to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GnomeEntry {
    /// The dconf path (with trailing slash).
    pub path: String,
    /// Shown in GNOME Settings.
    pub name: String,
    /// Parsed by `g_shell_parse_argv`.
    pub command: String,
    /// GTK accelerator, e.g. `<Control><Shift>s`.
    pub binding: String,
}

impl GnomeEntry {
    /// `SCHEMA:PATH` as `gsettings` wants it.
    pub fn schema_with_path(&self) -> String {
        format!("{CUSTOM_SCHEMA}:{}", self.path)
    }
}

/// Builds the entries for `bindings`.
pub fn entries(bindings: &[(Chord, Command)]) -> Result<Vec<GnomeEntry>> {
    validate(bindings)?;
    Ok(bindings
        .iter()
        .zip(slugs(bindings))
        .map(|((chord, cmd), slug)| {
            let component =
                if slug.starts_with(OWNED_PREFIX) { slug } else { format!("{OWNED_PREFIX}{slug}") };
            GnomeEntry {
                path: format!("{PATH_PREFIX}{component}/"),
                name: cmd.display_name(),
                command: cmd.shell_line(),
                binding: chord.to_gnome_accelerator(),
            }
        })
        .collect())
}

/// The existing list with `ours` appended (skipping paths already present). Existing
/// entries keep their order and are never removed.
pub fn merged_list(existing: &[String], ours: &[String]) -> Vec<String> {
    let mut out = existing.to_vec();
    for p in ours {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

fn gsettings(args: &[&str]) -> Vec<String> {
    std::iter::once("gsettings").chain(args.iter().copied()).map(str::to_owned).collect()
}

/// The exact `gsettings` invocations (program first) that install `entries` on top of an
/// `existing` list: three `set`s per entry, then the merged list.
pub fn commands(entries: &[GnomeEntry], existing: &[String]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for e in entries {
        let target = e.schema_with_path();
        out.push(gsettings(&["set", &target, "name", &gvariant_string(&e.name)]));
        out.push(gsettings(&["set", &target, "command", &gvariant_string(&e.command)]));
        out.push(gsettings(&["set", &target, "binding", &gvariant_string(&e.binding)]));
    }
    let ours: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    out.push(gsettings(&["set", SCHEMA, LIST_KEY, &gvariant_string_array(&merged_list(existing, &ours))]));
    out
}

/// [`commands`] as a shell script.
pub fn script(entries: &[GnomeEntry], existing: &[String]) -> String {
    let mut s = String::from(
        "#!/bin/sh\n# Registers ssx shortcuts as GNOME custom keybindings (generated by ssx).\nset -e\n",
    );
    for cmd in commands(entries, existing) {
        s.push_str(&crate::command::Command::new(&cmd[0]).args(cmd[1..].iter().cloned()).shell_line());
        s.push('\n');
    }
    s
}

/// Parses one GVariant text-format string (`'abc'` or `"it's"`) from the start of `s`,
/// returning the value and the unparsed rest.
pub fn parse_gvariant_string(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let mut chars = s.char_indices();
    let (_, quote) = chars.next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        if c == quote {
            return Some((out, &s[i + c.len_utf8()..]));
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        let (_, e) = chars.next()?;
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\u{7}'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            'u' | 'U' => {
                let n = if e == 'u' { 4 } else { 8 };
                let mut code = 0u32;
                for _ in 0..n {
                    code = code * 16 + chars.next()?.1.to_digit(16)?;
                }
                out.push(char::from_u32(code)?);
            }
            other => out.push(other),
        }
    }
    None
}

/// Parses `gsettings get` output for an `as` key: `['a', 'b']`, `@as []`, `[]`.
pub fn parse_gvariant_string_array(text: &str) -> Option<Vec<String>> {
    let mut rest = text.trim();
    if let Some(r) = rest.strip_prefix("@as") {
        rest = r.trim_start();
    }
    rest = rest.strip_prefix('[')?.trim_start();
    let mut out = Vec::new();
    loop {
        if let Some(r) = rest.strip_prefix(']') {
            return r.trim().is_empty().then_some(out);
        }
        let (item, r) = parse_gvariant_string(rest)?;
        out.push(item);
        rest = r.trim_start();
        if let Some(r) = rest.strip_prefix(',') {
            rest = r.trim_start();
        } else if !rest.starts_with(']') {
            return None;
        }
    }
}

/// What [`apply`] / [`remove`] changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyReport {
    /// Number of individual `name`/`command`/`binding` values written.
    pub values_written: usize,
    /// The `custom-keybindings` list was rewritten.
    pub list_updated: bool,
}

impl ApplyReport {
    /// `true` when nothing had to be written.
    pub fn is_noop(&self) -> bool {
        self.values_written == 0 && !self.list_updated
    }
}

fn read_list(runner: &dyn CommandRunner) -> Result<Vec<String>> {
    let args = gsettings(&["get", SCHEMA, LIST_KEY]);
    let out = run_checked(runner, "gsettings", &args[1..], HINT)?;
    parse_gvariant_string_array(&out.stdout).ok_or_else(|| BindingError::UnexpectedOutput {
        command: format!("gsettings get {SCHEMA} {LIST_KEY}"),
        output: out.stdout,
    })
}

fn read_string(runner: &dyn CommandRunner, schema_path: &str, key: &str) -> Option<String> {
    let args = gsettings(&["get", schema_path, key]);
    let out = runner.run("gsettings", &args[1..]).ok().filter(|o| o.success)?;
    parse_gvariant_string(&out.stdout).map(|(v, _)| v)
}

/// Installs `bindings` as GNOME custom keybindings. Idempotent; merges into the existing
/// list without touching other entries.
pub fn apply(runner: &dyn CommandRunner, bindings: &[(Chord, Command)]) -> Result<ApplyReport> {
    let entries = entries(bindings)?;
    let existing = read_list(runner)?;
    let mut report = ApplyReport::default();
    for e in &entries {
        let target = e.schema_with_path();
        for (key, want) in [("name", &e.name), ("command", &e.command), ("binding", &e.binding)] {
            if read_string(runner, &target, key).as_deref() == Some(want.as_str()) {
                continue;
            }
            let args = gsettings(&["set", &target, key, &gvariant_string(want)]);
            run_checked(runner, "gsettings", &args[1..], HINT)?;
            report.values_written += 1;
        }
    }
    let ours: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    let merged = merged_list(&existing, &ours);
    if merged != existing {
        let args = gsettings(&["set", SCHEMA, LIST_KEY, &gvariant_string_array(&merged)]);
        run_checked(runner, "gsettings", &args[1..], HINT)?;
        report.list_updated = true;
    }
    Ok(report)
}

/// Removes every ssx-owned custom keybinding (path component starting with `ssx-`),
/// leaving other entries alone. Idempotent.
pub fn remove(runner: &dyn CommandRunner) -> Result<ApplyReport> {
    let existing = read_list(runner)?;
    let is_ours = |p: &String| {
        p.strip_prefix(PATH_PREFIX)
            .is_some_and(|rest| rest.starts_with(OWNED_PREFIX))
    };
    let (ours, keep): (Vec<String>, Vec<String>) = existing.iter().cloned().partition(is_ours);
    let mut report = ApplyReport::default();
    if ours.is_empty() {
        return Ok(report);
    }
    // Update the list first so GNOME never sees a listed entry with reset values.
    let args = gsettings(&["set", SCHEMA, LIST_KEY, &gvariant_string_array(&keep)]);
    run_checked(runner, "gsettings", &args[1..], HINT)?;
    report.list_updated = true;
    for p in &ours {
        let args = gsettings(&["reset-recursively", &format!("{CUSTOM_SCHEMA}:{p}")]);
        run_checked(runner, "gsettings", &args[1..], HINT)?;
        report.values_written += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, io};

    use super::*;
    use crate::bindings::{RunOutput, tests::fixture};

    /// An in-memory `gsettings` with the semantics that matter here.
    #[derive(Default)]
    struct FakeGsettings {
        store: RefCell<BTreeMap<String, String>>,
        log: RefCell<Vec<String>>,
        no_schema: bool,
    }

    impl FakeGsettings {
        fn with_list(list: &[&str]) -> Self {
            let f = Self::default();
            f.store.borrow_mut().insert(
                format!("{SCHEMA} {LIST_KEY}"),
                gvariant_string_array(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()),
            );
            f
        }
        fn list(&self) -> Vec<String> {
            parse_gvariant_string_array(
                self.store.borrow().get(&format!("{SCHEMA} {LIST_KEY}")).map_or("@as []", |s| s),
            )
            .unwrap()
        }
        fn writes(&self) -> usize {
            self.log.borrow().iter().filter(|l| l.starts_with("set ") || l.starts_with("reset")).count()
        }
    }

    impl CommandRunner for FakeGsettings {
        fn run(&self, program: &str, args: &[String]) -> io::Result<RunOutput> {
            assert_eq!(program, "gsettings");
            self.log.borrow_mut().push(args.join(" "));
            if self.no_schema {
                return Ok(RunOutput::failed(format!("No such schema “{}”", args.get(1).map_or("", |s| s.as_str()))));
            }
            match args[0].as_str() {
                "get" => {
                    let key = format!("{} {}", args[1], args[2]);
                    Ok(match self.store.borrow().get(&key) {
                        Some(v) if v.starts_with('[') && v.len() > 2 => RunOutput::ok(format!("{v}\n")),
                        Some(v) if v == "[]" => RunOutput::ok("@as []\n"),
                        Some(v) => RunOutput::ok(format!("{v}\n")),
                        None if args[2] == LIST_KEY => RunOutput::ok("@as []\n"),
                        None => RunOutput::ok("''\n"),
                    })
                }
                "set" => {
                    self.store.borrow_mut().insert(format!("{} {}", args[1], args[2]), args[3].clone());
                    Ok(RunOutput::ok(""))
                }
                "reset-recursively" => {
                    let prefix = format!("{} ", args[1]);
                    self.store.borrow_mut().retain(|k, _| !k.starts_with(&prefix));
                    Ok(RunOutput::ok(""))
                }
                other => panic!("unexpected gsettings verb {other}"),
            }
        }
    }

    #[test]
    fn entries_have_owned_paths_and_gnome_syntax() {
        let e = entries(&fixture()).unwrap();
        assert_eq!(e[0].path, format!("{PATH_PREFIX}ssx-capture-region/"));
        assert_eq!(e[0].binding, "<Control><Shift>s");
        assert_eq!(e[0].name, "Capture region");
        assert_eq!(e[0].command, "ssx capture region");
        assert_eq!(e[1].binding, "Print");
        assert_eq!(e[1].name, "ssx capture screen");
        assert_eq!(e[2].binding, "<Alt><Super>r");
        assert!(e[2].command.starts_with("/usr/local/bin/ssx record --title "));
        assert!(e.iter().all(|e| e.path.starts_with(PATH_PREFIX) && e.path.ends_with('/')));
        assert_eq!(e[0].schema_with_path(), format!("{CUSTOM_SCHEMA}:{PATH_PREFIX}ssx-capture-region/"));
    }

    #[test]
    fn command_list_is_exact() {
        let e = entries(&fixture()[..1]).unwrap();
        let other = format!("{PATH_PREFIX}custom0/");
        let cmds: Vec<String> = commands(&e, &[other.clone()]).iter().map(|c| c.join(" ")).collect();
        let p = format!("{CUSTOM_SCHEMA}:{PATH_PREFIX}ssx-capture-region/");
        assert_eq!(
            cmds,
            [
                format!("gsettings set {p} name 'Capture region'"),
                format!("gsettings set {p} command 'ssx capture region'"),
                format!("gsettings set {p} binding '<Control><Shift>s'"),
                format!("gsettings set {SCHEMA} {LIST_KEY} ['{other}', '{PATH_PREFIX}ssx-capture-region/']"),
            ]
        );
        let sh = script(&e, &[other]);
        assert!(sh.starts_with("#!/bin/sh\n"));
        assert!(sh.contains("gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings '["));
    }

    #[test]
    fn merge_keeps_existing_order_and_dedupes() {
        let existing = vec!["/a/".to_owned(), "/b/".to_owned()];
        assert_eq!(merged_list(&existing, &["/c/".into(), "/a/".into()]), ["/a/", "/b/", "/c/"]);
        assert_eq!(merged_list(&[], &["/x/".into()]), ["/x/"]);
        assert_eq!(merged_list(&existing, &[]), existing);
    }

    #[test]
    fn gvariant_parsing_handles_gsettings_output_forms() {
        assert_eq!(parse_gvariant_string_array("@as []\n"), Some(vec![]));
        assert_eq!(parse_gvariant_string_array("[]"), Some(vec![]));
        assert_eq!(parse_gvariant_string_array("['/a/']\n"), Some(vec!["/a/".into()]));
        assert_eq!(parse_gvariant_string_array("['/a/', '/b/']"), Some(vec!["/a/".into(), "/b/".into()]));
        assert_eq!(parse_gvariant_string_array("[ \"it's\" , 'x' ]"), Some(vec!["it's".into(), "x".into()]));
        for bad in ["", "nonsense", "['unterminated", "['a' 'b']", "['a'] trailing", "['a',,]", "(1,2)"] {
            assert_eq!(parse_gvariant_string_array(bad), None, "{bad:?}");
        }
        assert_eq!(parse_gvariant_string("'a\\'b\\\\c\\n' rest"), Some(("a'b\\c\n".into(), " rest")));
        assert_eq!(parse_gvariant_string("'\\u00e9\\U0001F642'"), Some(("é🙂".into(), "")));
        assert_eq!(parse_gvariant_string("plain"), None);
    }

    #[test]
    fn gvariant_round_trips_hostile_strings() {
        for w in crate::command::tests::CORPUS {
            let text = gvariant_string(w);
            assert_eq!(parse_gvariant_string(&text), Some(((*w).to_owned(), "")), "{w:?} via {text}");
        }
    }

    #[test]
    fn apply_merges_into_an_existing_list_without_clobbering() {
        let existing = [format!("{PATH_PREFIX}custom0/"), format!("{PATH_PREFIX}custom1/")];
        let fake = FakeGsettings::with_list(&[&existing[0], &existing[1]]);
        let report = apply(&fake, &fixture()).unwrap();
        assert_eq!(report, ApplyReport { values_written: 9, list_updated: true });
        let list = fake.list();
        assert_eq!(&list[..2], &existing[..], "pre-existing entries and order untouched");
        assert_eq!(list.len(), 5);
        assert!(list[2..].iter().all(|p| p.contains("/ssx-")));
    }

    #[test]
    fn apply_is_idempotent() {
        let fake = FakeGsettings::default();
        let first = apply(&fake, &fixture()).unwrap();
        assert!(!first.is_noop());
        let list_after_first = fake.list();
        let writes_before = fake.writes();
        let second = apply(&fake, &fixture()).unwrap();
        assert!(second.is_noop(), "{second:?}");
        assert_eq!(fake.writes(), writes_before, "no writes on the second run");
        assert_eq!(fake.list(), list_after_first);
    }

    #[test]
    fn apply_updates_only_what_changed() {
        let fake = FakeGsettings::default();
        apply(&fake, &fixture()).unwrap();
        let mut changed = fixture();
        changed[0].0 = "Ctrl+Alt+S".parse().unwrap(); // new key, same command -> same path
        let r = apply(&fake, &changed).unwrap();
        assert_eq!(r, ApplyReport { values_written: 1, list_updated: false });
    }

    #[test]
    fn remove_deletes_only_owned_entries() {
        let custom = format!("{PATH_PREFIX}custom0/");
        let fake = FakeGsettings::with_list(&[&custom]);
        apply(&fake, &fixture()).unwrap();
        let r = remove(&fake).unwrap();
        assert!(r.list_updated);
        assert_eq!(r.values_written, 3);
        assert_eq!(fake.list(), vec![custom]);
        assert!(fake.store.borrow().keys().all(|k| !k.contains("ssx-")), "values reset");
        assert!(remove(&fake).unwrap().is_noop());
    }

    #[test]
    fn missing_schema_and_tool_are_reported_helpfully() {
        let fake = FakeGsettings { no_schema: true, ..Default::default() };
        let e = apply(&fake, &fixture()).unwrap_err();
        assert!(e.to_string().contains("No such schema"), "{e}");

        struct Missing;
        impl CommandRunner for Missing {
            fn run(&self, _: &str, _: &[String]) -> io::Result<RunOutput> {
                Err(io::ErrorKind::NotFound.into())
            }
        }
        let e = apply(&Missing, &fixture()).unwrap_err();
        assert!(matches!(e, BindingError::ToolMissing(..)), "{e}");
        assert!(e.to_string().contains("gsettings"));
    }

    #[test]
    fn unparsable_list_output_is_an_error_not_a_clobber() {
        let fake = FakeGsettings::default();
        fake.store.borrow_mut().insert(format!("{SCHEMA} {LIST_KEY}"), "garbage".into());
        let e = apply(&fake, &fixture()).unwrap_err();
        assert!(matches!(e, BindingError::UnexpectedOutput { .. }), "{e}");
        assert_eq!(fake.writes(), 0);
    }
}
