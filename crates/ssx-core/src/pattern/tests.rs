//! End-to-end tests of the pattern engine with a fully injected context.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, thread};

use super::*;

const T0: &str = "2024-03-09T14:05:06.789+01:00"; // a Saturday, ISO week 10

struct Fixture {
    clock: FixedClock,
    rng: SeededRng,
    env: StaticEnv,
    counter: MemoryCounter,
    inputs: NameInputs,
    options: RenderOptions,
}

impl Fixture {
    fn new() -> Self {
        Self::at(T0)
    }
    fn at(ts: &str) -> Self {
        Self {
            clock: FixedClock::from_rfc3339(ts).unwrap(),
            rng: SeededRng::new(42),
            env: StaticEnv {
                user: "alice".into(),
                domain: "WORKGROUP".into(),
                machine: "DESKTOP-1".into(),
                files: BTreeMap::new(),
            },
            counter: MemoryCounter::default(),
            inputs: NameInputs::default(),
            options: RenderOptions::default(),
        }
    }
    fn ctx(&self) -> PatternContext<'_> {
        PatternContext {
            clock: &self.clock,
            rng: &self.rng,
            env: &self.env,
            counter: &self.counter,
            inputs: &self.inputs,
            options: self.options,
        }
    }
    fn text(&self, p: &str) -> String {
        Pattern::parse(p).render(&self.ctx(), PatternKind::Text).unwrap()
    }
    fn name(&self, p: &str) -> String {
        Pattern::parse(p).render(&self.ctx(), PatternKind::FileName).unwrap()
    }
}

#[test]
fn date_time_tokens() {
    let f = Fixture::new();
    assert_eq!(f.text("%y|%yy|%mo|%d|%h|%mi|%s|%ms"), "2024|24|03|09|14|05|06|789");
    assert_eq!(f.text("%mon|%mon2|%w|%w2"), "March|March|Saturday|Saturday");
    assert_eq!(f.text("%wy"), "10");
    assert_eq!(f.text("%unix"), "1709989506");
}

#[test]
fn default_pattern() {
    let f = Fixture::new();
    assert_eq!(f.name("Screenshot_%y-%mo-%d_%h-%mi-%s"), "Screenshot_2024-03-09_14-05-06");
}

#[test]
fn twelve_hour_clock_when_pm_present() {
    let f = Fixture::new();
    assert_eq!(f.text("%h%pm"), "02PM");
    let f = Fixture::at("2024-03-09T00:30:00+00:00");
    assert_eq!(f.text("%h %pm"), "12 AM");
    let f = Fixture::at("2024-03-09T12:00:00+00:00");
    assert_eq!(f.text("%h %pm"), "12 PM");
    let f = Fixture::at("2024-03-09T23:59:59+00:00");
    assert_eq!(f.text("%h %pm"), "11 PM");
    assert_eq!(f.text("%h"), "23", "24-hour without %pm");
}

#[test]
fn timezone_offsets_change_the_rendered_fields_but_not_unix() {
    let a = Fixture::at("2024-03-09T23:30:00-08:00");
    let b = Fixture::at("2024-03-10T08:30:00+01:00");
    assert_eq!(a.text("%d-%h"), "09-23");
    assert_eq!(b.text("%d-%h"), "10-08");
    assert_eq!(a.text("%unix"), b.text("%unix"));
}

#[test]
fn dst_transition_days() {
    // US spring forward 2024-03-10: 01:59:59 -05:00 is followed by 03:00:00 -04:00.
    let before = Fixture::at("2024-03-10T01:59:59-05:00");
    let after = Fixture::at("2024-03-10T03:00:00-04:00");
    assert_eq!(before.text("%h:%mi:%s"), "01:59:59");
    assert_eq!(after.text("%h:%mi:%s"), "03:00:00");
    let delta: i64 =
        after.text("%unix").parse::<i64>().unwrap() - before.text("%unix").parse::<i64>().unwrap();
    assert_eq!(delta, 1, "wall clock skips an hour but time advances one second");
    // Fall back: 01:30 happens twice with different offsets and different unix times.
    let first = Fixture::at("2024-11-03T01:30:00-04:00");
    let second = Fixture::at("2024-11-03T01:30:00-05:00");
    assert_eq!(first.text("%h:%mi"), second.text("%h:%mi"));
    assert_ne!(first.text("%unix"), second.text("%unix"));
}

