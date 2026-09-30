//! The GNOME generator against the real `gsettings` binary.
//!
//! GNOME's schemas are not installed on CI machines, so the test compiles a private copy of
//! the two relevant schemas (same ids, key names and types as
//! `org.gnome.settings-daemon.plugins.media-keys`) into a temp dir and points `gsettings`
//! at it, with the `keyfile` backend so values persist between `gsettings` processes and no
//! dconf/D-Bus is needed. That exercises the real GVariant text parsing/printing, the
//! relocatable-schema `SCHEMA:PATH` syntax, and our escaping of hostile commands.
//!
//! Skips when `gsettings` or `glib-compile-schemas` is missing.
#![cfg(unix)]

use std::{
    io,
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
};

use ssx_hotkeys::{
    Chord, Command,
    bindings::{CommandRunner, RunOutput, gnome},
};

const SCHEMAS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<schemalist>
  <schema id="org.gnome.settings-daemon.plugins.media-keys" path="/org/gnome/settings-daemon/plugins/media-keys/">
    <key name="custom-keybindings" type="as">
      <default>[]</default>
      <summary>Custom keybindings</summary>
    </key>
  </schema>
  <schema id="org.gnome.settings-daemon.plugins.media-keys.custom-keybinding">
    <key name="name" type="s"><default>''</default><summary>Name</summary></key>
    <key name="command" type="s"><default>''</default><summary>Command</summary></key>
    <key name="binding" type="s"><default>''</default><summary>Binding</summary></key>
  </schema>
</schemalist>
"#;

/// Runs real processes with the private schema/backends in the environment.
struct Sandboxed {
    schema_dir: PathBuf,
    config_home: PathBuf,
}

