//! Editing `settings.toml` in place: dotted keys, comment preservation, validation before
//! writing.
//!
//! `ssx config set` and friends edit the *text* of the file with `toml_edit`, so comments,
//! ordering and keys unknown to this version survive. The candidate result is then parsed
//! and validated exactly as `ssx` would load it; if that fails the file on disk is left
//! untouched and the error says what to fix. `Settings::save` (which re-serialises the whole
//! file) is deliberately not used here for that reason.

use std::{path::Path, str::FromStr};

use ssx_core::settings::{Loaded, Settings, Severity, atomic_write};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::{CliError, CliResult};

/// One step of a dotted key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// A table key.
    Key(String),
    /// An array index (`workflows[2]`).
    Index(usize),
}

/// Splits `general.image_format`, `workflows[0].name` and `uploaders."is.gd".type` into
/// segments. Double quotes protect dots inside a key.
pub fn parse_key(key: &str) -> Result<Vec<Seg>, String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = key.chars().peekable();
    let bad = |why: &str| format!("invalid key {key:?}: {why}");
    let flush = |cur: &mut String, segs: &mut Vec<Seg>| -> Result<(), String> {
        if cur.is_empty() {
            return Err(bad("empty segment"));
        }
        segs.push(Seg::Key(std::mem::take(cur)));
        Ok(())
    };
    let mut just_closed_index = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                just_closed_index = false;
            }
            '.' if !in_quotes => {
                if just_closed_index {
                    just_closed_index = false;
                } else {
                    flush(&mut cur, &mut segs)?;
                }
            }
            '[' if !in_quotes => {
                if just_closed_index {
                    return Err(bad("nested lists are not supported"));
                }
                if !cur.is_empty() {
                    flush(&mut cur, &mut segs)?;
                } else if segs.is_empty() {
                    return Err(bad("a key cannot start with an index"));
                }
                let mut digits = String::new();
                loop {
                    match chars.next() {
                        Some(']') => break,
                        Some(d) if d.is_ascii_digit() => digits.push(d),
                        _ => return Err(bad("expected a number between [ and ]")),
                    }
                }
                segs.push(Seg::Index(digits.parse().map_err(|_| bad("index is too large or empty"))?));
                just_closed_index = true;
            }
            c => {
                if just_closed_index {
                    return Err(bad("expected `.` or `[` after an index"));
                }
                cur.push(c);
            }
        }
    }
    if in_quotes {
        return Err(bad("unterminated quote"));
    }
    if !just_closed_index {
        flush(&mut cur, &mut segs)?;
    }
    Ok(segs)
}

/// What kind of value `key` currently holds (from the file or the defaults), to decide how
/// to read the new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    String,
    Other,
}

fn expectation(doc: &DocumentMut, defaults: &DocumentMut, segs: &[Seg]) -> Expect {
    let is_string = |d: &DocumentMut| {
        find_value(d.as_item(), segs).is_some_and(|v| v.is_str())
    };
    if is_string(doc) || is_string(defaults) { Expect::String } else { Expect::Other }
}