#[test]
fn iso_week_edge_cases() {
    // 2021-01-03 belongs to ISO week 53 of 2020.
    assert_eq!(Fixture::at("2021-01-03T12:00:00+00:00").text("%y %wy"), "2021 53");
    assert_eq!(Fixture::at("2024-12-30T12:00:00+00:00").text("%y %wy"), "2024 1");
}

#[test]
fn leap_day_and_year_boundaries() {
    let f = Fixture::at("2024-02-29T23:59:59.999+00:00");
    assert_eq!(f.text("%y-%mo-%d %ms %w"), "2024-02-29 999 Thursday");
    let f = Fixture::at("2000-01-01T00:00:00+00:00");
    assert_eq!(f.text("%yy %mo %d %h %mi %s %ms"), "00 01 01 00 00 00 000");
}

#[test]
fn leap_second_millis_are_clamped() {
    let f = Fixture::at("2016-12-31T23:59:60.500Z");
    assert_eq!(f.text("%s.%ms"), "59.999");
}

#[test]
fn all_month_and_day_names() {
    let months = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    for (i, m) in months.iter().enumerate() {
        let f = Fixture::at(&format!("2023-{:02}-15T12:00:00+00:00", i + 1));
        assert_eq!(f.text("%mon"), *m);
        assert_eq!(f.text("%mo"), format!("{:02}", i + 1));
    }
    let days = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    for (i, d) in days.iter().enumerate() {
        // 2024-01-01 is a Monday.
        let f = Fixture::at(&format!("2024-01-{:02}T12:00:00+00:00", i + 1));
        assert_eq!(f.text("%w"), *d);
    }
}

#[test]
fn environment_tokens() {
    let f = Fixture::new();
    assert_eq!(f.text("%un@%cn/%uln"), "alice@DESKTOP-1/WORKGROUP");
}

#[test]
fn window_tokens_sanitised_and_truncated() {
    let mut f = Fixture::new();
    f.inputs.window_title = Some("  My: Document / v2 — Notepad  ".into());
    f.inputs.process_name = Some("note pad.exe".into());
    assert_eq!(f.name("%t"), "My_Document__v2_—_Notepad");
    assert_eq!(f.text("%t"), "My:_Document_/_v2_—_Notepad", "Text kind is not sanitised");
    assert_eq!(f.name("%pn"), "note_pad.exe");
    f.options.max_title_len = Some(5);
    assert_eq!(f.name("%t"), "My_Do");
    f.options.max_title_len = Some(0);
    assert!(f.name("%t").ends_with("Notepad"), "0 means unlimited");
}

#[test]
fn window_title_cannot_inject_path_separators() {
    let mut f = Fixture::new();
    f.inputs.window_title = Some("../../etc/passwd".into());
    let folder = Pattern::parse("%t").render_folder(&f.ctx()).unwrap();
    assert_eq!(folder, PathBuf::from("....etcpasswd"), "slashes removed, one component");
}

#[test]
fn window_title_unicode_truncation_is_grapheme_safe() {
    let mut f = Fixture::new();
    f.inputs.window_title = Some("👨‍👩‍👧‍👦👨‍👩‍👧‍👦👨‍👩‍👧‍👦".into());
    f.options.max_title_len = Some(2);
    assert_eq!(f.text("%t"), "👨‍👩‍👧‍👦👨‍👩‍👧‍👦");
}

#[test]
fn missing_window_info_renders_empty() {
    let f = Fixture::new();
    assert_eq!(f.text("[%t][%pn][%width][%height]"), "[][][][]");
}

#[test]
fn dimensions() {
    let mut f = Fixture::new();
    f.inputs.width = Some(1920);
    f.inputs.height = Some(1080);
    assert_eq!(f.text("%widthx%height"), "1920x1080");
    f.inputs.width = Some(0);
    assert_eq!(f.text("[%width]"), "[]", "zero size is treated as unknown");
}

