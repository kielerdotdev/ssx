//! Executes a [`CustomUploader`]: builds the request, streams it, evaluates the response
//! templates.
//!
//! Request construction follows ShareX's `CustomFileUploader`/`CustomTextUploader`/
//! `CustomURLShortener`:
//!
//! * `RequestURL`: templates evaluated with `{input}`/`{filename}` percent-encoded, an
//!   `https://` prefix added when there is no scheme, then `Parameters` appended as an
//!   encoded query string.
//! * `Parameters`/`Headers`/`Arguments` values: `%` name-parser codes first (escapes kept),
//!   then templates, without percent-encoding.
//! * `Data` (JSON/XML): `%` codes, then only `{input}`/`{filename}` are substituted,
//!   JSON- or XML-escaped. Other templates are *not* evaluated because JSON is full of
//!   literal braces; this is what ShareX does too.
//! * Files travel as multipart (streaming, with progress) or as the raw body (`Binary`).
//!
//! Success is any 2xx. The final URL after redirects feeds `{responseurl}`. On failure the
//! `ErrorMessage` template is evaluated and attached to the returned error.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};

use super::model::{BodyType, CustomUploader, replace_ci};
use super::template::{
    Env, Interaction, NonInteractive, TemplateError, TemplateResponse, render, render_with_names,
    url_encode,
};
use crate::body::{BodyPlan, Payload};
use crate::context::UploadContext;
use crate::error::UploadError;
use crate::http::{self, HttpResponse, RAW_RESPONSE_LIMIT, truncate_bytes};
use crate::multipart::{self, FilePart};
use crate::nameparser::NameParser;
use crate::types::{UploadKind, UploadRequest, UploadResult, Uploader, UrlShortener};

/// An [`Uploader`] (and [`UrlShortener`]) backed by a `.sxcu` definition.
#[derive(Clone)]
pub struct SxcuUploader {
    def: Arc<CustomUploader>,
    name: String,
    interaction: Arc<dyn Interaction>,
    counter: Arc<AtomicU64>,
}

impl std::fmt::Debug for SxcuUploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SxcuUploader").field("name", &self.name).finish_non_exhaustive()
    }
}

fn config_err(field: &str, e: &TemplateError) -> UploadError {
    UploadError::config(format!("{field}: {e}"))
}

