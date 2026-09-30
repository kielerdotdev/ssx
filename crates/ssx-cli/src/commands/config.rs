//! `ssx config`: path, show, validate, edit, set, reset.

use std::{
    io::{BufRead, IsTerminal},
    path::{Path, PathBuf},
};

use serde::Serialize;
use ssx_core::settings::{Settings, Severity};
use ssx_services::upload::config as uploader_config;
use toml_edit::DocumentMut;

use crate::{
    app::App,
    cli::ConfigCmd,
    error::{CliError, CliResult},
    output::{err_line, out_line, out_text},
    settings_edit::{self, Seg},
};

/// One finding of `config validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// `error` or `warning`.
    pub severity: &'static str,
    /// Where (`general.image_quality`, `uploaders.mine`, ...).
    pub path: String,
    /// What is wrong and how to fix it.
    pub message: String,
}

/// Everything wrong with `text`, including what only the application layer can see
/// (uploader tables, unknown keys). `base_dir` resolves relative `.sxcu` paths.
pub fn findings_for(text: &str, base_dir: &Path) -> Result<Vec<Finding>, String> {
    let loaded = Settings::from_toml_str(text).map_err(|e| e.to_string())?;
    let mut out: Vec<Finding> = loaded
        .settings
        .validate()
        .into_iter()
        .map(|i| Finding {
            severity: if i.severity == Severity::Error { "error" } else { "warning" },
            path: i.path,
            message: i.message,
        })
        .collect();
    out.extend(loaded.warnings.iter().map(|w| Finding {
        severity: "warning",
        path: "settings".to_owned(),
        message: w.clone(),
    }));
    for (name, table) in &loaded.settings.uploaders {
        if let Err(msg) = uploader_config::build(name, table, base_dir) {
            out.push(Finding {
                severity: "error",
                path: format!("uploaders.{name}"),
                message: msg.strip_prefix(&format!("[uploaders.{name}]: ")).unwrap_or(&msg).to_owned(),
            });
        }
    }
    Ok(out)
}

/// `ssx config path` output.
pub fn path_lines(app: &App) -> Vec<(&'static str, PathBuf)> {
    let p = &app.paths;
    vec![
        ("config_dir", p.config_dir.clone()),
        ("settings", p.settings_file()),
        ("uploaders", ssx_services::upload::sxcu_dir(&p.config_dir)),
        ("data_dir", p.data_dir.clone()),
        ("history", p.history_db()),
        ("counter", p.counter_file()),
        ("last_region", p.data_dir.join("last_region.json")),
    ]
}

/// Dispatches `ssx config ...`.
pub fn run(app: &App, cmd: ConfigCmd) -> CliResult<()> {
    match cmd {
        ConfigCmd::Path { all } => {
            if all {
                for (label, path) in path_lines(app) {
                    let note = if path.exists() { "" } else { "  (does not exist yet)" };
                    out_line(&format!("{label:<12} {}{note}", path.display()));
                }
            } else {
                out_line(&app.paths.settings_file().display().to_string());
            }
            Ok(())
        }
        ConfigCmd::Show { json, defaults } => show(app, json, defaults),
        ConfigCmd::Validate { file, strict, json } => validate(app, file.as_deref(), strict, json),
        ConfigCmd::Edit => edit(app),
        ConfigCmd::Set { key, value } => {
            let warnings = settings_edit::set_in_file(&app.paths.settings_file(), &key, &value)?;
            for w in warnings {
                err_line(&format!("{} {w}", app.err.yellow("warning:")));
            }
            out_line(&format!("{key} updated in {}", app.paths.settings_file().display()));
            Ok(())
        }
        ConfigCmd::Reset { key, yes } => reset(app, key.as_deref(), yes),
    }
}

fn show(app: &App, json: bool, defaults: bool) -> CliResult<()> {
    let settings = if defaults { Settings::default() } else { app.load_settings()? };
    if json {
        out_line(&serde_json::to_string_pretty(&settings)?);
    } else {
        out_text(&settings.to_toml_string()?);
    }
    Ok(())
}