#[test]
fn counter_tokens() {
    let f = Fixture::new();
    assert_eq!(f.text("%i"), "1");
    assert_eq!(f.text("%i{4}"), "0002");
    // one increment per render, shared by all counter tokens
    assert_eq!(f.text("%i|%i{3}|%ix|%iX"), "3|003|3|3");
    let f = Fixture { counter: MemoryCounter::starting_after(254), ..Fixture::new() };
    assert_eq!(f.text("%ix"), "ff");
    let f = Fixture { counter: MemoryCounter::starting_after(254), ..Fixture::new() };
    assert_eq!(f.text("%iX{4}"), "00FF");
    let f = Fixture { counter: MemoryCounter::starting_after(34), ..Fixture::new() };
    assert_eq!(f.text("%ia|%iA"), "z|Z");
    let f = Fixture { counter: MemoryCounter::starting_after(60), ..Fixture::new() };
    assert_eq!(f.text("%iAa"), "z");
    let f = Fixture { counter: MemoryCounter::starting_after(9), ..Fixture::new() };
    assert_eq!(f.text("%iAa|%iaA"), "A|a");
    let f = Fixture { counter: MemoryCounter::starting_after(4), ..Fixture::new() };
    assert_eq!(f.text("%ib{2,8}"), "00000101");
    let f = Fixture { counter: MemoryCounter::starting_after(4), ..Fixture::new() };
    assert_eq!(f.text("%iB{3,3}"), "012");
}

#[test]
fn counter_not_consumed_without_token() {
    let f = Fixture::new();
    let _ = f.text("%y-%mo");
    assert_eq!(f.counter.next().unwrap(), 1);
}

#[test]
fn counter_error_is_reported() {
    #[derive(Debug)]
    struct Broken;
    impl CounterStore for Broken {
        fn next(&self) -> std::io::Result<u64> {
            Err(std::io::Error::other("disk full"))
        }
    }
    let f = Fixture::new();
    let ctx = PatternContext { counter: &Broken, ..f.ctx() };
    let err = Pattern::parse("%i").render(&ctx, PatternKind::Text).unwrap_err();
    assert!(matches!(err, PatternError::Counter(_)));
    assert!(err.to_string().contains("disk full"));
    // ... but a pattern without %i is unaffected.
    assert!(Pattern::parse("x").render(&ctx, PatternKind::Text).is_ok());
}

