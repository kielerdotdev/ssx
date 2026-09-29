//! Argument templates for `run_command`.
//!
//! Each argument is expanded **on its own**: `{name}` is replaced by the variable's value and
//! the result stays *one* argument no matter what it contains (spaces, quotes, `;`, `$(…)`).
//! Nothing is ever concatenated into a command line, and no shell is involved, so a hostile
//! file name or URL cannot inject commands. Use `{{` and `}}` for literal braces.
//!
//! Variables: `{path}` (absolute path of the local file), `{dir}`, `{file_name}`, `{url}`,
//! `{short_url}`, `{thumbnail_url}`, `{deletion_url}`.

/// Values available to a template. `None` = not available for this item.
#[derive(Debug, Default, Clone)]
pub struct TemplateVars {
    /// `{path}`.
    pub path: Option<String>,
    /// `{dir}`.
    pub dir: Option<String>,
    /// `{file_name}`.
    pub file_name: Option<String>,
    /// `{url}` (the current URL, i.e. the short one after `shorten_url`).
    pub url: Option<String>,
    /// `{short_url}`.
    pub short_url: Option<String>,
    /// `{thumbnail_url}`.
    pub thumbnail_url: Option<String>,
    /// `{deletion_url}`.
    pub deletion_url: Option<String>,
}

/// Why a template could not be expanded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// `{name}` is not a known variable.
    #[error("unknown variable {{{0}}} in argument {1:?}; available: {{path}} {{dir}} {{file_name}} {{url}} {{short_url}} {{thumbnail_url}} {{deletion_url}}")]
    UnknownVariable(String, String),
    /// The variable is known but has no value for this item.
    #[error("{{{0}}} is not available for this file (for example {{url}} needs a successful upload)")]
    Unavailable(String),
    /// A `{` without a matching `}`, or a lone `}`.
    #[error("unbalanced brace in argument {0:?}; write {{{{ and }}}} for literal braces")]
    Unbalanced(String),
}

/// Expands one argument.
pub fn expand(arg: &str, vars: &TemplateVars) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for n in chars.by_ref() {
                    if n == '}' {
                        closed = true;
                        break;
                    }
                    name.push(n);
                }
                if !closed {
                    return Err(TemplateError::Unbalanced(arg.to_owned()));
                }
                let value = match name.as_str() {
                    "path" => &vars.path,
                    "dir" => &vars.dir,
                    "file_name" => &vars.file_name,
                    "url" => &vars.url,
                    "short_url" => &vars.short_url,
                    "thumbnail_url" => &vars.thumbnail_url,
                    "deletion_url" => &vars.deletion_url,
                    _ => return Err(TemplateError::UnknownVariable(name, arg.to_owned())),
                };
                match value {
                    Some(v) => out.push_str(v),
                    None => return Err(TemplateError::Unavailable(name)),
                }
            }
            '}' => return Err(TemplateError::Unbalanced(arg.to_owned())),
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Expands every argument, failing on the first error.
pub fn expand_all(args: &[String], vars: &TemplateVars) -> Result<Vec<String>, TemplateError> {
    args.iter().map(|a| expand(a, vars)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        TemplateVars {
            path: Some("/tmp/my shot.png".into()),
            dir: Some("/tmp".into()),
            file_name: Some("my shot.png".into()),
            url: Some("https://x.example/a?b=1&c=2".into()),
            short_url: None,
            thumbnail_url: Some("https://x.example/t".into()),
            deletion_url: Some("https://x.example/d".into()),
        }
    }

    #[test]
    fn substitutes_inside_one_argument() {
        assert_eq!(expand("{path}", &vars()).unwrap(), "/tmp/my shot.png");
        assert_eq!(expand("--url={url}", &vars()).unwrap(), "--url=https://x.example/a?b=1&c=2");
        assert_eq!(expand("{dir}/{file_name}.bak", &vars()).unwrap(), "/tmp/my shot.png.bak");
        assert_eq!(expand("plain", &vars()).unwrap(), "plain");
        assert_eq!(expand("", &vars()).unwrap(), "");
    }

    #[test]
    fn values_are_never_reinterpreted() {
        let v = TemplateVars {
            path: Some("{url} $(rm -rf /); `id` \"q\" 'x' {{ }}".into()),
            url: Some("SHOULD-NOT-APPEAR".into()),
            ..TemplateVars::default()
        };
        assert_eq!(expand("{path}", &v).unwrap(), "{url} $(rm -rf /); `id` \"q\" 'x' {{ }}");
    }

    #[test]
    fn escaped_braces() {
        assert_eq!(expand("{{literal}}", &vars()).unwrap(), "{literal}");
        assert_eq!(expand("{{{url}}}", &vars()).unwrap(), "{https://x.example/a?b=1&c=2}");
    }

    #[test]
    fn errors() {
        assert!(matches!(expand("{nope}", &vars()), Err(TemplateError::UnknownVariable(n, _)) if n == "nope"));
        assert!(matches!(expand("{short_url}", &vars()), Err(TemplateError::Unavailable(n)) if n == "short_url"));
        for bad in ["{", "a{b", "}", "a}b", "{path"] {
            assert!(matches!(expand(bad, &vars()), Err(TemplateError::Unbalanced(_))), "{bad}");
        }
        assert!(expand("{}", &vars()).unwrap_err().to_string().contains("unknown variable"));
    }

    #[test]
    fn expand_all_stops_at_first_error() {
        let args = vec!["ok".to_owned(), "{path}".to_owned(), "{bad}".to_owned()];
        assert!(expand_all(&args, &vars()).is_err());
        let args = vec!["a".to_owned(), "{url}".to_owned()];
        assert_eq!(expand_all(&args, &vars()).unwrap().len(), 2);
    }

    #[test]
    fn unicode_is_preserved() {
        let v = TemplateVars { file_name: Some("日本語 🎉.png".into()), ..TemplateVars::default() };
        assert_eq!(expand("«{file_name}»", &v).unwrap(), "«日本語 🎉.png»");
    }
}
