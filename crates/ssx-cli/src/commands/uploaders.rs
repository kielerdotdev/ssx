//! `ssx uploaders`: list, import, remove and test destinations; manage their secrets.

use std::{
    io::{IsTerminal, Read},
    path::Path,
};

use serde::Serialize;
use ssx_services::{
    LazySecrets, UploaderInfo,
    upload::{ImportError, import_sxcu, remove_sxcu},
};
use ssx_upload::SecretStore as _;
use toml_edit::DocumentMut;

use crate::{
    app::App,
    cli::{ListArgs, SecretCmd, UploadersCmd},
    error::{CliError, CliResult},
    output::{Style, Table, err_line, out_line, out_text},
    settings_edit,
};

/// The listing table.
pub fn render_list(list: &[UploaderInfo], style: Style) -> String {
    let mut t = Table::new(["NAME", "KIND", "ACCEPTS", "SOURCE", "STATUS"]);
    for i in list {
        let mut accepts = i.uploads.join(",");
        if i.shortens {
            if !accepts.is_empty() {
                accepts.push(',');
            }
            accepts.push_str("urls");
        }
        let status = match &i.error {
            None => style.green("ok"),
            Some(e) => style.red(&format!("broken: {e}")),
        };
        t.row([i.name.clone(), i.kind.clone(), accepts, i.origin.clone(), status]);
    }
    t.render(style)
}

#[derive(Serialize)]
struct ListItem<'a> {
    name: &'a str,
    kind: &'a str,
    origin: &'a str,
    uploads: &'a [&'static str],
    shortens: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

/// Dispatches `ssx uploaders ...`.
pub fn run(app: &App, cmd: UploadersCmd) -> CliResult<()> {
    match cmd {
        UploadersCmd::List(args) => list(app, &args),
        UploadersCmd::Import { file, name, force } => import(app, &file, name.as_deref(), force),
        UploadersCmd::Remove { name } => remove(app, &name),
        UploadersCmd::Test { name, json } => test(app, &name, json),
        UploadersCmd::Secret { cmd } => secret(app, cmd),
    }
}

fn list(app: &App, args: &ListArgs) -> CliResult<()> {
    let settings = app.load_settings()?;
    let services = app.services(&settings)?;
    let list = services.uploads.list();
    if args.json {
        let items: Vec<ListItem<'_>> = list
            .iter()
            .map(|i| ListItem {
                name: &i.name,
                kind: &i.kind,
                origin: &i.origin,
                uploads: &i.uploads,
                shortens: i.shortens,
                error: i.error.as_deref(),
            })
            .collect();
        out_line(&serde_json::to_string_pretty(&items)?);
    } else {
        out_text(&render_list(&list, app.out));
    }
    Ok(())
}

fn import(app: &App, file: &Path, name: Option<&str>, force: bool) -> CliResult<()> {
    let settings = app.load_settings()?;
    let imported = import_sxcu(&app.paths.config_dir, file, name, force).map_err(|e| {
        let hint = match &e {
            ImportError::Exists(_) => Some("pass --force to replace it, or --name to import it under another name"),
            ImportError::Invalid(_) => Some("ShareX 12.3.1 and older files are not supported; export the uploader again from a current ShareX"),
            _ => None,
        };
        let err = CliError::new(e.to_string());
        match hint {
            Some(h) => err.hint(h),
            None => err,
        }
    })?;
    for w in &imported.warnings {
        err_line(&format!("{} {w}", app.err.yellow("warning:")));
    }
    if settings.uploaders.contains_key(&imported.name) {
        err_line(&format!(
            "{} [uploaders.{}] in settings.toml takes precedence over the imported file",
            app.err.yellow("warning:"),
            imported.name
        ));
    }
    out_line(&format!(
        "imported {:?} as {} ({})",
        imported.display_name,
        app.out.bold(&imported.name),
        imported.path.display()
    ));
    err_line(&format!(
        "use it with `ssx upload --to {n} FILE`, or make it the default with `ssx config set destinations.image {n}`",
        n = imported.name
    ));
    Ok(())
}

fn remove(app: &App, name: &str) -> CliResult<()> {
    let removed_file = remove_sxcu(&app.paths.config_dir, name)
        .map_err(|e| CliError::new(format!("cannot remove the imported file: {e}")))?;
    let settings_path = app.paths.settings_file();
    let mut removed_table = false;
    if let Ok(text) = std::fs::read_to_string(&settings_path) {
        let mut doc: DocumentMut = text.parse().map_err(|e| {
            CliError::new(format!("{} is not valid TOML: {e}", settings_path.display()))
        })?;
        let key = format!("uploaders.\"{name}\"");
        if settings_edit::remove_key(&mut doc, &key).map_err(CliError::new)? {
            removed_table = true;
            for w in settings_edit::write_checked(&settings_path, &doc.to_string())? {
                err_line(&format!("{} {w}", app.err.yellow("warning:")));
            }
        }
    }
    if !removed_file && !removed_table {
        return Err(CliError::new(format!(
            "there is no imported or configured uploader called {name:?}"
        ))
        .hint("`ssx uploaders list` shows what exists; built-in destinations cannot be removed"));
    }
    if removed_file {
        out_line(&format!("removed the imported file for {name}"));
    }
    if removed_table {
        out_line(&format!("removed [uploaders.{name}] from settings.toml"));
    }
    let settings = app.load_settings()?;
    let dangling: Vec<&str> =
        settings.destinations.referenced_names().filter(|n| *n == name).collect();
    if !dangling.is_empty() {
        err_line(&format!(
            "{} destinations in settings.toml still point at {name:?}; change them with `ssx config set destinations.image NAME`",
            app.err.yellow("warning:")
        ));
    }
    Ok(())
}

