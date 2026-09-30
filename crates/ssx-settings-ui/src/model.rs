//! The working copy of the settings and its life cycle: dirty tracking, validation, atomic
//! save, revert, and detection of somebody else changing the file underneath us.
//!
//! The window never edits the file directly. Pages mutate [`SettingsModel::working_mut`];
//! `saved` is what is on disk as far as we know. Saving goes through
//! `ssx_core::settings::Settings::save`, which validates again and writes atomically (temp
//! file + rename), so the daemon watching the file never sees a half-written one and there is
//! nothing to restart.
//!
//! **External changes.** The background app (or the user with an editor, or `ssx config set`)
//! can rewrite `settings.toml` while this window is open. Every load and save records a
//! [`FileStamp`] (length + SHA-256 of the content). [`SettingsModel::check_external`]
//! compares it with the file: with no unsaved edits the new content is simply adopted; with
//! unsaved edits a [`Conflict`] is raised, Save is refused with [`SaveError::Conflict`] and
//! the user chooses between reloading (losing the edits) and overwriting the file.

use std::path::{Path, PathBuf};

use ssx_core::{
    history::sha256_hex,
    settings::{Settings, SettingsError},
};

use crate::{nav::Page, validation::Issues};

/// A cheap fingerprint of `settings.toml` on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    /// Size in bytes.
    pub len: u64,
    /// SHA-256 of the content.
    pub sha256: String,
}

impl FileStamp {
    /// Fingerprint of `path`; `Ok(None)` if the file does not exist.
    pub fn read(path: &Path) -> std::io::Result<Option<FileStamp>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(FileStamp { len: bytes.len() as u64, sha256: sha256_hex(&bytes) })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Somebody changed the file while there were unsaved edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// `Some(reason)` when the file on disk can no longer be read as settings (it is kept
    /// as it is until the user decides).
    pub disk_error: Option<String>,
    /// `true` if the file was deleted.
    pub deleted: bool,
}

/// What [`SettingsModel::check_external`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum External {
    /// Nothing changed.
    Unchanged,
    /// The file changed and had no unsaved edits to lose: the new content was adopted.
    Reloaded,
    /// The file changed and there are unsaved edits: the user must decide.
    Conflict,
}