#[test]
fn random_tokens_shape() {
    let f = Fixture::new();
    let s = f.text("%ra{200}");
    assert_eq!(s.chars().count(), 200);
    assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
    let s = f.text("%rn{50}");
    assert!(s.chars().all(|c| c.is_ascii_digit()) && s.len() == 50);
    let s = f.text("%rx{64}");
    assert!(s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    let s = f.text("%rX{64}");
    assert!(s.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')));
    let s = f.text("%rna{200}");
    assert!(s.chars().all(|c| !"01OIl".contains(c) && c.is_ascii_alphanumeric()), "{s}");
    assert_eq!(f.text("%ra").chars().count(), 1);
    assert!(f.text("%remoji{3}").chars().filter(|c| !c.is_ascii()).count() >= 3);
}

#[test]
fn random_words() {
    let f = Fixture::new();
    let a = f.text("%radjective");
    let n = f.text("%ranimal");
    assert!(a.chars().next().unwrap().is_uppercase(), "{a}");
    assert!(n.chars().next().unwrap().is_uppercase(), "{n}");
    assert!(a.chars().all(|c| c.is_ascii_alphabetic()));
}

#[test]
fn randomness_is_deterministic_for_a_seed_and_varies_otherwise() {
    let a = Fixture::new();
    let b = Fixture::new();
    assert_eq!(a.text("%ra{16}-%guid"), b.text("%ra{16}-%guid"));
    let c = Fixture { rng: SeededRng::new(43), ..Fixture::new() };
    assert_ne!(a.text("%ra{16}"), c.text("%ra{16}"));
    // successive calls differ
    let d = Fixture::new();
    assert_ne!(d.text("%ra{16}"), d.text("%ra{16}"));
}

#[test]
fn random_distribution_covers_the_alphabet() {
    let f = Fixture::new();
    let s = f.text("%rn{256}");
    for d in '0'..='9' {
        assert!(s.contains(d), "digit {d} never generated in 256 draws: {s}");
    }
}

#[test]
fn guid_tokens() {
    let f = Fixture::new();
    let g = f.text("%guid");
    assert_eq!(g.len(), 36);
    assert_eq!(g, g.to_lowercase());
    assert_eq!(g.as_bytes()[14], b'4', "version nibble");
    assert!(matches!(g.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "variant");
    let u = f.text("%GUID");
    assert_eq!(u, u.to_uppercase());
    assert_ne!(f.text("%guid"), f.text("%guid"));
}

#[test]
fn random_file_line() {
    let mut f = Fixture::new();
    f.env.files.insert("words.txt".into(), vec![String::new(), "  alpha  ".into(), String::new()]);
    assert_eq!(f.text("%rf{words.txt}"), "alpha");
    f.env.files.insert("empty.txt".into(), vec![String::new(), "  ".into()]);
    let err = Pattern::parse("%rf{empty.txt}").render(&f.ctx(), PatternKind::Text).unwrap_err();
    assert!(matches!(err, PatternError::EmptyRandomFile(_)));
    let err = Pattern::parse("%rf{missing.txt}").render(&f.ctx(), PatternKind::Text).unwrap_err();
    assert!(matches!(err, PatternError::RandomFile { .. }));
}

#[test]
fn newline_only_in_text_kind() {
    let f = Fixture::new();
    assert_eq!(f.text("a%nb"), "a\nb");
    // as a file name '%n' is kept like any unknown token: '%' is legal on all systems
    assert_eq!(f.name("a%nb"), "a%nb");
}

#[test]
fn escaping() {
    let f = Fixture::new();
    assert_eq!(f.text("100%% %y"), "100% 2024");
    assert_eq!(f.text("%%y"), "%y", "escaped percent must not start a token");
    assert_eq!(f.text("50% off"), "50% off");
    assert_eq!(f.text("%"), "%");
    assert_eq!(f.text("%%%%"), "%%");
}

#[test]
fn unknown_token_policies() {
    let mut f = Fixture::new();
    assert_eq!(f.text("a%foo-b%q"), "a%foo-b%q");
    f.options.unknown = UnknownTokens::Remove;
    assert_eq!(f.text("a%foo-b%q"), "a-b");
    f.options.unknown = UnknownTokens::Error;
    let err = Pattern::parse("a%foo").render(&f.ctx(), PatternKind::Text).unwrap_err();
    assert!(matches!(&err, PatternError::UnknownToken(t) if t == "%foo"));
    // invalid parameter counts as unknown
    let err = Pattern::parse("%ra{0}").render(&f.ctx(), PatternKind::Text).unwrap_err();
    assert!(matches!(err, PatternError::UnknownToken(_)));
}

#[test]
fn suspicious_tokens_detected() {
    let hits: Vec<_> = suspicious_tokens("%y%hh%%date-%uid").into_iter().map(|t| t.0).collect();
    assert_eq!(hits, vec!["%hh", "%uid"]);
    assert!(suspicious_tokens("%y-%mo-%d").is_empty());
}

#[test]
fn max_name_length_truncates_on_graphemes() {
    let mut f = Fixture::new();
    f.options.max_name_len = Some(4);
    assert_eq!(f.name("abcdefgh"), "abcd");
    assert_eq!(f.name("🎉🎉🎉🎉🎉🎉"), "🎉🎉🎉🎉");
    f.options.max_name_len = Some(3);
    assert_eq!(f.name("e\u{301}e\u{301}e\u{301}e\u{301}"), "e\u{301}e\u{301}e\u{301}");
    // truncation that leaves a trailing dot is re-sanitised
    f.options.max_name_len = Some(4);
    assert_eq!(f.name("abc.def"), "abc");
    f.options.max_name_len = None;
    assert_eq!(f.name("abcdefgh"), "abcdefgh");
}

#[test]
fn render_file_name_adds_extension_and_respects_limits() {
    let mut f = Fixture::new();
    assert_eq!(render_file_name("shot_%y", "png", &f.ctx()).unwrap(), "shot_2024.png");
    assert_eq!(render_file_name("shot_%y", ".jpg", &f.ctx()).unwrap(), "shot_2024.jpg");
    assert_eq!(render_file_name("shot", "", &f.ctx()).unwrap(), "shot");
    assert_eq!(render_file_name("shot", "p/n g", &f.ctx()).unwrap(), "shot.png");
    // very long stems keep the extension and stay within 255 bytes
    let long = "x".repeat(1000);
    let n = render_file_name(&long, "png", &f.ctx()).unwrap();
    assert_eq!(n.len(), 255);
    assert_eq!(n.rsplit('.').next(), Some("png"));
    // max_name_len applies to the stem only
    f.options.max_name_len = Some(3);
    assert_eq!(render_file_name("abcdef", "png", &f.ctx()).unwrap(), "abc.png");
}

#[test]
fn reserved_and_illegal_names_are_sanitised() {
    let f = Fixture::new();
    assert_eq!(render_file_name("CON", "png", &f.ctx()).unwrap(), "_CON.png");
    assert_eq!(render_file_name("nul", "", &f.ctx()).unwrap(), "_nul");
    assert_eq!(render_file_name("a:b*c?", "png", &f.ctx()).unwrap(), "abc.png");
    assert_eq!(render_file_name("...", "png", &f.ctx()).unwrap(), "file.png");
    assert_eq!(render_file_name("", "png", &f.ctx()).unwrap(), "file.png");
    assert_eq!(render_file_name("trail. ", "png", &f.ctx()).unwrap(), "trail.png");
    assert_eq!(render_file_name("a/b\\c", "png", &f.ctx()).unwrap(), "abc.png");
}

#[test]
fn window_title_that_is_a_reserved_name() {
    let mut f = Fixture::new();
    f.inputs.window_title = Some("CON".into());
    assert_eq!(render_file_name("%t", "png", &f.ctx()).unwrap(), "_CON.png");
}

#[test]
fn unicode_names_survive() {
    let mut f = Fixture::new();
    f.inputs.window_title = Some("日本語のウィンドウ".into());
    assert_eq!(render_file_name("%t_%y", "png", &f.ctx()).unwrap(), "日本語のウィンドウ_2024.png");
}

#[test]
fn folder_patterns() {
    let f = Fixture::new();
    assert_eq!(render_folder("%y-%mo", &f.ctx()).unwrap(), PathBuf::from("2024-03"));
    assert_eq!(render_folder("%y/%mo/%d", &f.ctx()).unwrap(), PathBuf::from("2024/03/09"));
    assert_eq!(render_folder("%y\\%mo", &f.ctx()).unwrap(), PathBuf::from("2024/03"));
    assert_eq!(render_folder("../../%y", &f.ctx()).unwrap(), PathBuf::from("2024"));
    assert_eq!(render_folder("/etc/%y", &f.ctx()).unwrap(), PathBuf::from("etc/2024"));
    assert_eq!(render_folder("", &f.ctx()).unwrap(), PathBuf::new());
    assert_eq!(render_folder("aux/%y", &f.ctx()).unwrap(), PathBuf::from("_aux/2024"));
    let s = Pattern::parse("%y/%mo").render(&f.ctx(), PatternKind::FilePath).unwrap();
    assert_eq!(s, "2024/03");
}

#[test]
fn every_known_token_renders_without_error() {
    let mut f = Fixture::new();
    f.env.files.insert("f".into(), vec!["x".into()]);
    for name in known_token_names() {
        let pattern = match name {
            "ib" | "iB" => format!("%{name}{{16,2}}"),
            "rf" => "%rf{f}".to_owned(),
            n => format!("%{n}"),
        };
        let p = Pattern::parse(&pattern);
        assert!(p.unknown_tokens().is_empty(), "{pattern} is unknown");
        for kind in [PatternKind::Text, PatternKind::FileName, PatternKind::FilePath] {
            assert!(p.render(&f.ctx(), kind).is_ok(), "{pattern} {kind:?}");
        }
    }
}

// ---- file-backed counter ----------------------------------------------------------

#[test]
fn file_counter_persists_across_instances() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state/counter");
    assert_eq!(FileCounter::new(&path).peek().unwrap(), 0);
    assert_eq!(FileCounter::new(&path).next().unwrap(), 1);
    assert_eq!(FileCounter::new(&path).next().unwrap(), 2);
    assert_eq!(FileCounter::new(&path).peek().unwrap(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "2");
}

#[test]
fn file_counter_recovers_from_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("counter");
    std::fs::write(&path, "not a number\u{0}").unwrap();
    assert_eq!(FileCounter::new(&path).next().unwrap(), 1);
    std::fs::write(&path, "").unwrap();
    assert_eq!(FileCounter::new(&path).next().unwrap(), 1);
    std::fs::write(&path, "  41\n").unwrap();
    assert_eq!(FileCounter::new(&path).next().unwrap(), 42);
}

#[test]
fn file_counter_shorter_value_does_not_leave_stale_digits() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("counter");
    std::fs::write(&path, "99999").unwrap();
    std::fs::write(&path, "5").unwrap();
    assert_eq!(FileCounter::new(&path).next().unwrap(), 6);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "6");
}

