//! End to end under a private Xvfb: real `ssx` binary, real X11 capture, known pixels.
#![cfg(all(unix, not(target_os = "macos")))]

mod common;

use common::{
    TestEnv, first_diff, have,
    mock::HttpMock,
    read_image, rgb, sxcu_json,
    x11::{ROOT_BG, expected_scene, scene_server},
};
use ssx_types::Frame;

fn rgb_of(c: u32) -> [u8; 3] {
    [(c >> 16) as u8, (c >> 8) as u8, c as u8]
}

#[test]
fn fullscreen_capture_is_pixel_exact() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let out = env.path("full.png");
    let r = env.ssx(&["capture", "fullscreen", "-o", out.to_str().unwrap()]).ok();
    assert_eq!(r.lines(), [out.to_str().unwrap()], "the saved path is printed on stdout");

    let img = read_image(&out);
    assert_eq!((img.width(), img.height()), (800, 600));
    assert_eq!(first_diff(&img, &expected_scene(800, 600, 0, 0)), None);
    assert!(std::fs::read(&out).unwrap().starts_with(b"\x89PNG"));
}

#[test]
fn rect_capture_crops_exactly_and_is_remembered_as_the_last_region() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let out = env.path("crop.png");
    // Covers parts of the red, blue and background areas.
    env.ssx(&["capture", "region", "--rect", "50,20,130,70", "-o", out.to_str().unwrap()]).ok();
    let img = read_image(&out);
    assert_eq!((img.width(), img.height()), (130, 70));
    assert_eq!(first_diff(&img, &expected_scene(130, 70, 50, 20)), None);

    // `last-region` repeats it without any selection.
    let again = env.path("again.png");
    env.ssx(&["capture", "last-region", "-o", again.to_str().unwrap()]).ok();
    let img2 = read_image(&again);
    assert_eq!((img2.width(), img2.height()), (130, 70));
    assert_eq!(first_diff(&img2, &expected_scene(130, 70, 50, 20)), None);
    assert!(env.cfg.join("data/last_region.json").exists(), "persisted in the data dir");
}

#[test]
fn last_region_without_history_and_interactive_region_explain_themselves() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let r = env.ssx(&["capture", "last-region", "-o", env.path("x.png").to_str().unwrap()]).code(1);
    assert!(
        r.stderr.contains("error:") && r.stderr.contains("no region has been captured yet"),
        "{}",
        r.stderr
    );

    let r = env.ssx(&["capture", "region", "-o", env.path("y.png").to_str().unwrap()]).code(1);
    assert!(r.stderr.contains("--rect") && r.stderr.contains("overlay"), "{}", r.stderr);
    assert!(!env.path("y.png").exists());
}

#[test]
fn format_flag_and_extension_choose_the_encoding() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);

    let jpg = env.path("shot.jpg");
    env.ssx(&["capture", "fullscreen", "-o", jpg.to_str().unwrap()]).ok();
    let bytes = std::fs::read(&jpg).unwrap();
    assert!(bytes.starts_with(&[0xff, 0xd8, 0xff]), "JPEG magic");
    let img = read_image(&jpg);
    let near = |got: [u8; 3], want: [u8; 3]| got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 8);
    assert!(
        near(rgb(&img, 20, 15), rgb_of(0x00ff_0000)),
        "red survives JPEG: {:?}",
        rgb(&img, 20, 15)
    );
    assert!(near(rgb(&img, 700, 500), rgb_of(ROOT_BG)));

    // --format wins over a missing extension, which is appended.
    let stem = env.path("noext");
    env.ssx(&["capture", "fullscreen", "--format", "jpg", "-o", stem.to_str().unwrap()]).ok();
    assert!(env.path("noext.jpg").exists());
    let webp = env.path("shot.webp");
    env.ssx(&["capture", "fullscreen", "-o", webp.to_str().unwrap()]).ok();
    assert!(std::fs::read(&webp).unwrap().starts_with(b"RIFF"));
    assert_eq!(
        first_diff(&read_image(&webp), &expected_scene(800, 600, 0, 0)),
        None,
        "WebP is lossless here"
    );

    let r =
        env.ssx(&["capture", "fullscreen", "-o", env.path("bad.xyz").to_str().unwrap()]).code(2);
    assert!(
        r.stderr.contains("cannot tell the image format") && r.stderr.contains("--format"),
        "{}",
        r.stderr
    );
}

#[test]
fn without_output_the_image_lands_in_the_configured_folder_and_history() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let save = env.path("shots");
    env.write_settings(&format!(
        "[general]\nsave_dir = {save:?}\nuse_type_subfolders = false\nfolder_pattern = \"\"\nfile_name_pattern = \"shot_%i\"\n"
    ));
    let r = env.ssx(&["capture", "fullscreen"]).ok();
    let printed = r.lines()[0].to_owned();
    assert!(
        printed.ends_with("shot_1.png") && printed.starts_with(save.to_str().unwrap()),
        "{printed}"
    );
    assert_eq!(
        first_diff(&read_image(std::path::Path::new(&printed)), &expected_scene(800, 600, 0, 0)),
        None
    );

    let h = env.ssx(&["history", "list", "--json"]).ok().json();
    assert_eq!(h.as_array().unwrap().len(), 1);
    assert_eq!(h[0]["local_path"], printed);
    assert_eq!(h[0]["kind"], "image");
    assert_eq!((h[0]["width"].as_u64(), h[0]["height"].as_u64()), (Some(800), Some(600)));

    // A second capture takes the next counter value: nothing is overwritten.
    let second = env.ssx(&["capture", "fullscreen", "--format", "jpg"]).ok();
    assert!(second.lines()[0].ends_with("shot_2.jpg"), "{:?}", second.lines());
}

