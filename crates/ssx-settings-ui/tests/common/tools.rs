//! Finding the external programs the process-level tests need (Xvfb, sway, ImageMagick, a
//! Vulkan software driver) and building the folder the real binary is pointed at.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use ssx_settings_ui::nav::Page;

/// Whether `tool` is on the `PATH`.
pub fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool} >/dev/null 2>&1"))
        .status()
        .is_ok_and(|s| s.success())
}

/// The first of `tools` that is missing, as a sentence, or `None` when all are there and a
/// Vulkan software driver (lavapipe) exists.
pub fn skip_reason(tools: &[&str]) -> Option<String> {
    for t in tools {
        if !have(t) {
            return Some(format!("{t} is not installed"));
        }
    }
    if !icd_file().exists() {
        return Some("no lavapipe Vulkan driver (lvp_icd.json)".into());
    }
    None
}

/// The lavapipe driver description.
pub fn icd_file() -> PathBuf {
    for d in ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"] {
        for n in ["lvp_icd.json", "lvp_icd.x86_64.json"] {
            let p = Path::new(d).join(n);
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("/usr/share/vulkan/icd.d/lvp_icd.json")
}

/// A settings folder for the real binary: lived-in settings, a history database with the demo
/// entries and their files. Returns the config folder.
pub fn demo_config(root: &Path) -> PathBuf {
    let cfg = root.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("settings.toml"),
        super::demo::lived_in_settings().to_toml_string().unwrap(),
    )
    .unwrap();
    super::demo::demo_history_file(&root.join("shots"), &cfg.join("data").join("history.sqlite3"));
    cfg
}

/// A home directory for the process, so that nothing outside the test folder is read or
/// written (autostart, file-manager entries, compositor configs).
pub fn private_home(root: &Path) -> PathBuf {
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".config")).unwrap();
    std::fs::create_dir_all(home.join(".local/share")).unwrap();
    home
}

/// The environment variables that point a process at [`private_home`] and the software
/// Vulkan driver.
pub fn process_env(home: &Path) -> Vec<(&'static str, std::ffi::OsString)> {
    vec![
        ("HOME", home.into()),
        ("XDG_CONFIG_HOME", home.join(".config").into()),
        ("XDG_DATA_HOME", home.join(".local/share").into()),
        ("WGPU_BACKEND", "vulkan".into()),
        ("VK_ICD_FILENAMES", icd_file().into()),
    ]
}

/// The pages in navigation order with their `Ctrl+<n>` shortcut digit.
pub fn pages_with_digits() -> Vec<(Page, usize)> {
    Page::ALL.iter().enumerate().map(|(i, p)| (*p, i + 1)).collect()
}
