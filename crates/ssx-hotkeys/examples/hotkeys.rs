//! Try the hotkey machinery from a terminal.
//!
//! ```text
//! cargo run -p ssx-hotkeys --example hotkeys -- <COMMAND>
//!
//!   detect                          what this session supports, best strategy first
//!   print <sway|hyprland|gnome|kde> the generated configuration for the demo bindings
//!   install <sway|hyprland> [--apply] [--reload] [--root DIR]
//!                                   write the include file; print the line to add to your
//!                                   config. --apply appends a removable marked block to it;
//!                                   --reload runs `swaymsg reload` / `hyprctl reload`
//!   uninstall <sway|hyprland> [--root DIR]
//!   gnome apply|remove              register/remove GNOME custom keybindings via gsettings
//!   kde apply|remove [--reload]     register/remove KDE command shortcuts
//!   listen [CHORD...]               grab chords in-process and print events (Ctrl+C to quit)
//! ```
//!
//! `--root DIR` redirects everything the generators write to `DIR/.config` and
//! `DIR/.local/share` instead of your real home directory.

use ssx_hotkeys::{
    Chord, Command, HotkeyId, HotkeyState, Strategy,
    bindings::{
        Dirs, SystemRunner, Target,
        conflict::check_main_config,
        files::{self, MainConfigChange},
        gnome, hyprland, kde, sway,
    },
    detect::{Environment, Platform, detect},
    open_best_manager,
};

fn demo_bindings() -> Result<Vec<(Chord, Command)>, Box<dyn std::error::Error>> {
    Ok(vec![
        (
            "Print".parse()?,
            Command::new("ssx").args(["capture", "screen"]).label("ssx: capture screen"),
        ),
        (
            "Ctrl+Shift+S".parse()?,
            Command::new("ssx").args(["capture", "region"]).label("ssx: capture region"),
        ),
        (
            "Super+Alt+R".parse()?,
            Command::new("ssx").args(["record", "--title", "it's a demo"]).label("ssx: record"),
        ),
    ])
}

fn target(name: &str) -> Result<Target, String> {
    match name {
        "sway" => Ok(Target::Sway),
        "hyprland" => Ok(Target::Hyprland),
        "gnome" => Ok(Target::Gnome),
        "kde" => Ok(Target::Kde),
        other => Err(format!("unknown target {other:?}; use sway, hyprland, gnome or kde")),
    }
}

