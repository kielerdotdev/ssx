//! End to end: configuration, hotkeys, shell integration, diagnostics, history and the small
//! commands. None of these needs a display.
#![cfg(unix)]

mod common;

use std::path::Path;

use common::TestEnv;

fn golden(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("cannot read golden {}: {e}", p.display()))
}

/// Compares against a golden file; `SSX_UPDATE_GOLDEN=1` rewrites it.
fn assert_golden(name: &str, actual: &str) {
    if std::env::var_os("SSX_UPDATE_GOLDEN").is_some() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, actual).unwrap();
        return;
    }
    assert_eq!(
        actual,
        golden(name),
        "golden {name} differs (rerun with SSX_UPDATE_GOLDEN=1 to update)"
    );
}

// ---- config -----------------------------------------------------------------------------

#[test]
fn config_validate_accepts_good_files_and_explains_bad_ones() {
    let env = TestEnv::new();
    // No file at all: defaults, which are valid.
    let r = env.ssx(&["config", "validate"]).ok();
    assert!(r.stdout.contains("defaults are in use"), "{}", r.stdout);

    // A good file, checked explicitly.
    let good = env.path("good.toml");
    std::fs::write(&good, "version = 1\n[general]\nimage_quality = 80\n").unwrap();
    let r = env.ssx(&["config", "validate", good.to_str().unwrap()]).ok();
    assert!(r.stdout.contains("is valid"), "{}", r.stdout);

    // Semantic errors: all of them, with paths and fixes; exit code 1.
    let bad = env.path("bad.toml");
    std::fs::write(
        &bad,
        "version = 1\n[general]\nimage_quality = 0\nfile_name_pattern = \"\"\n\n[capture]\ndelay_ms = 999999\n\n\
         [uploaders.s]\ntype = \"s3\"\nbukcet = \"x\"\n\n[uploaders.t]\ntype = \"imgur\"\naccess_token = \"hunter2\"\n",
    )
    .unwrap();
    let r = env.ssx(&["config", "validate", bad.to_str().unwrap()]).code(1);
    for needle in [
        "error: general.image_quality",
        "error: general.file_name_pattern",
        "error: capture.delay_ms",
        "error: uploaders.s",
        "bukcet",
        "plain-text secret",
    ] {
        assert!(r.stdout.contains(needle), "{needle} missing from:\n{}", r.stdout);
    }
    assert!(r.stderr.contains("error:") && r.stderr.contains("hint:"), "{}", r.stderr);

    // Syntax errors are reported with the parser's location.
    let broken = env.path("broken.toml");
    std::fs::write(&broken, "this is = not [toml").unwrap();
    let r = env.ssx(&["config", "validate", broken.to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("is not valid") && r.stderr.contains("hint:"), "{}", r.stderr);

    // Warnings pass, unless --strict.
    let warn = env.path("warn.toml");
    std::fs::write(&warn, "version = 1\nmystery_key = 1\n").unwrap();
    let r = env.ssx(&["config", "validate", warn.to_str().unwrap()]).ok();
    assert!(r.stdout.contains("warning:") && r.stdout.contains("mystery_key"), "{}", r.stdout);
    env.ssx(&["config", "validate", "--strict", warn.to_str().unwrap()]).code(1);

    // JSON for scripts.
    let j = env.ssx(&["config", "validate", "--json", good.to_str().unwrap()]).ok().json();
    assert_eq!(j["valid"], true);
    let j = env.ssx(&["config", "validate", "--json", bad.to_str().unwrap()]).code(1).json();
    assert_eq!(j["valid"], false);
    assert!(j["findings"].as_array().unwrap().len() >= 5);

    // The real settings file too.
    env.write_settings("[general]\nimage_quality = 300\n");
    env.ssx(&["config", "validate"]).code(1);
    let r = env.ssx(&["monitors"]).code(1);
    assert!(
        r.stderr.contains("cannot parse settings") && r.stderr.contains("hint:"),
        "commands refuse broken settings: {}",
        r.stderr
    );
}

#[test]
fn config_set_show_path_and_reset() {
    let env = TestEnv::new();
    let path = env.ssx(&["config", "path"]).ok();
    assert_eq!(path.lines(), [env.cfg.join("settings.toml").to_str().unwrap()]);
    let all = env.ssx(&["config", "path", "--all"]).ok();
    for label in
        ["config_dir", "settings", "uploaders", "data_dir", "history", "counter", "last_region"]
    {
        assert!(all.stdout.contains(label), "{label} in {}", all.stdout);
    }

    env.ssx(&["config", "set", "general.image_quality", "70"]).ok();
    env.ssx(&["config", "set", "general.file_name_pattern", "Shot_%y-%mo-%d"]).ok();
    env.ssx(&["config", "set", "destinations.image", "local"]).ok();
    env.ssx(&["config", "set", "workflows[0].name", "My first"]).ok();
    let shown = env.ssx(&["config", "show", "--json"]).ok().json();
    assert_eq!(shown["general"]["image_quality"], 70);
    assert_eq!(shown["general"]["file_name_pattern"], "Shot_%y-%mo-%d");
    assert_eq!(shown["destinations"]["image"], "local");
    assert_eq!(shown["workflows"][0]["name"], "My first");
    let toml = env.ssx(&["config", "show"]).ok().stdout;
    assert!(toml.contains("image_quality = 70"), "{toml}");

    // Refused changes leave the file alone and explain.
    let before = std::fs::read_to_string(env.cfg.join("settings.toml")).unwrap();
    let r = env.ssx(&["config", "set", "general.image_quality", "0"]).code(1);
    assert!(
        r.stderr.contains("1 to 100") && r.stderr.contains("nothing was written"),
        "{}",
        r.stderr
    );
    let r = env.ssx(&["config", "set", "general.imag_quality", "1"]).code(1);
    assert!(r.stderr.contains("typo") && r.stderr.contains("image_quality"), "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(env.cfg.join("settings.toml")).unwrap(), before);

    // Comments survive edits.
    let text = before.replacen("[general]", "# my note\n[general]", 1);
    std::fs::write(env.cfg.join("settings.toml"), text).unwrap();
    env.ssx(&["config", "set", "general.image_quality", "60"]).ok();
    assert!(std::fs::read_to_string(env.cfg.join("settings.toml")).unwrap().contains("# my note"));

    // Single-key reset, then full reset (needs --yes when not interactive; keeps a backup).
    env.ssx(&["config", "reset", "general.image_quality", "--yes"]).ok();
    assert_eq!(env.ssx(&["config", "show", "--json"]).ok().json()["general"]["image_quality"], 90);
    env.ssx(&["config", "reset"]).code(1);
    let r = env.ssx(&["config", "reset", "--yes"]).ok();
    assert!(r.stderr.contains("settings.toml.bak-"), "{}", r.stderr);
    assert_eq!(
        env.ssx(&["config", "show", "--json"]).ok().json()["destinations"]["image"],
        serde_json::Value::Null
    );
    let defaults = env.ssx(&["config", "show", "--defaults", "--json"]).ok().json();
    assert_eq!(defaults["general"]["image_quality"], 90);
}

#[test]
fn hotkeys_print_matches_the_golden_files() {
    let env = TestEnv::new();
    let r = env.ssx(&["hotkeys", "print", "--target", "sway"]).ok();
    assert_golden("hotkeys-sway.conf", &r.stdout);
    let r = env.ssx(&["hotkeys", "print", "--target", "hyprland"]).ok();
    assert_golden("hotkeys-hyprland.conf", &r.stdout);
    let r = env.ssx(&["hotkeys", "print", "--target", "kde"]).ok();
    assert_golden("hotkeys-kde.txt", &r.stdout);
    assert!(r.stderr.is_empty(), "no warnings for the defaults: {}", r.stderr);

    // A custom binding and an absolute exe path, quoting hostile paths for the target.
    env.write_settings(
        "[[workflows]]\nid = \"mine\"\nname = \"My; workflow $(x)\"\ninput = \"capture_fullscreen\"\n\
         [workflows.trigger]\nhotkey = \"Super+Shift+S\"\ncli_name = \"mine\"\n",
    );
    let r = env.ssx(&["hotkeys", "print", "--target", "sway", "--exe", "/opt/my apps/ssx"]).ok();
    assert!(
        r.stdout.contains("bindsym Shift+Mod4+s exec \"/opt/my apps/ssx\" run mine"),
        "{}",
        r.stdout
    );
}

#[test]
fn hotkeys_print_needs_a_target_or_a_known_desktop_and_a_hotkey() {
    let env = TestEnv::new();
    let r = env.ssx(&["hotkeys", "print"]).code(1);
    assert!(r.stderr.contains("--target") && r.stderr.contains("hint:"), "{}", r.stderr);
    env.write_settings("workflows = []\n");
    let r = env.ssx(&["hotkeys", "print", "--target", "sway"]).code(1);
    assert!(r.stderr.contains("no workflow has a hotkey"), "{}", r.stderr);
}

#[test]
fn hotkeys_detect_follows_the_session() {
    let mut env = TestEnv::new();
    env.set("XDG_CURRENT_DESKTOP", "sway")
        .set("XDG_SESSION_TYPE", "wayland")
        .set("WAYLAND_DISPLAY", "wayland-1");
    let j = env.ssx(&["hotkeys", "detect", "--json"]).ok().json();
    assert_eq!(j["desktop"], "sway");
    assert_eq!(j["session"], "Wayland");
    assert_eq!(j["recommended"], "SwayConfig");
    // With a compositor detected, print needs no --target.
    let r = env.ssx(&["hotkeys", "print"]).ok();
    assert!(r.stdout.contains("bindsym"), "{}", r.stdout);
    let text = env.ssx(&["hotkeys", "detect"]).ok().stdout;
    assert!(text.contains("desktop: sway") && text.contains("1. sway bindsym"), "{text}");
}

#[test]
fn hotkeys_install_never_touches_your_config_without_apply() {
    let mut env = TestEnv::new();
    env.set("XDG_CURRENT_DESKTOP", "sway")
        .set("XDG_SESSION_TYPE", "wayland")
        .set("WAYLAND_DISPLAY", "wayland-1");
    let sway_dir = env.home.join(".config/sway");
    std::fs::create_dir_all(&sway_dir).unwrap();
    let main = sway_dir.join("config");
    let original = "set $mod Mod4\nbindsym $mod+Return exec foot\n";
    std::fs::write(&main, original).unwrap();

    let r = env.ssx(&["hotkeys", "install", "--exe", "/usr/bin/ssx"]).ok();
    assert!(r.stdout.contains("include ") && r.stdout.contains("--apply"), "{}", r.stdout);
    let inc = sway_dir.join("config.d/ssx.conf");
    assert!(std::fs::read_to_string(&inc).unwrap().contains("exec /usr/bin/ssx run region"));
    assert_eq!(std::fs::read_to_string(&main).unwrap(), original, "the user's config is untouched");

    env.ssx(&["hotkeys", "install", "--exe", "/usr/bin/ssx", "--apply"]).ok();
    let applied = std::fs::read_to_string(&main).unwrap();
    assert!(applied.starts_with(original) && applied.contains("ssx hotkeys"), "{applied}");
    env.ssx(&["hotkeys", "install", "--exe", "/usr/bin/ssx", "--apply"]).ok();
    assert_eq!(std::fs::read_to_string(&main).unwrap(), applied, "idempotent");

    env.ssx(&["hotkeys", "uninstall"]).ok();
    assert_eq!(
        std::fs::read_to_string(&main).unwrap(),
        original,
        "uninstall restores the config byte for byte"
    );
    assert!(!inc.exists());

    // GNOME/KDE change desktop settings, so without --apply they only print the plan.
    let r = env.ssx(&["hotkeys", "install", "--target", "gnome", "--exe", "/usr/bin/ssx"]).ok();
    assert!(
        r.stdout.contains("gsettings") && r.stderr.contains("nothing was changed"),
        "{}{}",
        r.stdout,
        r.stderr
    );
}

// ---- shell integration -------------------------------------------------------------------

#[test]
fn shell_install_dry_run_writes_nothing_and_says_what_it_would() {
    let env = TestEnv::new();
    let r = env.ssx(&["shell", "install", "--dry-run", "--force", "--exe", "/usr/bin/ssx"]).ok();
    assert!(
        r.stdout.contains("would install for") && r.stdout.contains("Upload with ssx"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("Nautilus") && r.stdout.contains("Dolphin"), "{}", r.stdout);
    assert!(r.stderr.contains("dry run: nothing was written"), "{}", r.stderr);
    assert_eq!(std::fs::read_dir(&env.home).unwrap().count(), 0, "the home directory is untouched");

    let s = env.ssx(&["shell", "status", "--json"]).ok().json();
    assert!(s.as_array().unwrap().iter().all(|i| i["installed"] == false));
}

#[test]
fn shell_install_and_uninstall_round_trip_in_a_sandboxed_home() {
    let env = TestEnv::new();
    let exe = env.path("bin dir/ssx");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();

    let r = env.ssx(&["shell", "install", "--force", "--exe", exe.to_str().unwrap()]).ok();
    assert!(r.stdout.contains("installed"), "{}", r.stdout);
    let dolphin = env.home.join(".local/share/kio/servicemenus/ssx-upload.desktop");
    let text = std::fs::read_to_string(&dolphin).expect("dolphin service menu");
    assert!(text.contains("post-file"), "{text}");

    let s = env.ssx(&["shell", "status", "--json"]).ok().json();
    assert!(s.as_array().unwrap().iter().any(|i| i["id"] == "dolphin"), "{s}");

    let r = env.ssx(&["shell", "uninstall", "--exe", exe.to_str().unwrap()]).ok();
    assert!(r.stdout.contains("removed") || r.stdout.contains("not installed"), "{}", r.stdout);
    assert!(!dolphin.exists());
}

// ---- doctor, completions, misc ---------------------------------------------------------

#[test]
fn doctor_json_has_the_documented_shape() {
    let env = TestEnv::new();
    // No display, no session bus: doctor must still produce a report (and flag the error).
    let r = env.ssx(&["doctor", "--json"]).code(1);
    let j = r.json();
    for key in [
        "version",
        "os",
        "session",
        "capture",
        "monitors",
        "clipboard",
        "notifications",
        "keyring",
        "paths",
        "settings",
        "uploaders",
        "editor",
        "shell_integrations",
        "hotkeys",
        "problems",
    ] {
        assert!(j.get(key).is_some(), "{key} missing: {}", r.stdout);
    }
    assert_eq!(j["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(j["capture"]["ok"], false);
    assert!(
        j["capture"]["error"].as_str().unwrap().contains("no graphical session"),
        "{}",
        j["capture"]
    );
    let problems = j["problems"].as_array().unwrap();
    assert!(problems.iter().any(|p| p["severity"] == "error" && p["message"].as_str().unwrap().contains("capture")));
    assert!(problems.iter().all(|p| p["hint"].is_string()), "every problem says what to do");
    assert_eq!(j["paths"]["config_dir"], env.cfg.to_str().unwrap());
    assert_eq!(j["keyring"]["persistent"], false);
    assert!(j["uploaders"].as_array().unwrap().iter().any(|u| u["name"] == "local"));

    let text = env.ssx(&["doctor"]).code(1).stdout;
    for s in [
        "Session",
        "Screen capture",
        "Monitors",
        "Desktop integration",
        "Hotkeys",
        "Files",
        "Problems",
    ] {
        assert!(text.contains(s), "{s} missing from\n{text}");
    }
}

#[test]
fn doctor_reports_a_broken_settings_file_instead_of_failing_to_start() {
    let env = TestEnv::new();
    env.write_settings("[general]\nimage_quality = 300\n");
    let j = env.ssx(&["doctor", "--json"]).code(1).json();
    assert!(
        j["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["message"].as_str().unwrap().contains("settings.toml")),
        "{j}"
    );
}

#[test]
fn completions_and_man_page() {
    let env = TestEnv::new();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let r = env.ssx(&["completions", shell]).ok();
        assert!(
            r.stdout.contains("ssx") && r.stdout.len() > 200,
            "{shell}: {}",
            &r.stdout[..r.stdout.len().min(200)]
        );
    }
    let bash = env.ssx(&["completions", "bash"]).ok().stdout;
    assert!(bash.contains("post-file") && bash.contains("--coalesce"));
    assert_eq!(env.ssx(&["completions", "klingon"]).code, 2);
    let man = env.ssx(&["man"]).ok().stdout;
    assert!(man.starts_with(".ie") || man.contains(".TH ssx"), "roff output");
    assert!(man.contains("post\\-file"));
}

#[test]
fn help_documents_exit_codes_and_environment() {
    let env = TestEnv::new();
    let h = env.ssx(&["--help"]).ok().stdout;
    for s in ["EXIT CODES", "cancelled", "SSX_CONFIG_DIR", "RUST_LOG", "NO_COLOR"] {
        assert!(h.contains(s), "{s} missing from --help");
    }
    let h = env.ssx(&["capture", "--help"]).ok().stdout;
    for s in [
        "fullscreen",
        "--rect",
        "--upload",
        "--copy",
        "--edit",
        "--delay",
        "--cursor",
        "--format",
        "--json",
    ] {
        assert!(h.contains(s), "{s} missing from `capture --help`");
    }
}

#[test]
fn errors_use_the_documented_format_and_colour_follows_the_environment() {
    let env = TestEnv::new();
    let r = env.ssx(&["history", "show", "999"]).code(1);
    assert!(r.stderr.starts_with("error: there is no history entry 999 (hint: "), "{}", r.stderr);
    assert!(!r.stderr.contains('\x1b'), "NO_COLOR is respected");

    let mut env = TestEnv::new();
    env.set("NO_COLOR", "");
    let r = env.ssx(&["--color", "always", "history", "show", "999"]).code(1);
    assert!(r.stderr.contains("\x1b["), "--color always forces colour: {:?}", r.stderr);
    let r = env.ssx(&["--color", "never", "history", "show", "999"]).code(1);
    assert!(!r.stderr.contains('\x1b'));
}

// ---- uploaders, history ------------------------------------------------------------------

#[test]
fn uploaders_list_import_remove_and_secrets() {
    let env = TestEnv::new();
    let list = env.ssx(&["uploaders", "list"]).ok().stdout;
    for name in ["local", "is.gd", "v.gd", "tinyurl"] {
        assert!(list.contains(name), "{name} in\n{list}");
    }

    let sxcu = env.path("x.sxcu");
    std::fs::write(&sxcu, common::sxcu_json("http://127.0.0.1:9")).unwrap();
    env.ssx(&["uploaders", "import", sxcu.to_str().unwrap()]).ok();
    assert!(env.cfg.join("uploaders/mock-host.sxcu").exists(), "named after the file's Name field");
    let r = env.ssx(&["uploaders", "import", sxcu.to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("already exists") && r.stderr.contains("--force"), "{}", r.stderr);
    env.ssx(&["uploaders", "import", sxcu.to_str().unwrap(), "--force"]).ok();
    std::fs::write(&sxcu, "{not json").unwrap();
    let r = env.ssx(&["uploaders", "import", sxcu.to_str().unwrap(), "--name", "bad"]).code(1);
    assert!(r.stderr.contains("error:"), "{}", r.stderr);
    assert!(!env.cfg.join("uploaders/bad.sxcu").exists());

    // A settings table and an imported file can both be removed by name.
    env.ssx(&["config", "set", "uploaders.\"my-local\".type", "local"]).ok();
    assert!(env.ssx(&["uploaders", "list"]).ok().stdout.contains("my-local"));
    env.ssx(&["uploaders", "remove", "my-local"]).ok();
    env.ssx(&["uploaders", "remove", "mock-host"]).ok();
    let r = env.ssx(&["uploaders", "remove", "mock-host"]).code(1);
    assert!(r.stderr.contains("no imported or configured uploader"), "{}", r.stderr);
    let list = env.ssx(&["uploaders", "list"]).ok().stdout;
    assert!(!list.contains("my-local") && !list.contains("mock-host"), "{list}");

    // The built-in local uploader "uploads" without any network.
    let f = env.path("a.png");
    std::fs::write(&f, "x").unwrap();
    let t = env.ssx(&["uploaders", "test", "local"]).ok();
    assert!(t.stdout.trim().starts_with("local://"), "{}", t.stdout);
    let u = env.ssx(&["upload", "--to", "local", f.to_str().unwrap()]).ok();
    assert!(u.stdout.trim().starts_with("file://"), "{}", u.stdout);

    // Secrets: never on the command line, no keyring here, env variables work.
    let r = env.ssx(&["uploaders", "secret", "status", "my-token"]).ok();
    assert!(
        r.stdout.contains("secret store: none") && r.stdout.contains("my-token: not set"),
        "{}",
        r.stdout
    );
    let r = env.ssx_with_stdin(&["uploaders", "secret", "set", "my-token"], "hunter2\n").code(1);
    assert!(
        r.stderr.contains("NOT saved") && r.stderr.contains("SSX_SECRET_MY_TOKEN"),
        "{}",
        r.stderr
    );
    let mut env2 = TestEnv::new();
    env2.set("SSX_SECRET_MY_TOKEN", "from-env");
    assert!(
        env2.ssx(&["uploaders", "secret", "status", "my-token"])
            .ok()
            .stdout
            .contains("my-token: set (from environment)")
    );
    assert_eq!(env.ssx(&["uploaders", "secret", "set", "bad name"]).code, 2);
}

#[test]
fn history_commands_work_on_an_empty_and_a_populated_history() {
    let env = TestEnv::new();
    let r = env.ssx(&["history", "list"]).ok();
    assert!(
        r.stdout.is_empty() && r.stderr.contains("nothing in the history"),
        "{}{}",
        r.stdout,
        r.stderr
    );
    assert_eq!(env.ssx(&["history", "list", "--json"]).ok().json(), serde_json::json!([]));

    // Populate through the real upload path with the built-in local uploader.
    env.write_settings("[destinations]\nfile = \"local\"\nimage = \"local\"\n");
    let a = env.path("a.png");
    let b = env.path("b.txt");
    std::fs::write(&a, "png").unwrap();
    std::fs::write(&b, "text").unwrap();
    env.ssx(&["upload", a.to_str().unwrap(), b.to_str().unwrap()]).ok();
    let list = env.ssx(&["history", "list", "--json"]).ok().json();
    let entries = list.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries[0]["created"].as_str().unwrap().ends_with('Z'));
    let kinds: Vec<_> = entries.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"image") && kinds.contains(&"file"), "{kinds:?}");

    assert_eq!(
        env.ssx(&["history", "list", "--kind", "image", "--json"])
            .ok()
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        env.ssx(&["history", "list", "-n", "1", "--json"]).ok().json().as_array().unwrap().len(),
        1
    );
    assert_eq!(
        env.ssx(&["history", "list", "--uploaded", "--json"]).ok().json().as_array().unwrap().len(),
        2
    );
    assert_eq!(
        env.ssx(&["history", "search", "b.txt", "--json"]).ok().json().as_array().unwrap().len(),
        1
    );
    assert_eq!(
        env.ssx(&["history", "search", "nothing-matches-this", "--json"])
            .ok()
            .json()
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let id = entries[0]["id"].as_i64().unwrap().to_string();
    let shown = env.ssx(&["history", "show", &id, "--json"]).ok().json();
    assert_eq!(shown["id"].as_i64().unwrap().to_string(), id);
    let r = env.ssx(&["history", "delete", &id, "9999"]).code(1);
    assert!(
        r.stdout.contains("deleted 1 entry") && r.stderr.contains("9999"),
        "{}{}",
        r.stdout,
        r.stderr
    );
    assert_eq!(env.ssx(&["history", "list", "--json"]).ok().json().as_array().unwrap().len(), 1);

    let r = env.ssx(&["history", "prune", "--max-entries", "0", "--max-age-days", "0"]).ok();
    assert!(r.stdout.contains("removed 0"), "{}", r.stdout);
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    let r = env.ssx(&["history", "prune", "--orphans"]).ok();
    assert!(r.stdout.contains("whose file is gone"), "{}", r.stdout);
    assert_eq!(
        env.ssx(&["history", "list", "--json"]).ok().json().as_array().unwrap().len(),
        0,
        "entries of deleted files were pruned"
    );
}

#[test]
fn run_explains_workflows_that_cannot_run() {
    let env = TestEnv::new();
    let r = env.ssx(&["run", "nope"]).code(1);
    assert!(
        r.stderr.contains("no workflow called") && r.stderr.contains("region (capture-region)"),
        "{}",
        r.stderr
    );
    let r = env.ssx(&["run", "upload"]).code(2);
    assert!(r.stderr.contains("takes files") && r.stderr.contains("post-file"), "{}", r.stderr);
    let r = env.ssx(&["run", "record"]).code(1);
    assert!(r.stderr.contains("not available yet"), "{}", r.stderr);
    // Interactive region without an overlay: a clear message, not a crash (no display here, so
    // the selector check comes first).
    let r = env.ssx(&["run", "region"]).code(1);
    assert!(r.stderr.contains("--rect") || r.stderr.contains("capture"), "{}", r.stderr);
}

#[test]
fn edit_without_the_helper_explains_where_to_get_it() {
    let env = TestEnv::new();
    let img = env.path("i.png");
    let frame = ssx_types::Frame::from_rgba8(2, 2, [1, 2, 3, 255].repeat(4)).unwrap();
    frame.save(&img).unwrap();
    let r = env.ssx(&["edit", "--", img.to_str().unwrap()]).code(1);
    assert!(
        r.stderr.contains("ssx-editor-ui") && r.stderr.contains("SSX_EDITOR_UI"),
        "{}",
        r.stderr
    );
    let r = env.ssx(&["edit", env.path("missing.png").to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("cannot read"), "{}", r.stderr);
    let text = env.path("t.png");
    std::fs::write(&text, "not an image").unwrap();
    let r = env.ssx(&["edit", text.to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("not an image") && r.stderr.contains("hint:"), "{}", r.stderr);
}

/// A stand-in `ssx-editor-ui`: "edits" by writing a solid colour to `--output`.
fn fake_editor(dir: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-editor.sh");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn edit_round_trips_through_the_helper_protocol() {
    let mut env = TestEnv::new();
    let img = env.path("i.png");
    ssx_types::Frame::from_rgba8(4, 4, [10, 20, 30, 255].repeat(16)).unwrap().save(&img).unwrap();
    // The helper copies its input to the output ("accepted, unchanged").
    let ok = fake_editor(env.dir.path(), "cp \"$3\" \"$2\"");
    env.set("SSX_EDITOR_UI", ok.display().to_string());
    let r = env.ssx(&["edit", "--", img.to_str().unwrap()]).ok();
    let edited = env.path("i-edited.png");
    assert_eq!(r.lines(), [edited.to_str().unwrap()]);
    assert_eq!(common::read_image(&edited).width(), 4);
    assert!(img.exists(), "the original is kept unless --in-place");
    env.ssx(&["edit", "--in-place", "--", img.to_str().unwrap()]).ok();

    // Closing the editor without accepting is a cancellation: exit code 3, nothing written.
    let cancel = fake_editor(env.dir.path(), "exit 3");
    env.set("SSX_EDITOR_UI", cancel.display().to_string());
    let out = env.path("nope.png");
    env.ssx(&["edit", "-o", out.to_str().unwrap(), "--", img.to_str().unwrap()]).code(3);
    assert!(!out.exists());
}