/// Looks up a scalar/array value by path, descending through tables, inline tables, arrays
/// of tables and arrays.
pub fn find_value<'a>(root: &'a Item, segs: &[Seg]) -> Option<&'a Value> {
    enum Cur<'a> {
        Item(&'a Item),
        Val(&'a Value),
        Tab(&'a Table),
    }
    let mut cur = Cur::Item(root);
    for s in segs {
        cur = match (s, cur) {
            (Seg::Key(k), Cur::Item(Item::Table(t)) | Cur::Tab(t)) => Cur::Item(t.get(k)?),
            (Seg::Key(k), Cur::Item(Item::Value(Value::InlineTable(t))) | Cur::Val(Value::InlineTable(t))) => {
                Cur::Val(t.get(k)?)
            }
            (Seg::Index(i), Cur::Item(Item::ArrayOfTables(a))) => Cur::Tab(a.get(*i)?),
            (Seg::Index(i), Cur::Item(Item::Value(Value::Array(a))) | Cur::Val(Value::Array(a))) => {
                Cur::Val(a.get(*i)?)
            }
            _ => return None,
        };
    }
    match cur {
        Cur::Item(Item::Value(v)) | Cur::Val(v) => Some(v),
        _ => None,
    }
}

/// Reads `text` as a TOML value, or as a plain string when it is not valid TOML (or when the
/// setting is a string and the text is not itself a quoted string).
pub fn parse_value(text: &str, expect_string: bool) -> Value {
    let trimmed = text.trim();
    let quoted = (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2);
    if expect_string && !quoted {
        return Value::from(text);
    }
    Value::from_str(trimmed).unwrap_or_else(|_| Value::from(text))
}

/// Sets `key` to `value_text` in `doc`, creating intermediate tables. `defaults` (the
/// serialised default settings) tells string settings from numbers.
pub fn set_key(
    doc: &mut DocumentMut,
    defaults: &DocumentMut,
    key: &str,
    value_text: &str,
) -> Result<(), String> {
    let segs = parse_key(key)?;
    let expect_string = expectation(doc, defaults, &segs) == Expect::String;
    let value = parse_value(value_text, expect_string);
    set_value(doc, &segs, value, key)
}

fn set_value(doc: &mut DocumentMut, segs: &[Seg], value: Value, key: &str) -> Result<(), String> {
    let (last, parents) = segs.split_last().ok_or_else(|| format!("invalid key {key:?}"))?;
    let mut table: &mut Table = doc.as_table_mut();
    let mut i = 0;
    while i < parents.len() {
        match &parents[i] {
            Seg::Key(k) => {
                let next_is_index = matches!(parents.get(i + 1), Some(Seg::Index(_)));
                let entry = table.entry(k).or_insert_with(|| {
                    if next_is_index { Item::ArrayOfTables(toml_edit::ArrayOfTables::new()) } else { Item::Table(Table::new()) }
                });
                if next_is_index {
                    let Seg::Index(idx) = parents[i + 1] else { unreachable!("checked above") };
                    let Item::ArrayOfTables(arr) = entry else {
                        return Err(format!("{key:?}: {k} is not a list of tables"));
                    };
                    let len = arr.len();
                    table = arr
                        .get_mut(idx)
                        .ok_or_else(|| format!("{key:?}: {k} has no entry number {idx} (it has {len})"))?;
                    i += 2;
                } else {
                    let Item::Table(t) = entry else {
                        return Err(format!("{key:?}: {k} is not a table, so nothing can be set inside it"));
                    };
                    t.set_implicit(true);
                    table = t;
                    i += 1;
                }
            }
            Seg::Index(_) => return Err(format!("invalid key {key:?}: unexpected index")),
        }
    }
    match last {
        Seg::Key(k) => {
            // Replace an existing value in place so its comments and formatting survive;
            // `Table::insert` would replace the whole entry, key comments included.
            if let Some(Item::Value(old)) = table.get_mut(k) {
                let decor = old.decor().clone();
                *old = value;
                *old.decor_mut() = decor;
            } else {
                table.insert(k, Item::Value(value));
            }
            Ok(())
        }
        Seg::Index(_) => Err(format!("invalid key {key:?}: a key cannot end with an index; set the fields of that entry")),
    }
}

/// Removes `key`; `false` if it was not there.
pub fn remove_key(doc: &mut DocumentMut, key: &str) -> Result<bool, String> {
    let segs = parse_key(key)?;
    let (last, parents) = segs.split_last().ok_or_else(|| format!("invalid key {key:?}"))?;
    let Seg::Key(last) = last else { return Err(format!("invalid key {key:?}: cannot remove a list entry")) };
    let mut table: &mut Table = doc.as_table_mut();
    let mut i = 0;
    while i < parents.len() {
        let Seg::Key(k) = &parents[i] else { return Err(format!("invalid key {key:?}")) };
        let idx = match parents.get(i + 1) {
            Some(Seg::Index(n)) => Some(*n),
            _ => None,
        };
        let Some(item) = table.get_mut(k) else { return Ok(false) };
        match (item, idx) {
            (Item::Table(t), None) => {
                table = t;
                i += 1;
            }
            (Item::ArrayOfTables(a), Some(n)) => {
                let Some(t) = a.get_mut(n) else { return Ok(false) };
                table = t;
                i += 2;
            }
            _ => return Ok(false),
        }
    }
    Ok(table.remove(last).is_some())
}

/// Parses `text` as settings and validates it; error-severity issues are returned as an
/// error listing all of them, warnings are returned alongside the parsed settings.
pub fn check_text(text: &str) -> CliResult<(Loaded, Vec<String>)> {
    let loaded = Settings::from_toml_str(text).map_err(|e| match e {
        ssx_core::settings::SettingsError::Parse { message, .. } => {
            CliError::new(format!("the result would not be valid settings: {}", message.trim()))
                .hint("nothing was written; correct the value and try again")
        }
        other => CliError::from(other),
    })?;
    let issues = loaded.settings.validate();
    let errors: Vec<String> =
        issues.iter().filter(|i| i.severity == Severity::Error).map(ToString::to_string).collect();
    if !errors.is_empty() {
        return Err(CliError::new(format!("the result would be invalid:\n  {}", errors.join("\n  ")))
            .hint("nothing was written; correct the value and try again"));
    }
    let warnings = issues
        .iter()
        .filter(|i| i.severity == Severity::Warning)
        .map(ToString::to_string)
        .chain(loaded.warnings.iter().cloned())
        .collect();
    Ok((loaded, warnings))
}

/// The current settings text, or the serialised defaults for a missing file.
pub fn read_text_or_default(path: &Path) -> CliResult<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default().to_toml_string()?),
        Err(e) => Err(CliError::new(format!("cannot read {}: {e}", path.display()))),
    }
}

