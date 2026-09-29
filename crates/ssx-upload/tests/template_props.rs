//! Property tests: the template machinery must never panic on arbitrary input, and escaping
//! must round-trip.
#![cfg(feature = "sxcu")]

use proptest::prelude::*;
use ssx_upload::nameparser::NameParser;
use ssx_upload::sxcu::jsonpath;
use ssx_upload::sxcu::template::{
    Env, NonInteractive, Template, TemplateResponse, escape_literal, expand_names_keeping_escapes, render, url_encode,
};

/// Strings made of the characters that matter to the template grammar plus function names.
fn templateish() -> impl Strategy<Value = String> {
    let pieces = prop::sample::select(vec![
        "{", "}", "|", ":", "\\", "%", "$", "json", "xml", "regex", "base64", "random", "select", "header",
        "response", "responseurl", "filename", "input", "inputbox", "outputbox", "a", "b", "0", "1", "[", "]",
        "(", ")", ".", "*", "?", "'", "\"", " ", "日", "é", "\n", "%rn{3}", "%y", "$.a", "{json:a}", "{regex:(a)|1}",
    ]);
    prop::collection::vec(pieces, 0..40).prop_map(|v| v.concat())
}

fn response() -> TemplateResponse {
    TemplateResponse {
        text: r#"{"a":[1,{"b":"c"}],"d":"<x>é</x>"}"#.into(),
        url: "http://h/x".into(),
        headers: vec![("A".into(), "b".into())],
    }
}

fn env<'a>(resp: Option<&'a TemplateResponse>, encode: bool) -> Env<'a> {
    Env { file_name: "f é.png", input: "in put", response: resp, url_encode: encode, interaction: &NonInteractive }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn parse_and_render_never_panic_on_arbitrary_text(s in any::<String>()) {
        let resp = response();
        if let Ok(t) = Template::parse(&s) {
            let _ = t.calls();
            let _ = t.render(&env(Some(&resp), true));
            let _ = t.render(&env(None, false));
        }
    }

    #[test]
    fn parse_and_render_never_panic_on_templateish_text(s in templateish()) {
        let resp = response();
        if let Ok(t) = Template::parse(&s) {
            let _ = t.calls();
            let _ = t.render(&env(Some(&resp), false));
            let _ = t.render(&env(None, true));
        }
    }

    #[test]
    fn escape_literal_round_trips(s in prop_oneof![any::<String>(), templateish()]) {
        let escaped = escape_literal(&s);
        let out = render(&escaped, &env(None, false));
        prop_assert_eq!(out.as_deref(), Ok(s.as_str()));
        // Escaped text never contains a call, so validation has nothing to complain about.
        prop_assert!(Template::parse(&escaped).map(|t| t.calls().is_empty()).unwrap_or(false));
    }

    #[test]
    fn escaped_text_survives_the_name_parser_pipeline(s in prop_oneof![any::<String>(), templateish()]) {
        // `%` codes are expanded by the name parser; escaping `%` as `\%` must protect it, and
        // escape_literal leaves `%` alone, so protect it manually as the docs advise.
        let protected = escape_literal(&s).replace('%', "\\%");
        let names = NameParser::text();
        let expanded = expand_names_keeping_escapes(&protected, &names);
        let out = render(&expanded, &env(None, false));
        prop_assert_eq!(out.as_deref(), Ok(s.as_str()));
    }

    #[test]
    fn name_parser_never_panics(s in prop_oneof![any::<String>(), templateish()]) {
        let out = NameParser::text().parse(&s);
        // Output is bounded: hostile repeat counts cannot explode memory.
        prop_assert!(out.len() <= s.len().saturating_mul(4096) + 64);
    }

    #[test]
    fn jsonpath_never_panics(path in prop_oneof![any::<String>(), templateish()]) {
        let doc: serde_json::Value = serde_json::from_str(&response().text).unwrap();
        let _ = jsonpath::select_all(&doc, &path);
    }

    #[test]
    fn url_encode_output_is_url_safe_and_reversible(s in any::<String>()) {
        let enc = url_encode(&s);
        prop_assert!(enc.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'%')));
        let dec = percent_decode(&enc);
        prop_assert_eq!(dec, s);
    }

    #[test]
    fn regex_xml_and_json_functions_survive_hostile_arguments(pat in any::<String>()) {
        let resp = response();
        let e = env(Some(&resp), false);
        for f in ["regex", "xml", "json"] {
            let t = format!("{{{f}:{}}}", escape_literal(&pat));
            let _ = render(&t, &e);
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}
