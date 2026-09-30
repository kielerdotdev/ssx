//! Demo data for screenshots and tests: a believable history with procedurally drawn
//! thumbnails, a few uploaders, and file managers that look installed.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::DateTime;
use image::{Rgba, RgbaImage};
use ssx_core::{
    history::{EntryKind, History, NewEntry, ThumbnailOptions, thumbnail_from_image},
    settings::Settings,
};
use ssx_settings_ui::{
    host::{FixedDoctor, Host, SharedHistory},
    pages::integration::sample_report,
};

/// A 160x100 picture that looks like a screenshot of *something*, different for each seed.
pub fn picture(seed: u32) -> RgbaImage {
    let hue = |s: u32, k: u32| (40 + (s * 53 + k * 91) % 180) as u8;
    let (r, g, b) = (hue(seed, 1), hue(seed, 2), hue(seed, 3));
    let mut img = RgbaImage::from_pixel(160, 100, Rgba([236, 238, 242, 255]));
    for y in 0..14 {
        for x in 0..160 {
            img.put_pixel(x, y, Rgba([r / 2, g / 2, b / 2, 255]));
        }
    }
    for y in 24..90 {
        for x in 8..(30 + (seed * 7 % 40)) {
            img.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }
    for i in 0..5 {
        let y = 26 + i * 12;
        let w = 60 + ((seed + i) * 17) % 70;
        for yy in y..y + 5 {
            for x in 50..(50 + w).min(155) {
                img.put_pixel(x, yy, Rgba([90 + i as u8 * 10, 96, 110, 255]));
            }
        }
    }
    img
}

fn ms(rfc: &str) -> i64 {
    DateTime::parse_from_rfc3339(rfc).unwrap().timestamp_millis()
}

/// 40 entries around 2025-03-09 14:05 (+01:00). Files that "exist" are real, in `files_dir`.
pub fn demo_history(files_dir: &Path) -> Arc<History> {
    let db = Arc::new(History::open_in_memory().unwrap());
    fill_history(&db, files_dir);
    db
}

/// The same history in a database file (for the real binary).
pub fn demo_history_file(files_dir: &Path, db_path: &Path) {
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = History::open(db_path).unwrap();
    fill_history(&db, files_dir);
}

/// Writes the 40 demo entries into `db`.
pub fn fill_history(db: &History, files_dir: &Path) {
    std::fs::create_dir_all(files_dir).unwrap();
    let base = ms("2025-03-09T14:00:00+01:00");
    for i in 0..40i64 {
        let kind = match i % 9 {
            4 => EntryKind::Video,
            6 => EntryKind::File,
            7 => EntryKind::Text,
            _ => EntryKind::Image,
        };
        let mut e = NewEntry::new(kind);
        e.created_at = base - i * 47 * 60_000 - (i / 6) * 3 * 3_600_000;
        let ext = match kind {
            EntryKind::Image => "png",
            EntryKind::Video => "mp4",
            EntryKind::Text => "txt",
            _ => "zip",
        };
        let name = format!(
            "Screenshot_2025-03-{:02}_{:02}-{:02}-00.{ext}",
            9 - (i / 12),
            14 - (i % 12),
            (i * 7) % 60
        );
        let path: PathBuf = files_dir.join(&name);
        // every 5th entry has lost its file
        if i % 5 != 3 {
            std::fs::write(&path, b"demo").unwrap();
        }
        e.local_path = Some(path);
        e.size_bytes = Some(48_000 + (i as u64 * 91_337) % 2_400_000);
        if kind == EntryKind::Image {
            e.width = Some(1920);
            e.height = Some(1080);
            // every 7th image has no stored thumbnail (falls back to the file / a placeholder)
            if i % 7 != 5 {
                e.thumbnail = Some(
                    thumbnail_from_image(&picture(i as u32), ThumbnailOptions::default()).unwrap(),
                );
            }
        }
        if i % 2 == 0 {
            let up = if i % 4 == 0 { "imgur" } else { "my-s3" };
            e.uploader = Some(up.to_owned());
            e.upload_url = Some(if up == "imgur" {
                format!("https://i.imgur.com/k{i:03}Ab.png")
            } else {
                format!("https://cdn.example.com/shots/{i:03}.{ext}")
            });
            if up == "imgur" {
                e.deletion_url = Some(format!("https://imgur.com/delete/{i:03}xyz"));
            }
        }
        e.window_title = Some(
            ["Firefox", "Terminal", "Settings", "Slack", "Figma"][(i % 5) as usize].to_owned(),
        );
        e.process_name = Some(
            ["firefox", "foot", "gnome-control-center", "slack", "figma"][(i % 5) as usize]
                .to_owned(),
        );
        e.workflow_id =
            Some(if i % 3 == 0 { "capture-region" } else { "capture-fullscreen" }.to_owned());
        db.insert(&e).unwrap();
    }
}

/// Settings with a couple of uploaders and a few edits, like a lived-in install.
pub fn lived_in_settings() -> Settings {
    let mut s = Settings::default();
    s.uploaders.insert(
        "imgur".into(),
        "type = 'imgur'\nclient_id = 'a1b2c3d4e5f6'\nthumbnail_size = 'medium'\n".parse().unwrap(),
    );
    s.uploaders.insert(
        "my-s3".into(),
        "type = 's3'\npreset = 'r2'\nbucket = 'screenshots'\naccount_id = 'abc123'\naccess_key_id = 'keyring:my-s3-access-key-id'\nsecret_access_key = 'keyring:my-s3-secret-access-key'\npublic_url_template = 'https://cdn.example.com/{key}'\n".parse().unwrap(),
    );
    s.uploaders.insert("work-dropbox".into(), "type = 'http'\nurl = 'https://files.example.com/upload'\nmethod = 'post'\nauth = 'bearer'\nauth_secret = 'keyring:work-dropbox-auth-secret'\n".parse().unwrap());
    s.destinations.image = Some("imgur".into());
    s.destinations.file = Some("my-s3".into());
    s.destinations.url_shortener = Some("is.gd".into());
    s.destinations.extension_overrides.insert("zip".into(), "my-s3".into());
    s.general.save_dir = Some(PathBuf::from("/home/marius/Pictures/ssx"));
    s
}

/// A sandboxed host with the demo history, a diagnostics report and detected file managers.
pub fn demo_host(root: &Path) -> Host {
    let mut h = Host::sandboxed(root);
    h.history = Arc::new(SharedHistory(demo_history(&root.join("shots"))));
    h.doctor = Arc::new(FixedDoctor(Ok(sample_report())));
    // File managers that "are installed": executables on the sandbox PATH.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["nautilus", "dolphin", "thunar"] {
        let p = bin.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    h
}