fn json_escape(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_default();
    quoted.get(1..quoted.len().saturating_sub(1)).unwrap_or_default().to_owned()
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// ShareX's `URLHelpers.FixPrefix`: add `https://` when the text has no scheme.
fn fix_prefix(url: &str) -> String {
    let has_prefix = url.split_once("://").is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    if url.is_empty() || has_prefix { url.to_owned() } else { format!("https://{url}") }
}

impl SxcuUploader {
    /// Wrap a definition, rejecting it when [`CustomUploader::validate`] fails.
    pub fn new(def: CustomUploader) -> Result<Self, UploadError> {
        def.validate().map_err(|e| UploadError::config(e.to_string()))?;
        let name = def.display_name();
        Ok(Self {
            def: Arc::new(def),
            name,
            interaction: Arc::new(NonInteractive),
            counter: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Parse `.sxcu` text and wrap it.
    pub fn from_json_str(text: &str) -> Result<Self, UploadError> {
        let def =
            CustomUploader::from_json_str(text).map_err(|e| UploadError::config(e.to_string()))?;
        Self::new(def)
    }

    /// Handle `select`/`inputbox`/`outputbox` through `interaction` instead of the headless
    /// defaults.
    #[must_use]
    pub fn with_interaction(mut self, interaction: Arc<dyn Interaction>) -> Self {
        self.interaction = interaction;
        self
    }

    /// The underlying definition.
    pub fn definition(&self) -> &CustomUploader {
        &self.def
    }

    fn names(&self) -> NameParser {
        NameParser::text().with_counter(&self.counter)
    }

    fn request_env<'a>(&'a self, file_name: &'a str, input: &'a str, url_encode: bool) -> Env<'a> {
        Env { file_name, input, response: None, url_encode, interaction: self.interaction.as_ref() }
    }

    fn build_url(
        &self,
        file_name: &str,
        input: &str,
        names: &NameParser,
    ) -> Result<String, UploadError> {
        let env = self.request_env(file_name, input, true);
        let rendered =
            render(&self.def.request_url, &env).map_err(|e| config_err("RequestURL", &e))?;
        let mut url = fix_prefix(&rendered);
        if url.is_empty() {
            return Err(UploadError::config("RequestURL must be configured"));
        }
        let env = self.request_env(file_name, input, false);
        let mut sep = if url.contains('?') { '&' } else { '?' };
        for (k, v) in &self.def.parameters {
            let value = render_with_names(v, names, &env)
                .map_err(|e| config_err(&format!("Parameters.{k}"), &e))?;
            url.push(sep);
            sep = '&';
            url.push_str(&url_encode(k));
            url.push('=');
            url.push_str(&url_encode(&value));
        }
        Ok(url)
    }

    fn build_arguments(
        &self,
        file_name: &str,
        input: &str,
        names: &NameParser,
    ) -> Result<Vec<(String, String)>, UploadError> {
        let env = self.request_env(file_name, input, false);
        self.def
            .arguments
            .iter()
            .map(|(k, v)| {
                render_with_names(v, names, &env)
                    .map(|value| (k.clone(), value))
                    .map_err(|e| config_err(&format!("Arguments.{k}"), &e))
            })
            .collect()
    }

    fn build_headers(
        &self,
        file_name: &str,
        input: &str,
        names: &NameParser,
        content_type: Option<&str>,
    ) -> Result<HeaderMap, UploadError> {
        let mut map = HeaderMap::new();
        if let Some(ct) = content_type {
            let v = HeaderValue::from_str(ct)
                .map_err(|_| UploadError::config(format!("invalid content type '{ct}'")))?;
            map.insert(CONTENT_TYPE, v);
        }
        let env = self.request_env(file_name, input, false);
        for (k, v) in &self.def.headers {
            let value = render_with_names(v, names, &env)
                .map_err(|e| config_err(&format!("Headers.{k}"), &e))?;
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| {
                UploadError::config(format!("Headers: '{k}' is not a valid header name"))
            })?;
            let value = HeaderValue::from_str(&value).map_err(|_| {
                UploadError::config(format!(
                    "Headers.{k}: value contains characters not allowed in a header"
                ))
            })?;
            map.insert(name, value);
        }
        Ok(map)
    }

    fn build_data(&self, file_name: &str, input: &str, names: &NameParser) -> String {
        let encode = |s: &str| match self.def.body {
            BodyType::Json => json_escape(s),
            BodyType::Xml => xml_escape(s),
            _ => s.to_owned(),
        };
        let text = names.parse(&self.def.data);
        let text = replace_ci(&text, "{input}", &encode(input));
        replace_ci(&text, "{filename}", &encode(file_name))
    }

    /// Body plan and its `Content-Type` for this request.
    async fn build_body(
        &self,
        req: &UploadRequest,
        file_name: &str,
        input: &str,
        names: &NameParser,
    ) -> Result<(Option<BodyPlan>, Option<String>), UploadError> {
        let kind = req.kind;
        let is_file = !matches!(kind, UploadKind::Text | UploadKind::Url);
        match self.def.body {
            BodyType::None => {
                if is_file {
                    return Err(UploadError::config(
                        "Body is None, so files cannot be uploaded (use MultipartFormData or Binary)",
                    ));
                }
                Ok((None, None))
            }
            BodyType::MultipartFormData => {
                let fields = self.build_arguments(file_name, input, names)?;
                let file = if kind == UploadKind::Url
                    || (kind == UploadKind::Text && self.def.file_form_name.is_empty())
                {
                    None
                } else {
                    if self.def.file_form_name.is_empty() {
                        return Err(UploadError::config("FileFormName must be configured"));
                    }
                    Some(FilePart {
                        field: self.def.file_form_name.clone(),
                        filename: file_name.to_owned(),
                        mime: req.resolved_mime(),
                        payload: Payload::from_request(req).await?,
                    })
                };
                let (ct, plan) = multipart::plan(&fields, file);
                Ok((Some(plan), Some(ct)))
            }
            BodyType::FormUrlEncoded => {
                if is_file {
                    return Err(UploadError::config("FormURLEncoded bodies cannot carry files"));
                }
                let pairs = self.build_arguments(file_name, input, names)?;
                let body = pairs
                    .iter()
                    .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
                    .collect::<Vec<_>>()
                    .join("&");
                Ok((
                    Some(BodyPlan::raw(Payload::Memory(Bytes::from(body)))),
                    Some("application/x-www-form-urlencoded".into()),
                ))
            }
            BodyType::Json | BodyType::Xml => {
                if is_file {
                    return Err(UploadError::config("JSON/XML bodies cannot carry files"));
                }
                let data = self.build_data(file_name, input, names);
                let ct = self.def.body.content_type().map(str::to_owned);
                Ok((Some(BodyPlan::raw(Payload::Memory(Bytes::from(data)))), ct))
            }
            BodyType::Binary => {
                if kind == UploadKind::Url {
                    return Err(UploadError::config("Binary bodies cannot carry a URL to shorten"));
                }
                let payload = Payload::from_request(req).await?;
                Ok((Some(BodyPlan::raw(payload)), Some(req.resolved_mime())))
            }
        }
    }

    fn eval_error_message(&self, resp: &HttpResponse, file_name: &str) -> Option<String> {
        if self.def.error_message.is_empty() {
            return None;
        }
        let tr = to_template_response(resp);
        let env = Env {
            file_name,
            input: "",
            response: Some(&tr),
            url_encode: true,
            interaction: self.interaction.as_ref(),
        };
        render(&self.def.error_message, &env).ok().filter(|m| !m.trim().is_empty())
    }

    fn parse_response(
        &self,
        resp: &HttpResponse,
        file_name: &str,
    ) -> Result<UploadResult, UploadError> {
        let tr = to_template_response(resp);
        let env = Env {
            file_name,
            input: "",
            response: Some(&tr),
            url_encode: true,
            interaction: self.interaction.as_ref(),
        };
        let invalid = |field: &str, e: &TemplateError| {
            UploadError::invalid_response(format!(
                "could not evaluate {field}: {e}. The response began with: {}",
                http::snippet(&resp.text)
            ))
        };
        let url = if self.def.url.is_empty() {
            resp.text.clone()
        } else {
            render(&self.def.url, &env).map_err(|e| invalid("URL", &e))?
        };
        let url = url.trim().to_owned();
        if url.is_empty() && !self.def.url.to_lowercase().contains("{outputbox") {
            return Err(UploadError::invalid_response(format!(
                "the server accepted the upload but no URL could be extracted{}. The response began with: {}",
                if self.def.url.is_empty() { "" } else { " (check the URL template)" },
                http::snippet(&resp.text)
            )));
        }
        let optional = |field: &str, template: &str| -> Result<Option<String>, UploadError> {
            if template.is_empty() {
                return Ok(None);
            }
            let v = render(template, &env).map_err(|e| invalid(field, &e))?;
            let v = v.trim().to_owned();
            Ok(if v.is_empty() { None } else { Some(v) })
        };
        Ok(UploadResult {
            url,
            thumbnail_url: optional("ThumbnailURL", &self.def.thumbnail_url)?,
            deletion_url: optional("DeletionURL", &self.def.deletion_url)?,
            raw_response: truncate_bytes(&resp.text, RAW_RESPONSE_LIMIT),
            uploader_name: self.name.clone(),
            extra: std::collections::BTreeMap::new(),
        })
    }
}

fn to_template_response(resp: &HttpResponse) -> TemplateResponse {
    TemplateResponse {
        text: resp.text.clone(),
        url: resp.url.clone(),
        headers: resp
            .headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_owned(), v.to_owned())))
            .collect(),
    }
}