/// Validates `text` and writes it atomically to `path`. Returns the warnings.
pub fn write_checked(path: &Path, text: &str) -> CliResult<Vec<String>> {
    let (_, warnings) = check_text(text)?;
    atomic_write(path, text.as_bytes())
        .map_err(|e| CliError::new(format!("cannot write {}: {e}", path.display())))?;
    Ok(warnings)
}

/// Sets `key` in the file at `path` (creating it from the defaults if missing).
pub fn set_in_file(path: &Path, key: &str, value: &str) -> CliResult<Vec<String>> {
    let text = read_text_or_default(path)?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| CliError::new(format!("{} is not valid TOML: {e}", path.display())).hint("fix it with `ssx config edit`"))?;
    let defaults: DocumentMut = Settings::default().to_toml_string()?.parse().map_err(|e| CliError::new(format!("internal error: {e}")))?;
    set_key(&mut doc, &defaults, key, value).map_err(|e| CliError::new(e).hint("`ssx config show` lists all keys"))?;
    write_checked(path, &doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> DocumentMut {
        Settings::default().to_toml_string().unwrap().parse().unwrap()
    }

    fn set(text: &str, key: &str, value: &str) -> Result<DocumentMut, String> {
        let mut doc: DocumentMut = text.parse().unwrap();
        set_key(&mut doc, &defaults(), key, value)?;
        Ok(doc)
    }

    #[test]
    fn keys_split_into_segments() {
        use Seg::*;
        let k = |s: &str| Key(s.to_owned());
        assert_eq!(parse_key("a.b.c").unwrap(), [k("a"), k("b"), k("c")]);
        assert_eq!(parse_key("workflows[2].name").unwrap(), [k("workflows"), Index(2), k("name")]);
        assert_eq!(parse_key("uploaders.\"is.gd\".type").unwrap(), [k("uploaders"), k("is.gd"), k("type")]);
        assert!(parse_key("a.list[0][1]").unwrap_err().contains("nested"));
        for bad in ["", ".a", "a.", "a..b", "[0]", "a[x]", "a[", "a\"b", "a[1]b"] {
            assert!(parse_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn values_are_typed_by_the_setting_they_replace() {
        let doc = set("", "general.image_quality", "80").unwrap();
        assert_eq!(doc["general"]["image_quality"].as_integer(), Some(80));
        let doc = set("", "general.show_notifications", "false").unwrap();
        assert_eq!(doc["general"]["show_notifications"].as_bool(), Some(false));
        // A string setting takes the text literally, even if it looks like a number ...
        let doc = set("", "general.folder_pattern", "2026").unwrap();
        assert_eq!(doc["general"]["folder_pattern"].as_str(), Some("2026"));
        // ... and `%y-%mo` is not valid TOML anyway.
        let doc = set("", "general.file_name_pattern", "Shot_%y-%mo").unwrap();
        assert_eq!(doc["general"]["file_name_pattern"].as_str(), Some("Shot_%y-%mo"));
        // Explicit quotes are honoured.
        let doc = set("", "general.folder_pattern", "\"q\"").unwrap();
        assert_eq!(doc["general"]["folder_pattern"].as_str(), Some("q"));
        // Unknown-type keys: TOML if it parses, else text.
        let doc = set("", "capture.hdr.exposure", "-1.5").unwrap();
        assert!((doc["capture"]["hdr"]["exposure"].as_float().unwrap() + 1.5).abs() < 1e-9);
        let doc = set("", "destinations.image", "imgur").unwrap();
        assert_eq!(doc["destinations"]["image"].as_str(), Some("imgur"));
        let doc = set("", "uploaders.\"is.gd\".type", "shortener").unwrap();
        assert_eq!(doc["uploaders"]["is.gd"]["type"].as_str(), Some("shortener"));
    }

    #[test]
    fn comments_order_and_unknown_keys_survive() {
        let text = "# my settings\nfuture_key = 1\n\n[general]\n# quality\nimage_quality = 90 # inline\nimage_format = \"png\"\n";
        let doc = set(text, "general.image_quality", "70").unwrap();
        let out = doc.to_string();
        assert!(out.contains("# my settings") && out.contains("# quality") && out.contains("future_key = 1"), "{out}");
        assert!(out.contains("image_quality = 70"), "{out}");
        assert!(out.contains("image_format = \"png\""));
    }

    #[test]
    fn workflow_list_entries_can_be_edited() {
        let text = Settings::default().to_toml_string().unwrap();
        let mut doc: DocumentMut = text.parse().unwrap();
        set_key(&mut doc, &defaults(), "workflows[0].name", "Renamed").unwrap();
        let (loaded, _) = check_text(&doc.to_string()).unwrap();
        assert_eq!(loaded.settings.workflows[0].name, "Renamed");
        let e = set_key(&mut doc, &defaults(), "workflows[99].name", "x").unwrap_err();
        assert!(e.contains("no entry number 99"), "{e}");
        let e = set_key(&mut doc, &defaults(), "general.image_quality.deeper", "1").unwrap_err();
        assert!(e.contains("not a table"), "{e}");
        assert!(set_key(&mut doc, &defaults(), "workflows[0]", "1").is_err());
    }

    #[test]
    fn invalid_results_are_refused_with_every_problem_listed() {
        let mut doc: DocumentMut = "".parse().unwrap();
        set_key(&mut doc, &defaults(), "general.image_quality", "0").unwrap();
        set_key(&mut doc, &defaults(), "capture.delay_ms", "999999").unwrap();
        let e = check_text(&doc.to_string()).unwrap_err();
        assert!(e.message.contains("general.image_quality") && e.message.contains("1 to 100"), "{}", e.message);
        assert!(e.message.contains("capture.delay_ms"), "all problems at once: {}", e.message);
        // A number that does not even fit the type is a parse-level error.
        let mut doc: DocumentMut = "".parse().unwrap();
        set_key(&mut doc, &defaults(), "general.image_quality", "500").unwrap();
        let e = check_text(&doc.to_string()).unwrap_err();
        assert!(e.message.contains("general.image_quality") && e.message.contains("would not be valid"), "{}", e.message);
        assert!(e.hint.unwrap().contains("nothing was written"));
        // Wrong type for the field: parse error from the settings loader.
        let mut doc: DocumentMut = "".parse().unwrap();
        set_key(&mut doc, &defaults(), "general.image_quality", "high").unwrap();
        assert!(check_text(&doc.to_string()).is_err());
    }

    #[test]
    fn plain_text_secrets_are_refused() {
        let mut doc: DocumentMut = "".parse().unwrap();
        set_key(&mut doc, &defaults(), "uploaders.x.type", "imgur").unwrap();
        set_key(&mut doc, &defaults(), "uploaders.x.access_token", "hunter2").unwrap();
        let e = check_text(&doc.to_string()).unwrap_err();
        assert!(e.message.contains("plain-text secret"), "{}", e.message);
    }

    #[test]
    fn set_in_file_creates_updates_and_never_writes_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/settings.toml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        set_in_file(&p, "general.image_quality", "75").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("image_quality = 75") && text.contains("[[workflows]]"), "created from the defaults");
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(set_in_file(&p, "general.image_quality", "0").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before, "a refused change leaves the file alone");
        std::fs::write(&p, "not [valid").unwrap();
        let e = set_in_file(&p, "general.image_quality", "5").unwrap_err();
        assert!(e.message.contains("not valid TOML") && e.hint.unwrap().contains("config edit"));
    }

    #[test]
    fn keys_can_be_removed() {
        let mut doc: DocumentMut = "[a]\nb = 1\n[[w]]\nn = 1\n".parse().unwrap();
        assert!(remove_key(&mut doc, "a.b").unwrap());
        assert!(!remove_key(&mut doc, "a.b").unwrap());
        assert!(!remove_key(&mut doc, "x.y").unwrap());
        assert!(remove_key(&mut doc, "w[0].n").unwrap());
        assert!(remove_key(&mut doc, "w[0]").is_err());
    }
}