#[test]
fn file_counter_threads_never_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    let path = Arc::new(tmp.path().join("counter"));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = Arc::clone(&path);
            thread::spawn(move || {
                // each thread has its own handle, like separate processes would
                let c = FileCounter::new(path.as_path());
                (0..50).map(|_| c.next().unwrap()).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut all: Vec<u64> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    all.sort_unstable();
    let expected: Vec<u64> = (1..=400).collect();
    assert_eq!(all, expected, "every value 1..=400 handed out exactly once");
}

#[test]
fn file_counter_unwritable_location_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"").unwrap();
    assert!(FileCounter::new(blocker.join("counter")).next().is_err());
}

#[test]
fn end_to_end_unique_files_with_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let f = Fixture { counter: MemoryCounter::default(), ..Fixture::new() };
    for expected in ["img_001.png", "img_002.png"] {
        let name = render_file_name("img_%i{3}", "png", &f.ctx()).unwrap();
        let p = write_unique(tmp.path(), &name, b"x").unwrap();
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), expected);
    }
    // a pattern without a counter collides and falls back to "(2)"
    let name = render_file_name("fixed", "png", &f.ctx()).unwrap();
    write_unique(tmp.path(), &name, b"x").unwrap();
    let p = write_unique(tmp.path(), &name, b"x").unwrap();
    assert_eq!(p.file_name().unwrap().to_str().unwrap(), "fixed (2).png");
}