impl CommandRunner for Sandboxed {
    fn run(&self, program: &str, args: &[String]) -> io::Result<RunOutput> {
        let out = Process::new(program)
            .args(args)
            .env("GSETTINGS_SCHEMA_DIR", &self.schema_dir)
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("HOME", &self.config_home)
            // GNOME sessions are UTF-8; under the C locale GLib prints non-ASCII as `?`.
            .env("LC_ALL", "C.UTF-8")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .output()?;
        Ok(RunOutput {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

fn sandbox(tag: &str) -> Option<Sandboxed> {
    let root = std::env::temp_dir().join(format!("ssx-hk-gsettings-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let schema_dir = root.join("schemas");
    let config_home = root.join("config");
    std::fs::create_dir_all(&schema_dir).ok()?;
    std::fs::create_dir_all(&config_home).ok()?;
    std::fs::write(schema_dir.join("org.gnome.test.gschema.xml"), SCHEMAS).ok()?;
    match Process::new("glib-compile-schemas").arg(&schema_dir).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!("SKIP: glib-compile-schemas failed: {}", String::from_utf8_lossy(&o.stderr));
            return None;
        }
        Err(e) => {
            eprintln!("SKIP: glib-compile-schemas not available ({e}); install libglib2.0-bin");
            return None;
        }
    }
    let s = Sandboxed { schema_dir, config_home };
    match s.run("gsettings", &["get".into(), gnome::SCHEMA.into(), gnome::LIST_KEY.into()]) {
        Ok(o) if o.success => Some(s),
        Ok(o) => {
            eprintln!("SKIP: gsettings cannot use the private schemas: {}", o.stderr);
            None
        }
        Err(e) => {
            eprintln!("SKIP: gsettings not available ({e}); install libglib2.0-bin");
            None
        }
    }
}

fn get(s: &Sandboxed, schema_path: &str, key: &str) -> String {
    let out = s.run("gsettings", &["get".into(), schema_path.into(), key.into()]).expect("run");
    assert!(out.success, "{}", out.stderr);
    out.stdout
}

fn hostile_bindings() -> Vec<(Chord, Command)> {
    let words = [
        "it's a \"test\"",
        "$HOME `id` ; | & > <",
        "back\\slash and trailing\\",
        "ünïcödé 日本語 🙂",
        "hash # and % and ~ and *",
        "",
        "line'\"'mix",
    ];
    let mut out = vec![(
        "Ctrl+Shift+S".parse().unwrap(),
        Command::new("ssx").args(["capture", "region"]).label("Capture region"),
    )];
    for (i, w) in words.iter().enumerate() {
        let chord: Chord = format!("Ctrl+Alt+F{}", i + 1).parse().unwrap();
        out.push((
            chord,
            Command::new("/opt/my app/ssx")
                .arg("--arg")
                .arg(*w)
                .label(format!("Label {i}: it's \"x\" \\ ü")),
        ));
    }
    out
}

#[test]
fn apply_writes_exactly_what_gnome_settings_reads_back() {
    let Some(s) = sandbox("apply") else { return };
    let bindings = hostile_bindings();
    let report = gnome::apply(&s, &bindings).expect("apply");
    assert!(report.list_updated);
    assert_eq!(report.values_written, bindings.len() * 3);

    let entries = gnome::entries(&bindings).expect("entries");
    for e in &entries {
        let sp = e.schema_with_path();
        for (key, want) in [("name", &e.name), ("command", &e.command), ("binding", &e.binding)] {
            let raw = get(&s, &sp, key);
            let (got, rest) =
                gnome::parse_gvariant_string(&raw).unwrap_or_else(|| panic!("unparsable {raw:?}"));
            assert_eq!(rest.trim(), "");
            assert_eq!(&got, want, "{key} of {}", e.path);
        }
    }
    let list =
        gnome::parse_gvariant_string_array(&get(&s, gnome::SCHEMA, gnome::LIST_KEY)).expect("list");
    assert_eq!(list, entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>());
}

#[test]
fn second_apply_is_a_noop_and_merging_preserves_foreign_entries() {
    let Some(s) = sandbox("merge") else { return };
    // A pre-existing entry from GNOME Settings.
    let foreign = format!("{}custom0/", gnome::PATH_PREFIX);
    let set = s
        .run(
            "gsettings",
            &["set".into(), gnome::SCHEMA.into(), gnome::LIST_KEY.into(), format!("['{foreign}']")],
        )
        .expect("run");
    assert!(set.success, "{}", set.stderr);
    let fs = format!("{}:{foreign}", gnome::CUSTOM_SCHEMA);
    for (k, v) in
        [("name", "Terminal"), ("command", "gnome-terminal"), ("binding", "<Control><Alt>t")]
    {
        assert!(
            s.run("gsettings", &["set".into(), fs.clone(), k.into(), v.into()])
                .expect("run")
                .success
        );
    }

    let bindings = hostile_bindings();
    gnome::apply(&s, &bindings).expect("first apply");
    let second = gnome::apply(&s, &bindings).expect("second apply");
    assert!(second.is_noop(), "{second:?}");

    let list =
        gnome::parse_gvariant_string_array(&get(&s, gnome::SCHEMA, gnome::LIST_KEY)).expect("list");
    assert_eq!(list[0], foreign, "foreign entry stays first");
    assert_eq!(list.len(), 1 + bindings.len());
    assert_eq!(get(&s, &fs, "command").trim(), "'gnome-terminal'", "foreign values untouched");

    // Remove: ours go, the foreign one and its values stay.
    let removed = gnome::remove(&s).expect("remove");
    assert!(removed.list_updated);
    let list =
        gnome::parse_gvariant_string_array(&get(&s, gnome::SCHEMA, gnome::LIST_KEY)).expect("list");
    assert_eq!(list, vec![foreign]);
    assert_eq!(get(&s, &fs, "binding").trim(), "'<Control><Alt>t'");
    let first = &gnome::entries(&bindings).expect("entries")[0];
    assert_eq!(
        get(&s, &first.schema_with_path(), "command").trim(),
        "''",
        "values reset to default"
    );
    assert!(gnome::remove(&s).expect("remove again").is_noop());
}

#[test]
fn changed_binding_updates_in_place() {
    let Some(s) = sandbox("update") else { return };
    let mut bindings = hostile_bindings();
    gnome::apply(&s, &bindings).expect("apply");
    bindings[0].0 = "Ctrl+Alt+Shift+S".parse().unwrap();
    let r = gnome::apply(&s, &bindings).expect("apply changed");
    assert_eq!((r.values_written, r.list_updated), (1, false));
    let e = &gnome::entries(&bindings).expect("entries")[0];
    assert_eq!(get(&s, &e.schema_with_path(), "binding").trim(), "'<Control><Alt><Shift>s'");
}

#[test]
fn the_printed_script_does_the_same_thing_when_run_by_hand() {
    let Some(s) = sandbox("script") else { return };
    let bindings = hostile_bindings();
    let entries = gnome::entries(&bindings).expect("entries");
    let script = gnome::script(&entries, &[]);
    let path: &Path = &s.config_home;
    let file = path.join("apply.sh");
    std::fs::write(&file, script).expect("write script");
    let out = s.run("sh", &[file.display().to_string()]).expect("run sh");
    assert!(out.success, "script failed: {}", out.stderr);
    // Idempotent afterwards via the library.
    assert!(gnome::apply(&s, &bindings).expect("apply").is_noop());
}
