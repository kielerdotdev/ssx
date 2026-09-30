//! Cheap conflict detection: does the user's existing config already bind a chord?
//!
//! Parses `bindsym` (sway) and `bind*` (Hyprland) lines, resolving the `set $var value` /
//! `$var = value` variables people use for their mod key, and reports lines whose chord
//! equals one of ours. It looks only at the text it is given (normally the main config;
//! files pulled in with `include`/`source` are not followed), and understands only what
//! [`Chord`] can express, so a `Mod2`/`Mod3`/`Mod5` chord or a `bindcode` line is skipped
//! rather than misreported.

use std::collections::HashMap;

use crate::{
    chord::{Chord, Modifiers},
    key::Key,
};

use super::{Dirs, Result, Target, files};

/// An existing binding that uses one of our chords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The contested chord.
    pub chord: Chord,
    /// 1-based line number in the scanned text.
    pub line_number: usize,
    /// The offending line, trimmed.
    pub line: String,
}

/// Every chord bound in a sway config.
pub fn sway_bindings(config: &str) -> Vec<(usize, Chord)> {
    let mut vars: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for (i, raw) in config.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        match words.next() {
            Some("set") => {
                if let (Some(name), Some(value)) = (words.next(), words.next()) {
                    if name.starts_with('$') {
                        vars.insert(name.to_owned(), value.to_owned());
                    }
                }
            }
            Some("bindsym") => {
                // Skip flags (--release, --locked, --to-code, ...).
                let Some(spec) = words.find(|w| !w.starts_with("--")) else { continue };
                if let Some(chord) = parse_sway_chord(spec, &vars) {
                    out.push((i + 1, chord));
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_sway_chord(spec: &str, vars: &HashMap<String, String>) -> Option<Chord> {
    let mut mods = Modifiers::NONE;
    let mut parts: Vec<&str> = spec.split('+').collect();
    let key = parts.pop()?;
    for part in parts {
        let resolved = vars.get(part).map_or(part, String::as_str);
        // A variable may itself hold several modifiers ("Mod4+Shift").
        for atom in resolved.split('+') {
            mods = mods | sway_modifier(atom)?;
        }
    }
    let key = Key::from_xkb(vars.get(key).map_or(key, String::as_str))?;
    Chord::new(mods, key).ok()
}

fn sway_modifier(name: &str) -> Option<Modifiers> {
    match name.to_ascii_lowercase().as_str() {
        "shift" => Some(Modifiers::SHIFT),
        "control" | "ctrl" => Some(Modifiers::CTRL),
        "alt" | "mod1" => Some(Modifiers::ALT),
        "super" | "mod4" => Some(Modifiers::SUPER),
        _ => None,
    }
}

/// Every chord bound in a Hyprland config.
pub fn hyprland_bindings(config: &str) -> Vec<(usize, Chord)> {
    let mut vars: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for (i, raw) in config.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let Some((lhs, rhs)) = line.split_once('=') else { continue };
        let lhs = lhs.trim();
        if lhs.starts_with('$') {
            vars.insert(lhs.to_owned(), rhs.trim().to_owned());
            continue;
        }
        // bind, bindl, binde, bindr, bindm, bindel, ... (flags are letters after "bind").
        if !lhs.starts_with("bind") || lhs.len() > 8 || lhs.contains(char::is_whitespace) {
            continue;
        }
        let mut fields = rhs.splitn(3, ',');
        let (Some(mods), Some(key)) = (fields.next(), fields.next()) else { continue };
        if let Some(chord) = parse_hyprland_chord(mods.trim(), key.trim(), &vars) {
            out.push((i + 1, chord));
        }
    }
    out
}

fn parse_hyprland_chord(mods: &str, key: &str, vars: &HashMap<String, String>) -> Option<Chord> {
    let mut m = Modifiers::NONE;
    let resolved: String = mods
        .split_whitespace()
        .map(|w| vars.get(w).map_or(w, String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    for atom in resolved.split(|c: char| c.is_whitespace() || c == '_' || c == '+') {
        if atom.is_empty() {
            continue;
        }
        m = m | match atom.to_ascii_uppercase().as_str() {
            "SHIFT" => Modifiers::SHIFT,
            "CTRL" | "CONTROL" => Modifiers::CTRL,
            "ALT" | "MOD1" => Modifiers::ALT,
            "SUPER" | "WIN" | "LOGO" | "MOD4" => Modifiers::SUPER,
            _ => return None, // CAPS, MOD2, MOD3, MOD5 or unknown: not expressible
        };
    }
    let key = Key::from_xkb(vars.get(key).map_or(key, String::as_str))?;
    Chord::new(m, key).ok()
}

/// Finds lines in `config` that bind any of `chords` (sway or Hyprland syntax).
///
/// # Panics
/// Never; `Target::Gnome`/`Target::Kde` have no scannable text and return no conflicts.
pub fn find_conflicts(target: Target, config: &str, chords: &[Chord]) -> Vec<Conflict> {
    let bound = match target {
        Target::Sway => sway_bindings(config),
        Target::Hyprland => hyprland_bindings(config),
        Target::Gnome | Target::Kde => return Vec::new(),
    };
    let lines: Vec<&str> = config.lines().collect();
    bound
        .into_iter()
        .filter(|(_, c)| chords.contains(c))
        .map(|(n, chord)| Conflict {
            chord,
            line_number: n,
            line: lines.get(n - 1).map_or_else(String::new, |l| l.trim().to_owned()),
        })
        .collect()
}

/// Reads the user's main config and checks it for conflicts with `chords`. A missing
/// config means no conflicts.
pub fn check_main_config(dirs: &Dirs, target: Target, chords: &[Chord]) -> Result<Vec<Conflict>> {
    let Some(path) = files::main_config_path(dirs, target) else { return Ok(Vec::new()) };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(find_conflicts(target, &text, chords)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(super::BindingError::Io { action: "reading", path, source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Chord {
        s.parse().unwrap()
    }

    const SWAY: &str = "\
# comment
set $mod Mod4
set $alt Mod1
set $term foot
bindsym $mod+Return exec $term
bindsym $mod+Shift+s exec grim -g \"$(slurp)\"
bindsym --release $mod+Shift+r exec foo
bindsym --locked XF86AudioMute exec pactl set-sink-mute @DEFAULT_SINK@ toggle
bindsym Print exec grim
bindsym Ctrl+$alt+Delete exec swaylock
bindsym $mod+Mod2+x exec numlock-only
bindcode 133+31 exec by-code
  bindsym Control+Alt+t exec indented
";

    #[test]
    fn parses_sway_bindings_with_variables_and_flags() {
        let b: Vec<String> = sway_bindings(SWAY).iter().map(|(_, c)| c.to_string()).collect();
        assert_eq!(
            b,
            ["Super+Enter", "Shift+Super+S", "Shift+Super+R", "VolumeMute", "Print", "Ctrl+Alt+Delete", "Ctrl+Alt+T"]
        );
    }

    #[test]
    fn reports_line_numbers_and_text() {
        let got = find_conflicts(Target::Sway, SWAY, &[c("Super+Shift+S"), c("Print"), c("Ctrl+Shift+S")]);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].line_number, got[0].chord), (6, c("Shift+Super+S")));
        assert!(got[0].line.starts_with("bindsym $mod+Shift+s exec grim"));
        assert_eq!((got[1].line_number, got[1].line.as_str()), (9, "bindsym Print exec grim"));
    }

    const HYPR: &str = "\
$mainMod = SUPER
$terminal = kitty
bind = $mainMod, Q, exec, $terminal
bind = $mainMod SHIFT, S, exec, grimblast copy area # trailing comment
bindl = , XF86AudioMute, exec, wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle
binde = CTRL_ALT, Delete, exit
bind = , Print, exec, grim
bind = SUPER, mouse:272, movewindow
bindm = $mainMod, mouse:272, movewindow
bind = $mainMod CAPS, x, exec, caps
windowrulev2 = float, class:foo
";

    #[test]
    fn parses_hyprland_bindings() {
        let b: Vec<String> = hyprland_bindings(HYPR).iter().map(|(_, c)| c.to_string()).collect();
        assert_eq!(b, ["Super+Q", "Shift+Super+S", "VolumeMute", "Ctrl+Alt+Delete", "Print"]);
        let got = find_conflicts(Target::Hyprland, HYPR, &[c("Super+Shift+S"), c("Super+Q")]);
        assert_eq!(got.iter().map(|g| g.line_number).collect::<Vec<_>>(), [3, 4]);
    }

    #[test]
    fn no_false_positives_or_panics_on_garbage() {
        for garbage in ["", "bindsym", "bindsym +", "bindsym ++", "bind =", "bind = ,", "bind = , ,", "set $x", "\u{0}\u{1}", "bind==,,,"] {
            assert!(sway_bindings(garbage).is_empty(), "{garbage:?}");
            assert!(hyprland_bindings(garbage).is_empty(), "{garbage:?}");
        }
        assert!(find_conflicts(Target::Gnome, SWAY, &[c("Print")]).is_empty());
    }

    #[test]
    fn main_config_check_reads_from_the_injected_root() {
        let root = std::env::temp_dir().join(format!("ssx-hotkeys-conflict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dirs = Dirs::under(&root);
        assert!(check_main_config(&dirs, Target::Sway, &[c("Print")]).unwrap().is_empty(), "missing config");
        let p = files::main_config_path(&dirs, Target::Sway).unwrap();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, SWAY).unwrap();
        assert_eq!(check_main_config(&dirs, Target::Sway, &[c("Print")]).unwrap().len(), 1);
    }
}
