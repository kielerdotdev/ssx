//! Property-style tests: whatever the ssx executable path looks like, every generated
//! artefact must decode (with a reference parser for that format) back to exactly that path
//! followed by the fixed arguments and `--`, and never to anything shell-interpretable.

use std::path::Path;

use crate::action::Action;
use crate::context::Context;
use crate::quote::reference::{
    hostile_strings, keyfile_unescape, parse_desktop_exec, parse_py_str, parse_shell_words,
};

/// Absolute path made from a hostile string, without control characters (those are rejected
/// up front by `Context::validate`, which is tested separately).
fn exes() -> Vec<String> {
    hostile_strings(300)
        .into_iter()
        .map(|s| format!("/{}", s.chars().filter(|c| !c.is_control()).collect::<String>()))
        .collect()
}

fn ctx(exe: &str) -> Context {
    Context::sandboxed(Path::new("/tmp/sandbox"), exe)
}

fn line_value<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?} in:\n{text}"))
}

#[test]
fn hostile_exe_paths_survive_desktop_entry_exec() {
    for exe in exes() {
        let c = ctx(&exe);
        for a in Action::defaults() {
            let code = if a.multi_select { "%F" } else { "%f" };
            let want: Vec<String> = std::iter::once(exe.clone())
                .chain(a.exec_args.iter().cloned())
                .chain(["--".to_owned(), code.to_owned()])
                .collect();
            let dolphin = crate::linux::dolphin::service_menu_source(&c, &a).expect("dolphin");
            assert_eq!(parse_desktop_exec(line_value(&dolphin, "Exec=")), want, "dolphin {exe:?}");
            let desktop =
                crate::linux::desktop_entry::DesktopEntries::new().source(&c, &a).expect("desktop");
            assert_eq!(parse_desktop_exec(line_value(&desktop, "Exec=")), want, "desktop {exe:?}");
        }
    }
}

#[test]
fn hostile_exe_paths_survive_nemo_exec() {
    for exe in exes() {
        let c = ctx(&exe);
        for a in Action::defaults() {
            let text = crate::linux::nemo::action_source(&c, &a).expect("nemo");
            let exec = keyfile_unescape(line_value(&text, "Exec="));
            let words: Vec<String> =
                parse_shell_words(&exec).into_iter().map(|w| w.replace("%%", "%")).collect();
            let mut want = vec![exe.clone()];
            want.extend(a.exec_args.iter().cloned());
            want.extend(["--".to_owned(), "%F".to_owned()]);
            assert_eq!(words, want, "nemo {exe:?}");
        }
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

#[test]
fn hostile_exe_paths_survive_thunar_command() {
    for exe in exes() {
        let c = ctx(&exe);
        for a in Action::defaults() {
            let block = crate::linux::thunar::action_block(&c, &a).expect("thunar");
            let flat = block.replace('\t', "");
            let raw = line_value(&flat, "<command>");
            let raw = raw.strip_suffix("</command>").expect("closing tag");
            let words: Vec<String> = parse_shell_words(&xml_unescape(raw))
                .into_iter()
                .map(|w| w.replace("%%", "%"))
                .collect();
            let code = if a.multi_select { "%F" } else { "%f" };
            let mut want = vec![exe.clone()];
            want.extend(a.exec_args.iter().cloned());
            want.extend(["--".to_owned(), code.to_owned()]);
            assert_eq!(words, want, "thunar {exe:?}");
            // The block itself must be well-formed XML.
            let doc = format!("<actions>{block}</actions>");
            crate::linux::thunar::scan(&doc, Path::new("x")).expect("well-formed");
        }
    }
}

#[test]
fn hostile_exe_paths_survive_nautilus_script_and_extension() {
    for exe in exes() {
        let c = ctx(&exe);
        for a in Action::defaults() {
            let script = crate::linux::nautilus::script_source(&c, &a).expect("script");
            let words = parse_shell_words(line_value(&script, "SSX="));
            assert_eq!(words, [exe.clone()], "nautilus script {exe:?}");
            assert!(script.contains(" -- \""), "the -- terminator must precede the file arguments");
        }
        let ext = crate::linux::nautilus::extension_source(&c).expect("extension");
        let lit = line_value(&ext, "SSX = ");
        assert_eq!(parse_py_str(lit), exe, "python literal {exe:?}");
    }
}

#[test]
fn hostile_exe_paths_survive_macos_script() {
    for exe in exes() {
        let c = ctx(&exe);
        for a in Action::defaults() {
            let script = crate::macos::shell_script(&c, &a).expect("script");
            let first = script.lines().find(|l| l.contains(" -- ")).expect("command line");
            let words = parse_shell_words(first.trim_start_matches("exec ").trim());
            assert_eq!(words[0], exe, "macos {exe:?}");
            assert!(words.contains(&"--".to_owned()));
        }
    }
}

#[test]
fn control_characters_in_the_exe_path_are_rejected_before_any_generation() {
    for bad in ["/usr/bin/ss\nx", "/usr/bin/ss\rx", "/usr/bin/ss\0x", "/usr/bin/ss\tx"] {
        assert!(ctx(bad).validate().is_err(), "{bad:?}");
    }
}