fn validate(app: &App, file: Option<&Path>, strict: bool, json: bool) -> CliResult<()> {
    let path = file.map_or_else(|| app.paths.settings_file(), Path::to_path_buf);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && file.is_none() => {
            out_line(&format!("{} does not exist; the defaults are in use, which are valid", path.display()));
            return Ok(());
        }
        Err(e) => return Err(CliError::new(format!("cannot read {}: {e}", path.display()))),
    };
    let base = if file.is_some() { path.parent().unwrap_or(Path::new(".")).to_path_buf() } else { app.paths.config_dir.clone() };
    let findings = findings_for(&text, &base).map_err(|msg| {
        CliError::new(format!("{} is not valid: {msg}", path.display()))
            .hint("fix the mistake, or start over with `ssx config reset`")
    })?;
    let errors = findings.iter().filter(|f| f.severity == "error").count();
    let warnings = findings.len() - errors;
    if json {
        out_line(&serde_json::to_string_pretty(&serde_json::json!({
            "file": path,
            "valid": errors == 0 && (!strict || warnings == 0),
            "findings": findings,
        }))?);
    } else {
        for f in &findings {
            let tag = if f.severity == "error" { app.out.red("error:") } else { app.out.yellow("warning:") };
            out_line(&format!("{tag} {}: {}", f.path, f.message));
        }
        if findings.is_empty() {
            out_line(&format!("{} is valid", path.display()));
        }
    }
    if errors > 0 {
        return Err(CliError::new(format!("{} has {errors} error{}", path.display(), if errors == 1 { "" } else { "s" }))
            .hint("`ssx config set KEY VALUE` changes one value safely; `ssx config edit` opens the file"));
    }
    if strict && warnings > 0 {
        return Err(CliError::new(format!("{} has {warnings} warning{} (--strict)", path.display(), if warnings == 1 { "" } else { "s" })));
    }
    Ok(())
}

/// The editor command line from `$VISUAL` / `$EDITOR`, split on whitespace (no shell).
pub fn editor_command(visual: Option<&str>, editor: Option<&str>) -> Option<Vec<String>> {
    [visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
}

fn edit(app: &App) -> CliResult<()> {
    let path = app.paths.settings_file();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CliError::new(format!("cannot create {}: {e}", dir.display())))?;
        }
        std::fs::write(&path, Settings::default().to_toml_string()?)
            .map_err(|e| CliError::new(format!("cannot create {}: {e}", path.display())))?;
        err_line(&format!("created {} from the defaults", path.display()));
    }
    let cmdline = editor_command(std::env::var("VISUAL").ok().as_deref(), std::env::var("EDITOR").ok().as_deref());
    if let Some(mut parts) = cmdline {
        let program = parts.remove(0);
        let status = std::process::Command::new(&program)
            .args(&parts)
            .arg(&path)
            .status()
            .map_err(|e| {
                CliError::new(format!("cannot start the editor {program:?}: {e}"))
                    .hint("set $VISUAL or $EDITOR to a working editor")
            })?;
        if !status.success() {
            return Err(CliError::new(format!("the editor {program:?} exited with {status}")));
        }
    } else {
        let settings = app.load_settings().ok().unwrap_or_default();
        app.services(&settings)?.opener.open_path(&path)?;
        err_line("opened the file with the system default program; run `ssx config validate` when you are done");
        return Ok(());
    }
    validate(app, None, false, false)
}

fn reset(app: &App, key: Option<&str>, yes: bool) -> CliResult<()> {
    let path = app.paths.settings_file();
    let what = key.map_or_else(|| format!("all settings in {}", path.display()), |k| format!("{k:?}"));
    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::new(format!("refusing to reset {what} without confirmation"))
                .hint("pass --yes to confirm"));
        }
        err_line(&format!("Reset {what} to the defaults? [y/N]"));
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(CliError::cancelled());
        }
    }
    let defaults_text = Settings::default().to_toml_string()?;
    match key {
        None => {
            if path.exists() {
                let backup = backup_path(&path);
                std::fs::copy(&path, &backup)
                    .map_err(|e| CliError::new(format!("cannot back up {}: {e}", path.display())))?;
                err_line(&format!("the previous file was saved as {}", backup.display()));
            }
            ssx_core::settings::atomic_write(&path, defaults_text.as_bytes())
                .map_err(|e| CliError::new(format!("cannot write {}: {e}", path.display())))?;
            out_line(&format!("{} reset to the defaults", path.display()));
        }
        Some(key) => {
            let text = settings_edit::read_text_or_default(&path)?;
            let mut doc: DocumentMut = text.parse().map_err(|e| {
                CliError::new(format!("{} is not valid TOML: {e}", path.display())).hint("fix it with `ssx config edit`")
            })?;
            let defaults: DocumentMut = defaults_text.parse().map_err(|e| CliError::new(format!("internal error: {e}")))?;
            let segs = settings_edit::parse_key(key).map_err(CliError::new)?;
            match default_value_text(&defaults, &segs) {
                Some(v) => settings_edit::set_key(&mut doc, &defaults, key, &v).map_err(CliError::new)?,
                None => {
                    // No default value means "unset" (an optional setting).
                    settings_edit::remove_key(&mut doc, key).map_err(CliError::new)?;
                }
            }
            settings_edit::write_checked(&path, &doc.to_string())?;
            out_line(&format!("{key} reset to its default"));
        }
    }
    Ok(())
}