#[async_trait]
impl Uploader for SxcuUploader {
    fn name(&self) -> &str {
        &self.name
    }

    fn supports(&self, kind: UploadKind) -> bool {
        self.def.supports(kind)
    }

    async fn upload(
        &self,
        req: &UploadRequest,
        ctx: &UploadContext,
    ) -> Result<UploadResult, UploadError> {
        ctx.check_cancelled()?;
        if !self.supports(req.kind) {
            return Err(UploadError::Unsupported { uploader: self.name.clone(), kind: req.kind });
        }
        let file_name = req.resolved_filename();
        let input = req.input_text().await?;
        let names = self.names();
        let url = self.build_url(&file_name, &input, &names)?;
        let (plan, content_type) = self.build_body(req, &file_name, &input, &names).await?;
        let headers = self.build_headers(&file_name, &input, &names, content_type.as_deref())?;

        let mut rb = ctx.http.request(self.def.request_method.to_reqwest(), &url).headers(headers);
        let mut fault = None;
        if let Some(plan) = plan {
            let len = plan.content_length();
            let (body, f) = plan.into_body(ctx).await?;
            fault = Some(f);
            rb = rb.header(CONTENT_LENGTH, len).body(body);
        }
        let resp = http::fetch(ctx, rb, fault.as_ref()).await?;
        if !resp.is_success() {
            let message = self.eval_error_message(&resp, &file_name);
            return Err(resp.to_error(message));
        }
        self.parse_response(&resp, &file_name)
    }
}

#[async_trait]
impl UrlShortener for SxcuUploader {
    fn name(&self) -> &str {
        &self.name
    }

    async fn shorten(&self, url: &str, ctx: &UploadContext) -> Result<String, UploadError> {
        Ok(self.upload(&UploadRequest::url(url), ctx).await?.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_helpers() {
        assert_eq!(json_escape("a\"b\\c\n\u{1}é"), "a\\\"b\\\\c\\n\\u0001é");
        assert_eq!(
            xml_escape("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&apos;&amp;&apos;&lt;/a&gt;"
        );
    }

    #[test]
    fn prefix_fixing() {
        assert_eq!(fix_prefix("example.com/x"), "https://example.com/x");
        assert_eq!(fix_prefix("http://example.com"), "http://example.com");
        assert_eq!(fix_prefix(""), "");
    }
}