fn test(app: &App, name: &str, json: bool) -> CliResult<()> {
    let settings = app.load_settings()?;
    let services = app.services(&settings)?;
    let out = services.uploads.test(name, &app.cancel)?;
    if json {
        out_line(&serde_json::to_string_pretty(&serde_json::json!({
            "name": name,
            "url": out.url,
            "thumbnail_url": out.thumbnail_url,
            "deletion_url": out.deletion_url,
        }))?);
    } else {
        out_line(&out.url);
        if let Some(d) = &out.deletion_url {
            err_line(&format!("delete it with: {d}"));
        }
    }
    Ok(())
}

/// Secret names follow the uploader-name rule.
fn check_secret_name(name: &str) -> CliResult<()> {
    if ssx_services::upload::sxcu_files::valid_uploader_name(name) {
        Ok(())
    } else {
        Err(CliError::usage(format!("{name:?} is not a valid secret name"))
            .hint("use letters, digits, '.', '_' or '-' (at most 64 characters)"))
    }
}

/// Reads a secret from standard input: a prompt on a terminal, the first line of a pipe.
fn read_secret_from_stdin(prompt: &str) -> CliResult<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        err_line(prompt);
    }
    let mut text = String::new();
    stdin
        .lock()
        .take(64 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| CliError::new(format!("cannot read the secret from standard input: {e}")))?;
    let value = text.lines().next().unwrap_or("").trim_end_matches('\r').to_owned();
    if value.is_empty() {
        return Err(CliError::new("no secret was given on standard input")
            .hint("pipe it in: printf %s \"$TOKEN\" | ssx uploaders secret set NAME"));
    }
    Ok(value)
}

fn secret(app: &App, cmd: SecretCmd) -> CliResult<()> {
    let secrets = LazySecrets::new();
    match cmd {
        SecretCmd::Set { name } => {
            check_secret_name(&name)?;
            let value = read_secret_from_stdin(&format!(
                "Type the secret for {name:?} and press Enter (the input is visible):"
            ))?;
            secrets.set(&name, &value).map_err(|e| CliError::new(e.to_string()))?;
            let st = secrets.status();
            if st.persistent {
                out_line(&format!(
                    "stored {name:?} in the {}",
                    st.backend.unwrap_or("credential store")
                ));
            } else {
                err_line(&format!(
                    "{} there is no OS credential store here ({}), so {name:?} was NOT saved. \
                     Provide it at run time instead: export {}=...",
                    app.err.yellow("warning:"),
                    st.unavailable_reason.unwrap_or_default(),
                    ssx_services::secrets::env_var_name(&name)
                ));
                return Err(CliError::new("the secret was not saved"));
            }
        }
        SecretCmd::Delete { name } => {
            check_secret_name(&name)?;
            secrets.delete(&name).map_err(|e| CliError::new(e.to_string()))?;
            out_line(&format!("deleted {name:?}"));
        }
        SecretCmd::Status { names } => {
            let st = ssx_services::LayeredSecretStore::probe_status();
            match (st.backend, &st.unavailable_reason) {
                (Some(b), _) => out_line(&format!("secret store: {b}")),
                (None, why) => out_line(&format!(
                    "secret store: none ({}); use {}<NAME> environment variables",
                    why.clone().unwrap_or_default(),
                    ssx_services::secrets::ENV_PREFIX
                )),
            }
            for name in names {
                check_secret_name(&name)?;
                let env = std::env::var_os(ssx_services::secrets::env_var_name(&name))
                    .is_some_and(|v| !v.is_empty());
                let present = secrets.get(&name).ok().flatten().is_some();
                let text = match (env, present) {
                    (true, _) => "set (from environment)",
                    (false, true) => "set",
                    (false, false) => "not set",
                };
                out_line(&format!("{name}: {text}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        name: &str,
        kind: &str,
        uploads: &[&'static str],
        shortens: bool,
        error: Option<&str>,
    ) -> UploaderInfo {
        UploaderInfo {
            name: name.into(),
            kind: kind.into(),
            origin: "settings.toml".into(),
            uploads: uploads.to_vec(),
            shortens,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn the_table_shows_what_each_destination_accepts_and_why_one_is_broken() {
        let out = render_list(
            &[
                info("pic", "imgur", &["image", "video"], false, None),
                info("is.gd", "shortener", &[], true, None),
                info("bad", "broken", &[], false, Some("missing bucket")),
            ],
            Style::plain(),
        );
        assert!(out.contains("image,video") && out.contains("urls"), "{out}");
        assert!(out.contains("broken: missing bucket"), "{out}");
        assert!(out.lines().next().unwrap().starts_with("NAME"));
    }

    #[test]
    fn secret_names_are_validated() {
        assert!(check_secret_name("imgur-token").is_ok());
        for bad in ["", "a b", "a/b", "$(x)", &"x".repeat(65)] {
            assert_eq!(
                check_secret_name(bad).unwrap_err().code,
                crate::error::ExitCode::Usage,
                "{bad:?}"
            );
        }
    }
}