#[test]
fn capture_json_describes_the_result() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let out = env.path("j.png");
    let j = env.ssx(&["capture", "fullscreen", "-o", out.to_str().unwrap(), "--json"]).ok().json();
    assert_eq!(j["outcome"], "success");
    assert_eq!(j["items"][0]["path"], out.to_str().unwrap());
}

#[test]
fn monitors_json_schema() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let j = env.ssx(&["monitors", "--json"]).ok().json();
    let m = &j.as_array().expect("an array")[0];
    assert!(m["id"].is_string() && m["name"].is_string());
    assert_eq!(m["rect"], serde_json::json!({"x": 0, "y": 0, "width": 800, "height": 600}));
    assert!((m["scale_factor"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    assert_eq!(m["primary"], true);
    assert!(
        m.get("refresh_hz").is_some() && m.get("hdr").is_some(),
        "keys are present even when null: {m}"
    );

    let table = env.ssx(&["monitors"]).ok();
    assert!(
        table.stdout.starts_with("ID") && table.stdout.contains("0,0 800x600"),
        "{}",
        table.stdout
    );
}

#[test]
fn windows_lists_something_or_explains() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let r = env.ssx(&["windows", "--json"]);
    // Without a window manager there is no client list; both outcomes must be well-formed.
    if r.code == 0 {
        assert!(r.json().is_array());
    } else {
        assert!(r.stderr.starts_with("error:"), "{}", r.stderr);
    }
    let r = env.ssx(&[
        "capture",
        "window",
        "--id",
        "0xdeadbeef",
        "-o",
        env.path("w.png").to_str().unwrap(),
    ]);
    assert_eq!(r.code, 1, "an unknown window id fails cleanly");
    assert!(r.stderr.contains("error:"), "{}", r.stderr);
}

#[test]
fn capture_and_upload_prints_the_url_and_records_history() {
    let Some((x, _painter)) = scene_server() else { return };
    let mock = HttpMock::start();
    mock.respond_to_all(
        200,
        r#"{"link":"https://cdn.example/i/abc.png","delete":"https://cdn.example/del/abc"}"#,
    );
    let env = TestEnv::new().with_x11(&x.display);

    // Import the ShareX uploader through the CLI (this exercises `uploaders import` too).
    let sxcu = env.path("mock host.sxcu");
    std::fs::write(&sxcu, sxcu_json(&mock.url())).unwrap();
    let imp = env.ssx(&["uploaders", "import", sxcu.to_str().unwrap(), "--name", "mock"]).ok();
    assert!(imp.stdout.contains("imported") && imp.stdout.contains("mock"), "{}", imp.stdout);
    assert!(env.cfg.join("uploaders/mock.sxcu").exists());
    let list = env.ssx(&["uploaders", "list", "--json"]).ok().json();
    assert!(
        list.as_array().unwrap().iter().any(|u| u["name"] == "mock" && u["kind"] == "sxcu"),
        "{list}"
    );

    let save = env.path("shots");
    env.write_settings(&format!(
        "[general]\nsave_dir = {save:?}\nuse_type_subfolders = false\nfolder_pattern = \"\"\n\n[destinations]\nimage = \"mock\"\n"
    ));
    let r = env.ssx(&["capture", "fullscreen", "--upload"]).ok();
    assert_eq!(
        r.lines(),
        ["https://cdn.example/i/abc.png"],
        "stdout carries only the URL: {}",
        r.stdout
    );
    assert!(r.stderr.contains("saved:"), "the saved path goes to stderr: {}", r.stderr);

    // The server received the real screenshot.
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let body = &reqs[0].body;
    let start =
        body.windows(4).position(|w| w == b"\x89PNG").expect("a PNG part in the multipart body");
    let uploaded = Frame::decode(&body[start..]).expect("valid PNG");
    assert_eq!(
        first_diff(&uploaded, &expected_scene(800, 600, 0, 0)),
        None,
        "the uploaded pixels are the screenshot"
    );

    // History has the file and the URLs.
    let h = env.ssx(&["history", "list", "--json"]).ok().json();
    assert_eq!(h.as_array().unwrap().len(), 1, "{h}");
    assert_eq!(h[0]["upload_url"], "https://cdn.example/i/abc.png");
    assert_eq!(h[0]["deletion_url"], "https://cdn.example/del/abc");
    assert_eq!(h[0]["uploader"], "mock");
    let path = h[0]["local_path"].as_str().unwrap();
    assert!(std::path::Path::new(path).exists());
    let id = h[0]["id"].as_i64().unwrap().to_string();
    let shown = env.ssx(&["history", "show", &id]).ok();
    assert!(shown.stdout.contains("https://cdn.example/i/abc.png"), "{}", shown.stdout);
    assert_eq!(
        env.ssx(&["history", "search", "abc"]).ok().stdout.lines().count(),
        2,
        "header + the entry"
    );
}

#[test]
fn upload_with_output_path_and_explicit_destination() {
    let Some((x, _painter)) = scene_server() else { return };
    let mock = HttpMock::start();
    mock.respond_to_all(200, r#"{"link":"https://cdn.example/o.png"}"#);
    let env = TestEnv::new().with_x11(&x.display);
    let sxcu = env.path("h.sxcu");
    std::fs::write(&sxcu, sxcu_json(&mock.url())).unwrap();
    env.ssx(&["uploaders", "import", sxcu.to_str().unwrap(), "--name", "mock"]).ok();

    let out = env.path("kept.png");
    let r = env
        .ssx(&[
            "capture",
            "region",
            "--rect",
            "0,0,300,200",
            "-o",
            out.to_str().unwrap(),
            "--to",
            "mock",
        ])
        .ok();
    assert_eq!(r.lines(), ["https://cdn.example/o.png"]);
    assert!(out.exists(), "the explicit output file is kept");
    let h = env.ssx(&["history", "list", "--json"]).ok().json();
    assert_eq!(h[0]["local_path"], out.to_str().unwrap(), "history knows the file: {h}");
}

#[test]
fn a_failing_upload_keeps_the_file_and_exits_1() {
    let Some((x, _painter)) = scene_server() else { return };
    let mock = HttpMock::start();
    mock.respond_to_all(404, "nope");
    let env = TestEnv::new().with_x11(&x.display);
    let sxcu = env.path("h.sxcu");
    std::fs::write(&sxcu, sxcu_json(&mock.url())).unwrap();
    env.ssx(&["uploaders", "import", sxcu.to_str().unwrap(), "--name", "mock"]).ok();
    let out = env.path("kept.png");
    let r =
        env.ssx(&["capture", "fullscreen", "-o", out.to_str().unwrap(), "--to", "mock"]).code(1);
    assert!(r.stdout.trim().is_empty(), "no URL on stdout: {}", r.stdout);
    assert!(r.stderr.contains("error:") && r.stderr.contains("404"), "{}", r.stderr);
    assert!(out.exists(), "a failed upload keeps the local file");
}

#[test]
fn copy_puts_the_image_on_the_x_clipboard() {
    if !have("xclip") {
        eprintln!("SKIP: xclip is not installed");
        return;
    }
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let out = env.path("c.png");
    env.ssx(&["capture", "fullscreen", "-o", out.to_str().unwrap(), "--copy"]).ok();
    // xclip forked into the background and keeps serving the selection after ssx exited.
    let got = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/png", "-o"])
        .env("DISPLAY", &x.display)
        .output()
        .expect("run xclip");
    assert!(got.status.success(), "{}", String::from_utf8_lossy(&got.stderr));
    let img = Frame::decode(&got.stdout).expect("the clipboard holds a PNG");
    assert_eq!(first_diff(&img, &expected_scene(800, 600, 0, 0)), None);
}

#[test]
fn verbose_flags_and_rust_log_control_logging() {
    let Some((x, _painter)) = scene_server() else { return };
    let mut env = TestEnv::new().with_x11(&x.display);
    let out = env.path("v.png");
    let quiet = env.ssx(&["capture", "fullscreen", "-o", out.to_str().unwrap()]).ok();
    assert!(
        !quiet.stderr.contains("capture backend selected"),
        "quiet by default: {}",
        quiet.stderr
    );
    let v = env.ssx(&["-v", "capture", "fullscreen", "-o", out.to_str().unwrap()]).ok();
    assert!(
        v.stderr.contains("capture backend selected") && v.stderr.contains("INFO"),
        "-v shows info logs: {}",
        v.stderr
    );
    env.set("RUST_LOG", "ssx_platform=debug");
    let d = env.ssx(&["capture", "fullscreen", "-o", out.to_str().unwrap()]).ok();
    assert!(
        d.stderr.contains("capture backend selected"),
        "RUST_LOG overrides the flags: {}",
        d.stderr
    );
    let q = env.ssx(&["-q", "capture", "fullscreen", "-o", out.to_str().unwrap()]).ok();
    assert!(q.stderr.contains("capture backend selected"), "RUST_LOG wins over -q too");
}

#[test]
fn ctrl_c_during_the_delay_cancels_with_exit_code_3() {
    let Some((x, _painter)) = scene_server() else { return };
    let env = TestEnv::new().with_x11(&x.display);
    let child = env
        .command()
        .args([
            "capture",
            "fullscreen",
            "--delay",
            "30000",
            "-o",
            env.path("never.png").to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    std::thread::sleep(std::time::Duration::from_millis(600));
    let kill = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(kill.success());
    let started = std::time::Instant::now();
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "cancellation is prompt");
    assert!(!env.path("never.png").exists());
}
