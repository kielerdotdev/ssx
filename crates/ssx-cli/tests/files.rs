//! End to end: everything that starts from files (`post-file`, `upload`, `post-video`), with
//! hostile file names, folders, forwarding to a running instance, and failure paths. Needs no
//! display: uploads go to a local mock server through a generic HTTP `PUT` uploader.
#![cfg(unix)]

mod common;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use common::{TestEnv, mock::HttpMock};
use ssx_core::ipc::{
    ErrorCode, PostAction, Request, RequestEnvelope, Response, ResponseEnvelope, decode_line,
    encode_line,
};
use ssx_ipc::{Acquired, Instance, Location};

/// Settings that send every kind of content to a `PUT {url}/f/{filename}` uploader.
fn http_settings(url: &str) -> String {
    format!(
        "[destinations]\nimage = \"web\"\nfile = \"web\"\nvideo = \"web\"\ntext = \"web\"\n\n\
         [uploaders.web]\ntype = \"http\"\nurl = \"{url}/f/{{filename}}\"\n"
    )
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && b.len() >= i + 3
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(decoded file name, body)` of every upload the server received, sorted by name.
fn uploads(mock: &HttpMock) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = mock
        .requests()
        .into_iter()
        .map(|r| {
            let path = percent_decode(r.url.path());
            (path.strip_prefix("/f/").unwrap_or(&path).to_owned(), r.body)
        })
        .collect();
    v.sort();
    v
}

const HOSTILE: &[&str] = &[
    "plain.png",
    "with space.txt",
    "--dashes.txt",
    "quote'single\"double.txt",
    "$(touch PWNED).txt",
    "`touch PWNED`.txt",
    "a;b&c|d.txt",
    "new\nline.txt",
    "unicode-\u{e9}\u{65e5}\u{672c}-\u{1f600}.txt",
    "percent%20literal.txt",
    "#hash?query=1.txt",
];

fn write_files(dir: &Path, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let p = dir.join(n);
            std::fs::write(&p, format!("content #{i} of {n}")).expect("write hostile file");
            p
        })
        .collect()
}

#[test]
fn post_file_uploads_every_file_including_hostile_names() {
    let mock = HttpMock::start();
    mock.respond_to_all(200, "");
    let env = TestEnv::new();
    env.write_settings(&http_settings(&mock.url()));
    let work = env.path("files");
    std::fs::create_dir(&work).unwrap();
    let files = write_files(&work, HOSTILE);

    let mut args: Vec<&str> = vec!["post-file", "--"];
    args.extend(files.iter().map(|p| p.to_str().unwrap()));
    let r = env.ssx(&args).ok();

    // One URL per file on stdout, in input order.
    assert_eq!(r.lines().len(), HOSTILE.len(), "{}", r.stdout);
    for (line, name) in r.lines().iter().zip(HOSTILE) {
        assert!(line.starts_with(&mock.url()), "{line}");
        assert_eq!(
            percent_decode(line).strip_prefix(&format!("{}/f/", mock.url())),
            Some(*name),
            "{line}"
        );
    }
    // The server received every file, under the exact name, with the exact bytes.
    let got = uploads(&mock);
    let mut want: Vec<(String, Vec<u8>)> = HOSTILE
        .iter()
        .enumerate()
        .map(|(i, n)| ((*n).to_owned(), format!("content #{i} of {n}").into_bytes()))
        .collect();
    want.sort();
    assert_eq!(got, want);
    // Nothing was interpreted by a shell.
    assert!(!work.join("PWNED").exists() && !env.path("PWNED").exists());
    assert!(!std::env::current_dir().unwrap().join("PWNED").exists());
    // History has one entry per file.
    assert_eq!(
        env.ssx(&["history", "list", "--json", "-n", "50"]).ok().json().as_array().unwrap().len(),
        HOSTILE.len()
    );
}

#[test]
fn a_failing_file_does_not_stop_the_others_and_fails_the_command() {
    let mock = HttpMock::start();
    mock.respond_to_all(200, "");
    let env = TestEnv::new();
    env.write_settings(&http_settings(&mock.url()));
    let work = env.path("files");
    std::fs::create_dir(&work).unwrap();
    let good = write_files(&work, &["a.txt", "b.txt"]);
    let missing = work.join("missing.txt");

    let r = env
        .ssx(&[
            "upload",
            good[0].to_str().unwrap(),
            missing.to_str().unwrap(),
            good[1].to_str().unwrap(),
        ])
        .code(1);
    assert_eq!(r.lines().len(), 2, "the two good files still produced URLs: {}", r.stdout);
    assert!(r.stderr.contains("error:") && r.stderr.contains("missing.txt"), "{}", r.stderr);
    assert_eq!(uploads(&mock).len(), 2);
}

#[test]
fn upload_prints_bare_urls_or_json_and_honours_to() {
    let mock = HttpMock::start();
    mock.respond_to_all(200, "");
    let env = TestEnv::new(); // no settings at all: --to picks the uploader
    env.write_settings(&format!(
        "[uploaders.web]\ntype = \"http\"\nurl = \"{}/f/{{filename}}\"\n",
        mock.url()
    ));
    let work = env.path("files");
    std::fs::create_dir(&work).unwrap();
    let files = write_files(&work, &["one.txt", "two.png"]);

    let r = env
        .ssx(&["upload", "--to", "web", files[0].to_str().unwrap(), files[1].to_str().unwrap()])
        .ok();
    assert_eq!(
        r.lines(),
        [format!("{}/f/one.txt", mock.url()), format!("{}/f/two.png", mock.url())]
    );
    assert!(
        r.stderr.lines().all(|l| l.trim().is_empty() || l.contains("upload")),
        "quiet apart from progress: {}",
        r.stderr
    );

    let j = env.ssx(&["upload", "--to", "web", "--json", files[0].to_str().unwrap()]).ok().json();
    assert_eq!(j["outcome"], "success");
    assert_eq!(j["items"][0]["url"], format!("{}/f/one.txt", mock.url()));
    assert_eq!(j["items"][0]["uploader"], "web");

    // Without a destination the error says what to configure.
    let r = env.ssx(&["upload", files[0].to_str().unwrap()]).code(1);
    assert!(
        r.stderr.contains("no file uploader is configured")
            && r.stderr.contains("destinations.file"),
        "{}",
        r.stderr
    );
}

#[test]
fn folders_are_zipped_and_symlinks_out_of_them_are_refused() {
    use std::os::unix::fs::symlink;
    let mock = HttpMock::start();
    mock.respond_to_all(200, "");
    let env = TestEnv::new();
    env.write_settings(&http_settings(&mock.url()));
    let folder = env.path("my folder");
    std::fs::create_dir_all(folder.join("sub")).unwrap();
    std::fs::write(folder.join("a.txt"), "alpha").unwrap();
    std::fs::write(folder.join("sub/b.txt"), "bravo").unwrap();

    let r = env.ssx(&["upload", folder.to_str().unwrap()]).ok();
    assert_eq!(r.lines(), [format!("{}/f/my%20folder.zip", mock.url())]);
    let got = uploads(&mock);
    assert_eq!(got[0].0, "my folder.zip");
    assert!(got[0].1.starts_with(b"PK"), "a real zip archive was uploaded");
    assert!(String::from_utf8_lossy(&got[0].1).contains("my folder/sub/b.txt"));

    // A link to a file outside the folder must not be followed.
    let secret = env.path("secret.txt");
    std::fs::write(&secret, "PRIVATE").unwrap();
    symlink(&secret, folder.join("leak")).unwrap();
    let before = mock.requests().len();
    let r = env.ssx(&["upload", folder.to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("refusing") && r.stderr.contains("outside"), "{}", r.stderr);
    assert_eq!(mock.requests().len(), before, "nothing was uploaded");
}

#[test]
fn post_video_and_kind_pick_the_right_destination() {
    let mock = HttpMock::start();
    mock.respond_to_all(200, "");
    let env = TestEnv::new();
    env.write_settings(&format!(
        "[destinations]\nvideo = \"vid\"\nfile = \"other\"\nimage = \"other\"\n\n\
         [uploaders.vid]\ntype = \"http\"\nurl = \"{u}/video/{{filename}}\"\n\n\
         [uploaders.other]\ntype = \"http\"\nurl = \"{u}/other/{{filename}}\"\n",
        u = mock.url()
    ));
    let work = env.path("files");
    std::fs::create_dir(&work).unwrap();
    let files = write_files(&work, &["clip.mp4", "note.txt"]);

    let r = env.ssx(&["post-video", files[0].to_str().unwrap()]).ok();
    assert_eq!(r.lines(), [format!("{}/video/clip.mp4", mock.url())]);
    assert!(files[0].exists(), "a video the user picked is never deleted");

    // --kind video sends even a .txt through the video destination.
    let r = env.ssx(&["post-file", "--kind", "video", "--", files[1].to_str().unwrap()]).ok();
    assert_eq!(r.lines(), [format!("{}/video/note.txt", mock.url())]);
    // Without --kind the extension decides.
    let r = env.ssx(&["post-file", "--", files[1].to_str().unwrap()]).ok();
    assert_eq!(r.lines(), [format!("{}/other/note.txt", mock.url())]);
    let r = env.ssx(&["post-file", "--kind", "text", "--", files[1].to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("no text uploader"), "{}", r.stderr);
}

/// A fake tray instance: records every request line and answers with `reply`.
struct FakeInstance {
    // Field order matters: the server must shut down (it wakes its accept loop by connecting to
    // the socket) *before* the temp directory holding that socket is deleted.
    _handle: ssx_ipc::ServeHandle,
    _tmp: tempfile::TempDir,
    runtime_dir: PathBuf,
    lines: Arc<Mutex<Vec<String>>>,
}

fn fake_instance(reply: Response) -> FakeInstance {
    // Unix socket paths are short: keep the runtime dir directly under the temp root.
    let tmp = tempfile::Builder::new().prefix("ssxrt").tempdir().unwrap();
    let runtime_dir = tmp.path().to_path_buf();
    let Acquired::Primary(server) =
        Instance::acquire_in(&Location::in_dir(runtime_dir.join("ssx")), "ssx").unwrap()
    else {
        panic!("expected to become the primary instance");
    };
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = lines.clone();
    let handle = server
        .serve(move |line| {
            seen.lock().unwrap().push(line);
            encode_line(&ResponseEnvelope::new(1, reply.clone())).unwrap().trim_end().to_owned()
        })
        .unwrap();
    FakeInstance { _handle: handle, _tmp: tmp, runtime_dir, lines }
}

#[test]
fn coalesce_hands_the_paths_to_the_running_instance_verbatim() {
    let inst = fake_instance(Response::Accepted { run_id: 1 });
    let mut env = TestEnv::new();
    env.set("XDG_RUNTIME_DIR", inst.runtime_dir.display().to_string());
    let work = env.path("files");
    std::fs::create_dir(&work).unwrap();
    let files = write_files(&work, HOSTILE);

    let mut args: Vec<&str> = vec!["post-file", "--coalesce", "--"];
    args.extend(files.iter().map(|p| p.to_str().unwrap()));
    let r = env.ssx(&args).ok();
    assert!(r.stdout.is_empty(), "nothing is uploaded here: {}", r.stdout);

    let lines = inst.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "one request for the whole batch");
    let env_req: RequestEnvelope = decode_line(&lines[0]).unwrap();
    let Request::PostFiles { paths, action, wait } = env_req.request else {
        panic!("not post_files: {lines:?}")
    };
    assert_eq!(action, PostAction::Upload);
    assert!(!wait, "shell shims must not block on the upload");
    assert_eq!(paths, files, "absolute paths, unchanged, in order, hostile names included");
}

#[test]
fn coalesce_falls_back_to_uploading_here_when_nobody_listens_or_it_refuses() {
    let mock = HttpMock::start();
    mock.respond_to_all(200, "");
    let mut env = TestEnv::new();
    env.write_settings(&http_settings(&mock.url()));
    let work = env.path("files");
    std::fs::create_dir(&work).unwrap();
    let files = write_files(&work, &["a.txt"]);

    // No instance: an empty runtime dir has no socket.
    let empty = tempfile::Builder::new().prefix("ssxrt").tempdir().unwrap();
    env.set("XDG_RUNTIME_DIR", empty.path().display().to_string());
    let r = env.ssx(&["post-file", "--coalesce", "--", files[0].to_str().unwrap()]).ok();
    assert_eq!(r.lines(), [format!("{}/f/a.txt", mock.url())]);

    // An instance that answers with an error: warn and do the work here.
    let busy = fake_instance(Response::error(ErrorCode::Busy, "another run is active"));
    env.set("XDG_RUNTIME_DIR", busy.runtime_dir.display().to_string());
    let r = env.ssx(&["post-file", "--coalesce", "--", files[0].to_str().unwrap()]).ok();
    assert_eq!(r.lines().len(), 1);
    assert!(r.stderr.contains("another run is active"), "{}", r.stderr);
    assert_eq!(busy.lines.lock().unwrap().len(), 1, "it was asked first");

    // --kind / --to cannot be expressed on the wire, so they never forward.
    let before = busy.lines.lock().unwrap().len();
    env.ssx(&["post-file", "--coalesce", "--to", "web", "--", files[0].to_str().unwrap()]).ok();
    assert_eq!(busy.lines.lock().unwrap().len(), before);
}

#[test]
fn an_unreachable_server_is_a_clean_retryable_error() {
    let env = TestEnv::new();
    env.write_settings(
        "[uploaders.dead]\ntype = \"http\"\nurl = \"http://127.0.0.1:9/f/{filename}\"\n",
    );
    let f = env.path("a.txt");
    std::fs::write(&f, "x").unwrap();
    let started = std::time::Instant::now();
    let r = env.ssx(&["upload", "--to", "dead", f.to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("error:") && r.stderr.contains("dead:"), "{}", r.stderr);
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "retries are bounded");
    assert!(f.exists());
}

#[test]
fn usage_errors_exit_2() {
    let env = TestEnv::new();
    for args in [
        &["upload"][..],
        &["post-file"],
        &["capture"],
        &["capture", "region", "--rect", "1,2,3"],
        &["nonsense"],
        &["--backend", "nonsense", "monitors"],
    ] {
        let r = env.ssx(args);
        assert_eq!(r.code, 2, "{args:?}: {}", r.stderr);
    }
    assert_eq!(
        env.ssx(&["--version"]).ok().stdout.trim(),
        format!("ssx {}", env!("CARGO_PKG_VERSION"))
    );
}