/// Why a save did not happen.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// The validator reports errors; fix them first.
    #[error("{count} problem(s) must be fixed before saving (first: {first})")]
    Blocked {
        /// Number of errors.
        count: usize,
        /// The first one, for the message.
        first: String,
    },
    /// The file changed on disk since it was loaded.
    #[error(
        "settings.toml was changed by another program since this window loaded it; choose Reload (discard your edits) or Overwrite"
    )]
    Conflict,
    /// The write failed.
    #[error("{0}")]
    Write(#[from] SettingsError),
    /// The config folder could not be created.
    #[error("cannot create {path}: {source}")]
    CreateDir {
        /// The folder.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The fingerprint of the file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// The settings being edited.
#[derive(Debug)]
pub struct SettingsModel {
    path: PathBuf,
    saved: Settings,
    working: Settings,
    stamp: Option<FileStamp>,
    warnings: Vec<String>,
    issues: Issues,
    generation: u64,
    validated_at: u64,
    conflict: Option<Conflict>,
    notice: Option<String>,
}

impl SettingsModel {
    /// Loads `path` (a missing file gives defaults). A file that cannot be parsed is moved
    /// aside by `Settings::load_or_recover` and reported through [`warnings`](Self::warnings).
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, SettingsError> {
        let path = path.into();
        let loaded = Settings::load_or_recover(&path)?;
        let stamp = FileStamp::read(&path).ok().flatten();
        let mut m = Self::from_parts(path, loaded.settings, stamp);
        m.warnings = loaded.warnings;
        Ok(m)
    }

    /// A model over `settings` that pretends to have been loaded from `path` (tests and the
    /// screenshot harness).
    pub fn from_parts(path: PathBuf, settings: Settings, stamp: Option<FileStamp>) -> Self {
        let issues = Issues::of(&settings);
        Self {
            path,
            saved: settings.clone(),
            working: settings,
            stamp,
            warnings: Vec::new(),
            issues,
            generation: 0,
            validated_at: 0,
            conflict: None,
            notice: None,
        }
    }

    /// The file being edited.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The settings as edited.
    pub fn working(&self) -> &Settings {
        &self.working
    }

    /// The settings as last loaded or saved.
    pub fn saved(&self) -> &Settings {
        &self.saved
    }

    /// Mutable access for a page. Marks the cached validation stale.
    pub fn working_mut(&mut self) -> &mut Settings {
        self.generation += 1;
        &mut self.working
    }

    /// Notes from loading (unknown keys, migrations, a corrupt file that was moved aside).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Dismisses the load warnings.
    pub fn clear_warnings(&mut self) {
        self.warnings.clear();
    }

    /// Validation of the working copy, recomputed when the copy changed.
    pub fn issues(&mut self) -> &Issues {
        if self.validated_at != self.generation {
            self.issues = Issues::of(&self.working);
            self.validated_at = self.generation;
        }
        &self.issues
    }

    /// A fresh validation of the working copy (no cache; for callers that only have `&self`).
    pub fn current_issues(&self) -> Issues {
        Issues::of(&self.working)
    }

    /// Whether anything differs from what is on disk.
    pub fn is_dirty(&self) -> bool {
        self.working != self.saved
    }

    /// The pages with unsaved changes.
    pub fn dirty_pages(&self) -> Vec<Page> {
        crate::nav::dirty_pages(&self.saved, &self.working)
    }

    /// Throws the edits away.
    pub fn revert(&mut self) {
        self.working = self.saved.clone();
        self.generation += 1;
    }

    /// Resets the working copy to the defaults (still needs saving).
    pub fn reset_to_defaults(&mut self) {
        self.working = Settings::default();
        self.generation += 1;
    }

    /// The pending external-modification conflict, if any.
    pub fn conflict(&self) -> Option<&Conflict> {
        self.conflict.as_ref()
    }

    /// A one-shot message (for a toast) about something the model did on its own.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// Compares the file with the fingerprint taken at the last load or save.
    pub fn check_external(&mut self) -> External {
        let now = match FileStamp::read(&self.path) {
            Ok(s) => s,
            // Unreadable right now (permissions, a network share hiccup): not a change.
            Err(_) => return External::Unchanged,
        };
        if now == self.stamp {
            return External::Unchanged;
        }
        if self.conflict.is_some() && !self.is_dirty() {
            self.conflict = None;
        }
        if self.is_dirty() {
            let conflict = match &now {
                None => Conflict { disk_error: None, deleted: true },
                Some(_) => Conflict {
                    disk_error: Settings::load(&self.path).err().map(|e| e.to_string()),
                    deleted: false,
                },
            };
            self.conflict = Some(conflict);
            // Remember what we saw, so the same change is not reported every second; the
            // decision (reload / overwrite) is what clears the conflict.
            self.stamp = now;
            return External::Conflict;
        }
        match Settings::load(&self.path) {
            Ok(loaded) => {
                self.adopt(loaded.settings, now);
                self.notice = Some("settings.toml changed on disk; reloaded".to_owned());
                External::Reloaded
            }
            Err(e) => {
                self.stamp = now;
                self.conflict = Some(Conflict { disk_error: Some(e.to_string()), deleted: false });
                External::Conflict
            }
        }
    }

    fn adopt(&mut self, settings: Settings, stamp: Option<FileStamp>) {
        self.saved = settings.clone();
        self.working = settings;
        self.stamp = stamp;
        self.conflict = None;
        self.generation += 1;
    }

    /// Resolves a conflict by loading the file from disk, discarding the edits.
    pub fn reload_from_disk(&mut self) -> Result<(), SettingsError> {
        let loaded = Settings::load(&self.path)?;
        let stamp = FileStamp::read(&self.path).ok().flatten();
        self.adopt(loaded.settings, stamp);
        Ok(())
    }

    /// Resolves a conflict by keeping the edits; the next save replaces the file.
    pub fn keep_edits_and_overwrite(&mut self) {
        self.conflict = None;
        self.stamp = FileStamp::read(&self.path).ok().flatten();
    }

    /// Validates, checks that nobody else changed the file, and writes it atomically.
    pub fn save(&mut self) -> Result<(), SaveError> {
        let issues = self.issues().clone();
        if let Some(first) = issues.errors().next() {
            return Err(SaveError::Blocked {
                count: issues.errors().count(),
                first: format!("{}: {}", first.path, first.message),
            });
        }
        let on_disk = FileStamp::read(&self.path)
            .map_err(|source| SaveError::Read { path: self.path.clone(), source })?;
        if on_disk != self.stamp || self.conflict.is_some() {
            if self.conflict.is_none() {
                self.conflict = Some(Conflict {
                    disk_error: Settings::load(&self.path).err().map(|e| e.to_string()),
                    deleted: on_disk.is_none(),
                });
            }
            return Err(SaveError::Conflict);
        }
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|source| SaveError::CreateDir { path: dir.to_path_buf(), source })?;
        }
        self.working.save(&self.path)?;
        self.stamp = FileStamp::read(&self.path)
            .map_err(|source| SaveError::Read { path: self.path.clone(), source })?;
        self.saved = self.working.clone();
        self.warnings.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ssx_core::settings::ImageFormatKind;

    use super::*;

    fn model() -> (tempfile::TempDir, SettingsModel) {
        let dir = tempfile::tempdir().unwrap();
        let m = SettingsModel::load(dir.path().join("settings.toml")).unwrap();
        (dir, m)
    }

    #[test]
    fn missing_file_loads_defaults_and_is_clean() {
        let (_d, m) = model();
        assert_eq!(m.working(), &Settings::default());
        assert!(!m.is_dirty());
        assert!(m.dirty_pages().is_empty());
        assert!(m.conflict().is_none());
    }

    #[test]
    fn edit_save_reload_round_trips() {
        let (dir, mut m) = model();
        m.working_mut().general.image_format = ImageFormatKind::Jpg;
        m.working_mut().general.image_quality = 77;
        m.working_mut().capture.hdr.knee = 0.8;
        assert!(m.is_dirty());
        assert_eq!(m.dirty_pages(), [Page::General, Page::Capture]);
        m.save().unwrap();
        assert!(!m.is_dirty());
        let loaded = Settings::load(&dir.path().join("settings.toml")).unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(&loaded.settings, m.working());
        assert_eq!(loaded.settings.general.image_quality, 77);
        // a second model over the same file sees the saved state
        let m2 = SettingsModel::load(dir.path().join("settings.toml")).unwrap();
        assert_eq!(m2.working(), m.working());
    }

    #[test]
    fn save_creates_the_config_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = SettingsModel::load(dir.path().join("deep/er/settings.toml")).unwrap();
        m.working_mut().general.show_notifications = false;
        m.save().unwrap();
        assert!(dir.path().join("deep/er/settings.toml").is_file());
    }

    #[test]
    fn invalid_settings_block_save_and_leave_the_file_alone() {
        let (dir, mut m) = model();
        m.save().unwrap();
        let before = std::fs::read_to_string(dir.path().join("settings.toml")).unwrap();
        m.working_mut().general.image_quality = 0;
        assert!(m.issues().blocks_save());
        let e = m.save().unwrap_err();
        assert!(matches!(e, SaveError::Blocked { count: 1, .. }), "{e}");
        assert!(e.to_string().contains("image_quality"), "{e}");
        assert_eq!(std::fs::read_to_string(dir.path().join("settings.toml")).unwrap(), before);
        assert!(m.is_dirty());
        // fixing it unblocks
        m.working_mut().general.image_quality = 50;
        m.save().unwrap();
    }

    #[test]
    fn issues_are_recomputed_when_the_working_copy_changes() {
        let (_d, mut m) = model();
        assert!(m.issues().is_empty());
        m.working_mut().capture.delay_ms = 999_999;
        assert_eq!(m.issues().errors().count(), 1);
        m.revert();
        assert!(m.issues().is_empty());
        assert!(!m.is_dirty());
    }

    #[test]
    fn revert_restores_the_saved_state() {
        let (_d, mut m) = model();
        m.working_mut().general.folder_pattern = "%y".into();
        m.revert();
        assert_eq!(m.working(), m.saved());
        assert!(!m.is_dirty());
    }

    #[test]
    fn external_change_without_edits_is_adopted() {
        let (dir, mut m) = model();
        m.save().unwrap();
        let mut other = Settings::default();
        other.general.image_quality = 33;
        other.save(&dir.path().join("settings.toml")).unwrap();
        assert_eq!(m.check_external(), External::Reloaded);
        assert_eq!(m.working().general.image_quality, 33);
        assert!(!m.is_dirty());
        assert!(m.take_notice().unwrap().contains("reloaded"));
        assert_eq!(m.check_external(), External::Unchanged);
    }

    #[test]
    fn external_change_with_edits_is_a_conflict_and_blocks_save() {
        let (dir, mut m) = model();
        m.save().unwrap();
        m.working_mut().general.image_quality = 10;
        let mut other = Settings::default();
        other.general.image_quality = 33;
        other.save(&dir.path().join("settings.toml")).unwrap();
        assert_eq!(m.check_external(), External::Conflict);
        assert!(m.conflict().is_some());
        assert!(matches!(m.save().unwrap_err(), SaveError::Conflict));
        assert_eq!(
            Settings::load(&dir.path().join("settings.toml")).unwrap().settings.general.image_quality,
            33,
            "the other program's file is untouched"
        );
        // Overwrite: our edits win.
        m.keep_edits_and_overwrite();
        assert!(m.conflict().is_none());
        m.save().unwrap();
        assert_eq!(
            Settings::load(&dir.path().join("settings.toml")).unwrap().settings.general.image_quality,
            10
        );
    }

    #[test]
    fn conflict_can_be_resolved_by_reloading() {
        let (dir, mut m) = model();
        m.save().unwrap();
        m.working_mut().general.image_quality = 10;
        let mut other = Settings::default();
        other.general.image_quality = 33;
        other.save(&dir.path().join("settings.toml")).unwrap();
        assert_eq!(m.check_external(), External::Conflict);
        m.reload_from_disk().unwrap();
        assert_eq!(m.working().general.image_quality, 33);
        assert!(!m.is_dirty());
        assert!(m.conflict().is_none());
    }

    #[test]
    fn save_notices_a_change_even_if_nobody_polled() {
        let (dir, mut m) = model();
        m.save().unwrap();
        m.working_mut().general.image_quality = 10;
        std::fs::write(dir.path().join("settings.toml"), "[general]\nimage_quality = 44\n").unwrap();
        assert!(matches!(m.save().unwrap_err(), SaveError::Conflict));
        assert!(m.conflict().is_some());
    }

    #[test]
    fn a_file_that_became_invalid_is_reported_not_reloaded() {
        let (dir, mut m) = model();
        m.save().unwrap();
        std::fs::write(dir.path().join("settings.toml"), "this is = not [valid").unwrap();
        assert_eq!(m.check_external(), External::Conflict);
        let c = m.conflict().unwrap();
        assert!(c.disk_error.is_some());
        assert!(m.reload_from_disk().is_err());
        assert_eq!(m.working(), &Settings::default(), "working copy untouched");
    }

    #[test]
    fn a_deleted_file_with_edits_is_a_conflict() {
        let (dir, mut m) = model();
        m.save().unwrap();
        m.working_mut().general.image_quality = 10;
        std::fs::remove_file(dir.path().join("settings.toml")).unwrap();
        assert_eq!(m.check_external(), External::Conflict);
        assert!(m.conflict().unwrap().deleted);
    }

    #[test]
    fn our_own_save_is_not_reported_as_external() {
        let (_d, mut m) = model();
        m.working_mut().general.image_quality = 12;
        m.save().unwrap();
        assert_eq!(m.check_external(), External::Unchanged);
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        std::fs::write(&p, "general = [broken").unwrap();
        let m = SettingsModel::load(&p).unwrap();
        assert_eq!(m.working(), &Settings::default());
        assert!(m.warnings().iter().any(|w| w.contains("corrupt")), "{:?}", m.warnings());
        assert!(!p.exists());
    }

    #[test]
    fn reset_to_defaults_is_dirty_until_saved() {
        let (_d, mut m) = model();
        m.working_mut().general.image_quality = 12;
        m.save().unwrap();
        m.reset_to_defaults();
        assert!(m.is_dirty());
        m.save().unwrap();
        assert_eq!(m.working(), &Settings::default());
    }
}