fn dirs(root: Option<&str>) -> Result<Dirs, Box<dyn std::error::Error>> {
    match root {
        Some(r) => Ok(Dirs::under(r)),
        None => Dirs::from_env().ok_or_else(|| "HOME is not set; pass --root DIR".into()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |args: &mut Vec<String>, name: &str| {
        args.iter().position(|a| a == name).map(|i| args.remove(i)).is_some()
    };
    let apply = flag(&mut args, "--apply");
    let reload = flag(&mut args, "--reload");
    let root = args.iter().position(|a| a == "--root").map(|i| {
        args.remove(i);
        args.remove(i)
    });
    let mut it = args.into_iter();
    let command = it.next().unwrap_or_default();
    let rest: Vec<String> = it.collect();
    let bindings = demo_bindings()?;

    match command.as_str() {
        "detect" => {
            let d = detect(&Environment::from_env(), Platform::current());
            println!("desktop {:?}, session {:?}", d.desktop, d.session);
            for (i, s) in d.candidates.iter().enumerate() {
                println!(
                    "  {}. {s:?}{}",
                    i + 1,
                    if s.is_in_process() { " (in-process)" } else { "" }
                );
            }
            println!("best: {:?}", d.primary());
        }
        "print" => match target(rest.first().map_or("", String::as_str))? {
            Target::Sway => print!("{}", sway::render(&bindings)?),
            Target::Hyprland => print!("{}", hyprland::render(&bindings)?),
            Target::Gnome => {
                let entries = gnome::entries(&bindings)?;
                print!("{}", gnome::script(&entries, &[]));
            }
            Target::Kde => {
                let entries = kde::entries(&bindings)?;
                for e in &entries {
                    println!("# {}\n{}", e.desktop_id, e.desktop_file);
                }
                for c in kde::kwriteconfig_commands("kwriteconfig6", &entries) {
                    println!("{}", c.join(" "));
                }
            }
        },
        "install" | "uninstall" => {
            let t = target(rest.first().map_or("", String::as_str))?;
            let dirs = dirs(root.as_deref())?;
            if command == "uninstall" {
                files::uninstall(&dirs, t)?;
                println!("removed the ssx block and include file for {t}");
                return Ok(());
            }
            let chords: Vec<Chord> = bindings.iter().map(|b| b.0).collect();
            for c in check_main_config(&dirs, t, &chords)? {
                println!(
                    "warning: {} is already bound on line {}: {}",
                    c.chord, c.line_number, c.line
                );
            }
            let report = files::write_include_file(&dirs, t, &bindings)?;
            println!(
                "{} {}",
                if report.changed { "wrote" } else { "unchanged:" },
                report.path.display()
            );
            if apply {
                match files::install_main_include(&dirs, t)? {
                    MainConfigChange::Changed => println!("added the marked block to your config"),
                    MainConfigChange::Unchanged => println!("your config already has the block"),
                    MainConfigChange::AlreadyIncludedManually => {
                        println!("you already include the file by hand");
                    }
                }
            } else {
                println!(
                    "add this line to your {t} config (or re-run with --apply):\n\n    {}\n",
                    report.include_line
                );
            }
            if reload {
                files::reload(&SystemRunner, t)?;
                println!("reloaded");
            }
        }
        "gnome" => match rest.first().map(String::as_str) {
            Some("apply") => println!("{:?}", gnome::apply(&SystemRunner, &bindings)?),
            Some("remove") => println!("{:?}", gnome::remove(&SystemRunner)?),
            _ => return Err("usage: gnome apply|remove".into()),
        },
        "kde" => {
            let dirs = dirs(root.as_deref())?;
            match rest.first().map(String::as_str) {
                Some("apply") => println!("{:?}", kde::apply(&dirs, &SystemRunner, &bindings)?),
                Some("remove") => println!("{:?}", kde::remove(&dirs, &SystemRunner)?),
                _ => return Err("usage: kde apply|remove [--reload]".into()),
            }
            if reload {
                kde::reload(&SystemRunner)?;
            }
        }
        "listen" => {
            let chords: Vec<Chord> = if rest.is_empty() {
                vec!["Ctrl+Shift+F9".parse()?]
            } else {
                rest.iter().map(|s| s.parse()).collect::<Result<_, _>>()?
            };
            let mut mgr = match open_best_manager() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("{e}");
                    let d = detect(&Environment::from_env(), Platform::current());
                    if d.candidates.iter().any(|s| !s.is_in_process() && *s != Strategy::CliOnly) {
                        eprintln!("try: print/install with one of {:?}", d.candidates);
                    }
                    std::process::exit(1);
                }
            };
            println!(
                "using {} (release events reliable: {})",
                mgr.backend(),
                mgr.reports_release()
            );
            for (i, c) in chords.iter().enumerate() {
                mgr.register(HotkeyId::new(format!("demo-{i}"))?, *c)?;
                println!("registered {c}");
            }
            while let Ok(ev) = mgr.events().recv() {
                let what = if ev.state == HotkeyState::Pressed { "pressed" } else { "released" };
                println!("{} {what}", ev.id);
            }
        }
        _ => {
            eprintln!(
                "usage: hotkeys detect | print T | install T [--apply] [--reload] | uninstall T | gnome apply|remove | kde apply|remove | listen [CHORD...]"
            );
            std::process::exit(2);
        }
    }
    Ok(())
}
