//! Copy-paste configuration for terminal file managers. **Text only: nothing is installed.**
//!
//! yazi, ranger and lf have no menu to extend, only key bindings in files the user owns, so
//! we print snippets and let them paste them. Each snippet passes the selection as argv
//! (`"$@"`, ranger's `%s` which ranger shell-quotes itself, lf's `$fx` guarded with `set -f`
//! and newline-only `IFS`) after a `--`.

use crate::action::Action;
use crate::quote::sh_single_quote;

fn key_for(action: &Action) -> &'static str {
    match action.id.as_str() {
        "upload" => "U",
        "edit" => "E",
        "upload-video" => "V",
        _ => "",
    }
}

fn args_for(action: &Action) -> String {
    action.exec_args.iter().map(|a| sh_single_quote(a)).collect::<Vec<_>>().join(" ")
}

/// A snippet for yazi's `keymap.toml`.
///
/// yazi ≥ 25.5 calls the table `[mgr]` (older: `[manager]`); `$@` expands to the selected
/// files, or the hovered one when nothing is selected.
pub fn yazi(ssx_exe: &str, actions: &[Action]) -> String {
    let exe = sh_single_quote(ssx_exe);
    let mut s = String::from(
        "# Add to ~/.config/yazi/keymap.toml (use [[manager.prepend_keymap]] on yazi < 25.5)\n",
    );
    for a in actions {
        let key = key_for(a);
        if key.is_empty() {
            continue;
        }
        let run = format!("shell -- {exe} {} -- \"$@\"", args_for(a));
        // Single-quoted TOML literal strings cannot contain `'`; use a basic string then.
        s.push_str(&format!(
            "\n[[mgr.prepend_keymap]]\non   = [ \"<C-s>\", \"{key}\" ]\nrun  = {}\ndesc = {}\n",
            toml_basic(&run),
            toml_basic(&a.label),
        ));
    }
    s
}

/// A snippet for ranger's `rc.conf`. `%s` is the marked files, quoted by ranger.
pub fn ranger(ssx_exe: &str, actions: &[Action]) -> String {
    let exe = sh_single_quote(ssx_exe);
    let mut s = String::from("# Add to ~/.config/ranger/rc.conf\n");
    for a in actions {
        let key = key_for(a);
        if key.is_empty() {
            continue;
        }
        s.push_str(&format!(
            "map <C-s>{} shell -f {exe} {} -- %s   # {}\n",
            key.to_ascii_lowercase(),
            args_for(a),
            a.label
        ));
    }
    s
}

/// A snippet for lf's `lfrc`. `$fx` is the newline-separated selection; with `IFS` set to
/// newline and globbing off, `$fx` splits exactly at file boundaries.
pub fn lf(ssx_exe: &str, actions: &[Action]) -> String {
    let exe = sh_single_quote(ssx_exe);
    let mut s = String::from("# Add to ~/.config/lf/lfrc\n");
    for a in actions {
        let key = key_for(a);
        if key.is_empty() {
            continue;
        }
        let cmd = format!("ssx-{}", a.id);
        s.push_str(&format!(
            "cmd {cmd} %{{{{\n    set -f\n    IFS=\"$(printf '\\n\\t')\"\n    {exe} {} -- $fx\n}}}}\nmap <c-s>{} {cmd}   # {}\n",
            args_for(a),
            key.to_ascii_lowercase(),
            a.label,
        ));
    }
    s
}

/// All three snippets, each headed by its file manager name.
pub fn all(ssx_exe: &str, actions: &[Action]) -> String {
    format!(
        "== yazi ==\n{}\n== ranger ==\n{}\n== lf ==\n{}",
        yazi(ssx_exe, actions),
        ranger(ssx_exe, actions),
        lf(ssx_exe, actions)
    )
}

fn toml_basic(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