#[test]
fn system_context_smoke() {
    let clock = SystemClock;
    let rng = SystemRng::new();
    let env = SystemEnv;
    let counter = MemoryCounter::default();
    let inputs = NameInputs::default();
    let ctx = PatternContext {
        clock: &clock,
        rng: &rng,
        env: &env,
        counter: &counter,
        inputs: &inputs,
        options: RenderOptions::default(),
    };
    let n = render_file_name("%y%mo%d_%un_%cn_%ra{6}", "png", &ctx).unwrap();
    assert!(is_valid_file_name(&n, 255), "{n}");
    assert_ne!(rng.next_u64(), rng.next_u64());
}

mod property {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// Whatever the pattern text, title and extension, the result is a valid file name.
        #[test]
        fn rendered_names_are_always_valid(
            pattern in "[ -~%{}éあ👍]{0,80}",
            title in "\\PC{0,60}",
            ext in "[ -~]{0,6}",
        ) {
            let mut f = Fixture::new();
            f.inputs.window_title = Some(title);
            let name = render_file_name(&pattern, &ext, &f.ctx()).unwrap();
            prop_assert!(is_valid_file_name(&name, 255), "{:?} -> {:?}", pattern, name);
        }

        #[test]
        fn rendered_folders_never_escape(pattern in "[ -~%{}]{0,60}") {
            let f = Fixture::new();
            let p = render_folder(&pattern, &f.ctx()).unwrap();
            prop_assert!(!p.is_absolute());
            for c in p.components() {
                prop_assert!(matches!(c, std::path::Component::Normal(_)));
            }
        }

        #[test]
        fn parse_and_render_never_panic(pattern in "\\PC{0,80}") {
            let f = Fixture::new();
            let p = Pattern::parse(&pattern);
            for kind in [PatternKind::Text, PatternKind::FileName, PatternKind::FilePath] {
                let _ = p.render(&f.ctx(), kind);
            }
        }

        #[test]
        fn random_length_matches_request(n in 1usize..=256) {
            let f = Fixture::new();
            let s = Pattern::parse(&format!("%ra{{{n}}}")).render(&f.ctx(), PatternKind::Text).unwrap();
            prop_assert_eq!(s.len(), n);
        }
    }
}