/// The TOML text of the default value at `segs`, if the defaults have one.
fn default_value_text(defaults: &DocumentMut, segs: &[Seg]) -> Option<String> {
    settings_edit::find_value(defaults.as_item(), segs).map(|v| v.to_string().trim().to_owned())
}

fn backup_path(path: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}.bak-{stamp}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> String {
        Settings::default().to_toml_string().unwrap()
    }

    #[test]
    fn a_default_file_has_no_findings() {
        assert_eq!(findings_for(&good(), Path::new("/x")).unwrap(), []);
    }

    #[test]
    fn findings_are_actionable_and_located() {
        let text = format!(
            "{}\n[uploaders.typo]\ntype = \"s3\"\nbukcet = \"x\"\n[capture]\ndelay_ms = 999999\n",
            good().replace("[capture]", "[capture_unused]")
        );
        let f = findings_for(&text, Path::new("/x")).unwrap();
        let by = |path: &str| f.iter().find(|x| x.path == path).unwrap_or_else(|| panic!("{path} missing in {f:#?}"));
        assert_eq!(by("uploaders.typo").severity, "error");
        assert!(by("uploaders.typo").message.contains("bukcet"), "{:?}", by("uploaders.typo"));
        assert_eq!(by("capture.delay_ms").severity, "error");
        assert!(f.iter().any(|x| x.severity == "warning" && x.message.contains("capture_unused")), "unknown keys warn: {f:#?}");
    }

    #[test]
    fn unparseable_files_are_reported_as_a_single_message() {
        let e = findings_for("this is = not [toml", Path::new("/x")).unwrap_err();
        assert!(e.contains("cannot parse"), "{e}");
        let e = findings_for("version = 999999\n", Path::new("/x")).unwrap_err();
        assert!(!e.is_empty());
    }

    #[test]
    fn sxcu_uploaders_resolve_relative_to_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!("{}\n[uploaders.mine]\ntype = \"sxcu\"\nfile = \"mine.sxcu\"\n", good());
        let f = findings_for(&text, dir.path()).unwrap();
        assert!(f.iter().any(|x| x.path == "uploaders.mine" && x.message.contains("mine.sxcu")), "{f:#?}");
        std::fs::write(
            dir.path().join("mine.sxcu"),
            r#"{"Version":"14.0.0","RequestURL":"http://127.0.0.1:9/x","Body":"MultipartFormData","FileFormName":"f","URL":"{response}"}"#,
        )
        .unwrap();
        assert_eq!(findings_for(&text, dir.path()).unwrap(), []);
    }

    #[test]
    fn editor_commands_split_without_a_shell() {
        assert_eq!(editor_command(Some("code -w"), Some("vim")).unwrap(), ["code", "-w"]);
        assert_eq!(editor_command(None, Some("  nano  ")).unwrap(), ["nano"]);
        assert_eq!(editor_command(Some(""), Some("vim")).unwrap(), ["vim"], "an empty VISUAL falls through");
        assert!(editor_command(None, None).is_none());
        assert_eq!(editor_command(Some("a;b $(x)"), None).unwrap(), ["a;b", "$(x)"], "no shell semantics");
    }

    #[test]
    fn default_values_can_be_looked_up_by_key() {
        let defaults: DocumentMut = good().parse().unwrap();
        let get = |k: &str| default_value_text(&defaults, &settings_edit::parse_key(k).unwrap());
        assert_eq!(get("general.image_quality").as_deref(), Some("90"));
        assert_eq!(get("general.image_format").as_deref(), Some("\"png\""));
        assert_eq!(get("general.save_dir"), None, "optional settings have no default value");
        assert_eq!(get("nonsense.key"), None);
    }

    #[test]
    fn backups_are_named_with_a_timestamp() {
        let p = backup_path(Path::new("/c/settings.toml"));
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("settings.toml.bak-"), "{name}");
    }
}
