//! The individual steps: what `save_to_file`, `upload`, `copy_url`, … actually do.

use std::path::{Path, PathBuf};

use super::{
    CommandSpec, EditResult, FailureKind, Notification, NotificationLevel, SkipReason, StepKind,
    StepReport, StepStatus, UploadProgress, UploadRequest, UploadSource,
    engine::{COMMAND_TIMEOUT, Run, StepEnd},
    item::{Encoded, Item, Origin, extension_of, is_image_ext, mime_for},
    report::Importance,
    template::{TemplateVars, expand_all},
};
use crate::{
    history::{
        EntryKind, NewEntry, PrunePolicy, ThumbnailOptions, sha256_hex, sha256_reader,
        thumbnail_from_bytes, thumbnail_from_frame,
    },
    pattern::{
        NameInputs, PatternContext, PatternError, RenderOptions, UnknownTokens, render_file_name,
        render_folder,
    },
    settings::{AfterCapture, AfterUpload, DestinationType, FolderPolicy},
};

/// Largest image file decoded for editing, clipboard, pinning, OCR and thumbnails.
const MAX_DECODE_BYTES: u64 = 128 * 1024 * 1024;
/// Largest file hashed for the history.
const MAX_HASH_BYTES: u64 = 256 * 1024 * 1024;
/// Largest image file decoded just to make a history thumbnail.
const MAX_THUMBNAIL_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

fn kind_of(ac: AfterCapture) -> StepKind {
    match ac {
        AfterCapture::OpenEditor => StepKind::OpenEditor,
        AfterCapture::CopyImageToClipboard => StepKind::CopyImage,
        AfterCapture::SaveToFile => StepKind::SaveToFile,
        AfterCapture::SaveAsDialog => StepKind::SaveAsDialog,
        AfterCapture::PinToScreen => StepKind::PinToScreen,
        AfterCapture::Ocr => StepKind::Ocr,
        AfterCapture::Upload => StepKind::Upload,
        AfterCapture::DeleteLocalFile => StepKind::DeleteLocalFile,
    }
}

fn na(why: &str) -> SkipReason {
    SkipReason::NotApplicable(why.to_owned())
}

fn path_str(p: &Path) -> String {
    p.display().to_string()
}

impl Run<'_> {
    // ---- naming --------------------------------------------------------------------

    fn pattern_ctx<'b>(&'b self, inputs: &'b NameInputs) -> PatternContext<'b> {
        let g = &self.settings().general;
        let naming = &self.engine.naming;
        PatternContext {
            clock: &*naming.clock,
            rng: &*naming.rng,
            env: &*naming.env,
            counter: &*naming.counter,
            inputs,
            options: RenderOptions {
                unknown: UnknownTokens::Keep,
                max_name_len: (g.max_file_name_len > 0).then_some(g.max_file_name_len),
                max_title_len: (g.max_title_len > 0).then_some(g.max_title_len),
            },
        }
    }

    /// Renders the file name pattern (for `ext == ""` a bare, sanitised stem).
    pub fn file_name(&self, inputs: &NameInputs, ext: &str) -> Result<String, PatternError> {
        render_file_name(&self.settings().general.file_name_pattern, ext, &self.pattern_ctx(inputs))
    }

    /// The folder new files of `kind` are saved in: save dir / type subfolder / dated folder.
    pub fn save_dir(&self, kind: EntryKind, inputs: &NameInputs) -> Result<PathBuf, PatternError> {
        let g = &self.settings().general;
        let mut dir = g.resolve_save_dir();
        if g.use_type_subfolders {
            dir.push(match kind {
                EntryKind::Image => &g.subfolders.image,
                EntryKind::Video => &g.subfolders.video,
                EntryKind::Text => &g.subfolders.text,
                EntryKind::File | EntryKind::Url => &g.subfolders.file,
            });
        }
        if !g.folder_pattern.trim().is_empty() {
            dir.push(render_folder(&g.folder_pattern, &self.pattern_ctx(inputs))?);
        }
        Ok(dir)
    }

    fn name_inputs(item: &Item) -> NameInputs {
        let (width, height) = item.dimensions();
        NameInputs {
            window_title: item.window_title.clone(),
            process_name: item.process_name.clone(),
            width,
            height,
        }
    }

    // ---- content helpers -----------------------------------------------------------

    /// Decodes the item's file into a frame if it has none yet.
    fn ensure_frame(&self, item: &mut Item) -> Result<(), StepEnd> {
        if item.frame.is_some() {
            return Ok(());
        }
        let path = item
            .local_path
            .clone()
            .or_else(|| item.input_path.clone())
            .ok_or_else(|| StepEnd::fail(FailureKind::Invalid, "there is no image"))?;
        let len = self.svc.fs.file_len(&path)?;
        if len > MAX_DECODE_BYTES {
            return Err(StepEnd::fail(
                FailureKind::Invalid,
                format!("{} is too large to open as an image ({len} bytes)", path_str(&path)),
            ));
        }
        let bytes = self.svc.fs.read(&path)?;
        let img = image::load_from_memory(&bytes).map_err(|e| {
            StepEnd::fail(
                FailureKind::Invalid,
                format!("{} is not a readable image: {e}", path_str(&path)),
            )
        })?;
        item.frame = Some(ssx_types::Frame::from_image(img.into_rgba8()));
        Ok(())
    }

    /// Encodes the current frame with the configured format (cached until the next edit).
    fn ensure_encoded<'i>(&self, item: &'i mut Item) -> Result<&'i Encoded, StepEnd> {
        if item.encoded.is_none() {
            let frame = item
                .frame
                .as_ref()
                .ok_or_else(|| StepEnd::fail(FailureKind::Invalid, "there is no image"))?;
            let opts = self.settings().general.encode_options();
            let bytes = frame.encode(opts).map_err(|e| {
                StepEnd::fail(FailureKind::Invalid, format!("cannot encode the image: {e}"))
            })?;
            item.encoded = Some(Encoded { bytes, ext: opts.format.extension() });
        }
        item.encoded
            .as_ref()
            .ok_or_else(|| StepEnd::fail(FailureKind::Internal, "encoder produced nothing"))
    }

    /// The bytes and extension that "saving" this item would write.
    fn content_bytes(&self, item: &mut Item) -> Result<(Vec<u8>, String), StepEnd> {
        if item.origin == Origin::Text {
            let text = item.text.clone().unwrap_or_default();
            return Ok((text.into_bytes(), "txt".to_owned()));
        }
        let enc = self.ensure_encoded(item)?;
        Ok((enc.bytes.clone(), enc.ext.to_owned()))
    }

    fn can_save_content(item: &Item) -> bool {
        matches!(item.origin, Origin::Image | Origin::Text)
            || (item.origin == Origin::UserFile && item.edited)
    }

    // ---- the per-item pipeline -----------------------------------------------------

    pub fn process_item(&self, mut item: Item) -> Item {
        self.prepare(&mut item);
        let idx = item.index;
        for &ac in &self.wf.after_capture {
            let kind = kind_of(ac);
            if let Some(reason) = item.halted.clone() {
                let r = self.skipped(Some(idx), kind, reason);
                item.steps.push(r);
                continue;
            }
            let report = match ac {
                AfterCapture::OpenEditor => self.open_editor(&mut item),
                AfterCapture::CopyImageToClipboard => self.copy_image(&mut item),
                AfterCapture::SaveToFile => self.save_to_file(&mut item),
                AfterCapture::SaveAsDialog => self.save_as(&mut item),
                AfterCapture::PinToScreen => self.pin(&mut item),
                AfterCapture::Ocr => self.ocr(&mut item),
                AfterCapture::Upload => self.upload(&mut item),
                AfterCapture::DeleteLocalFile => self.delete_local(&mut item),
            };
            item.push(report);
        }
        self.cleanup(&mut item);
        item
    }

    /// Validates input files/folders (and zips folders).
    fn prepare(&self, item: &mut Item) {
        if !matches!(item.origin, Origin::UserFile | Origin::Recording) {
            return;
        }
        let Some(path) = item.input_path.clone() else { return };
        let idx = item.index;
        let policy = self.settings().post_file.folders;
        let mut is_dir = false;
        let report = self.step(Some(idx), StepKind::LoadFile, || {
            if !self.svc.fs.exists(&path) {
                return Err(StepEnd::fail(
                    FailureKind::Io,
                    format!("{} does not exist (moved or deleted?)", path_str(&path)),
                ));
            }
            is_dir = self.svc.fs.is_dir(&path);
            if is_dir {
                return match policy {
                    FolderPolicy::Zip => Ok(Some("folder".to_owned())),
                    FolderPolicy::Error => Err(StepEnd::fail(
                        FailureKind::Invalid,
                        format!(
                            "{} is a folder and post_file.folders is \"error\"; set it to \"zip\" to upload folders as zip archives",
                            path_str(&path)
                        ),
                    )),
                };
            }
            let len = self.svc.fs.file_len(&path)?;
            Ok(Some(format!("{len} bytes")))
        });
        item.push(report);
        if !is_dir || item.halted.is_some() {
            return;
        }
        let mut zip: Option<PathBuf> = None;
        let report = self.step(Some(idx), StepKind::Zip, || {
            let z = self.svc.zipper.zip_folder(&path, self.cancel)?;
            let detail = format!("zipped {} into {}", path_str(&path), path_str(&z));
            zip = Some(z);
            Ok(Some(detail))
        });
        if let Some(z) = zip {
            item.origin = Origin::Folder;
            item.kind = EntryKind::File;
            item.ephemeral = Some(z.clone());
            item.local_path = Some(z);
            item.created = true;
        }
        item.push(report);
    }

    /// Removes the temporary archive of a zipped folder. Idempotent; failures only logged.
    fn cleanup(&self, item: &mut Item) {
        if let Some(zip) = item.ephemeral.take() {
            match self.svc.fs.remove_file(&zip) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %zip.display(), error = %e, "could not remove temporary archive");
                }
            }
            if item.local_path.as_deref() == Some(zip.as_path()) {
                item.local_path = None;
                item.created = false;
            }
        }
    }

    fn open_editor(&self, item: &mut Item) -> StepReport {
        let idx = Some(item.index);
        if !item.is_image() {
            return self.skipped(idx, StepKind::OpenEditor, na("not an image"));
        }
        if item.origin == Origin::UserFile && !self.settings().post_file.images_through_editor {
            return self.skipped(
                idx,
                StepKind::OpenEditor,
                na("post_file.images_through_editor is off"),
            );
        }
        self.step(idx, StepKind::OpenEditor, || {
            self.ensure_frame(item)?;
            let frame = item
                .frame
                .as_ref()
                .ok_or_else(|| StepEnd::fail(FailureKind::Internal, "frame vanished"))?;
            match self.svc.editor.edit(frame, self.cancel)? {
                EditResult::Edited(f) => {
                    if !f.is_sdr8() {
                        return Err(StepEnd::fail(
                            FailureKind::Invalid,
                            "the editor returned a non-8-bit-sRGB image",
                        ));
                    }
                    item.frame = Some(f);
                    item.encoded = None;
                    item.edited = true;
                    item.file_matches_content = false;
                    Ok(Some("edited".to_owned()))
                }
                EditResult::Cancelled => Err(StepEnd::Cancelled),
            }
        })
    }

    fn image_gate(&self, item: &Item, kind: StepKind) -> Option<StepReport> {
        let idx = Some(item.index);
        if self.batch {
            return Some(self.skipped(idx, kind, na("multiple files were posted")));
        }
        if !item.is_image() {
            return Some(self.skipped(idx, kind, SkipReason::NoImage));
        }
        None
    }

    fn copy_image(&self, item: &mut Item) -> StepReport {
        if let Some(r) = self.image_gate(item, StepKind::CopyImage) {
            return r;
        }
        self.step(Some(item.index), StepKind::CopyImage, || {
            self.ensure_frame(item)?;
            if let Some(f) = &item.frame {
                self.svc.clipboard.set_image(f)?;
            }
            Ok(None)
        })
    }

    fn pin(&self, item: &mut Item) -> StepReport {
        if let Some(r) = self.image_gate(item, StepKind::PinToScreen) {
            return r;
        }
        self.step(Some(item.index), StepKind::PinToScreen, || {
            self.ensure_frame(item)?;
            if let Some(f) = &item.frame {
                self.svc.pinner.pin(f)?;
            }
            Ok(None)
        })
    }

    fn ocr(&self, item: &mut Item) -> StepReport {
        if let Some(r) = self.image_gate(item, StepKind::Ocr) {
            return r;
        }
        self.step(Some(item.index), StepKind::Ocr, || {
            self.ensure_frame(item)?;
            let Some(f) = &item.frame else { return Ok(None) };
            let text = self.svc.ocr.recognize(f, self.cancel)?;
            if text.trim().is_empty() {
                return Ok(Some("no text found".to_owned()));
            }
            self.svc.clipboard.set_text(&text)?;
            Ok(Some(format!("copied {} characters", text.chars().count())))
        })
    }

    fn save_gate(&self, item: &Item, kind: StepKind) -> Option<StepReport> {
        let idx = Some(item.index);
        if Self::can_save_content(item) {
            return None;
        }
        let why = match item.origin {
            Origin::Recording => "recordings are written to the save folder while recording",
            _ => "the file is already on disk",
        };
        Some(self.skipped(idx, kind, na(why)))
    }

    fn save_to_file(&self, item: &mut Item) -> StepReport {
        if let Some(r) = self.save_gate(item, StepKind::SaveToFile) {
            return r;
        }
        self.step(Some(item.index), StepKind::SaveToFile, || {
            let inputs = Self::name_inputs(item);
            let (bytes, ext) = self.content_bytes(item)?;
            let dir = self
                .save_dir(item.kind, &inputs)
                .map_err(|e| StepEnd::fail(FailureKind::Invalid, e.to_string()))?;
            let name = self
                .file_name(&inputs, &ext)
                .map_err(|e| StepEnd::fail(FailureKind::Invalid, e.to_string()))?;
            let path = self.svc.fs.write_unique(&dir, &name, &bytes)?;
            let detail = format!("saved to {}", path_str(&path));
            item.local_path = Some(path);
            item.created = true;
            item.file_matches_content = true;
            Ok(Some(detail))
        })
    }

    fn save_as(&self, item: &mut Item) -> StepReport {
        if let Some(r) = self.save_gate(item, StepKind::SaveAsDialog) {
            return r;
        }
        self.step(Some(item.index), StepKind::SaveAsDialog, || {
            let inputs = Self::name_inputs(item);
            let (bytes, ext) = self.content_bytes(item)?;
            let dir = self
                .save_dir(item.kind, &inputs)
                .map_err(|e| StepEnd::fail(FailureKind::Invalid, e.to_string()))?;
            let name = self
                .file_name(&inputs, &ext)
                .map_err(|e| StepEnd::fail(FailureKind::Invalid, e.to_string()))?;
            let Some(chosen) = self.svc.save_dialog.choose_path(&dir.join(name))? else {
                return Err(StepEnd::Skipped(SkipReason::UserDeclined));
            };
            self.svc.fs.write_file(&chosen, &bytes)?;
            let detail = format!("saved to {}", path_str(&chosen));
            item.local_path = Some(chosen);
            item.created = true;
            item.file_matches_content = true;
            Ok(Some(detail))
        })
    }

    // ---- upload --------------------------------------------------------------------

    fn upload(&self, item: &mut Item) -> StepReport {
        let idx = item.index;
        self.step(Some(idx), StepKind::Upload, || {
            let ty = item.destination_type();
            // Stream the file when it *is* the content; otherwise send the bytes.
            let use_file = item.local_path.is_some()
                && (item.file_matches_content
                    || matches!(item.origin, Origin::Recording | Origin::UserFile | Origin::Folder)
                        && !item.edited);
            let mut bytes: Option<Vec<u8>> = None;
            let file_name: String;
            let ext: Option<String>;
            if use_file {
                let p = item.local_path.clone().unwrap_or_default();
                file_name = p.file_name().map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
                ext = extension_of(&p);
            } else {
                let (b, e) = self.content_bytes(item)?;
                let inputs = Self::name_inputs(item);
                file_name = match (&item.display_name, &item.input_path) {
                    (Some(n), _) => n.clone(),
                    (None, Some(orig)) if item.origin == Origin::UserFile => {
                        let stem = orig.file_stem().map_or_else(|| "image".to_owned(), |s| s.to_string_lossy().into_owned());
                        format!("{stem}.{e}")
                    }
                    _ => self
                        .file_name(&inputs, &e)
                        .map_err(|err| StepEnd::fail(FailureKind::Invalid, err.to_string()))?,
                };
                item.display_name = Some(file_name.clone());
                ext = Some(e);
                bytes = Some(b);
            }
            let uploader = self
                .settings()
                .destinations
                .resolve(ty, &self.wf.destination, ext.as_deref())
                .ok_or_else(|| {
                    StepEnd::fail(
                        FailureKind::NotConfigured,
                        format!(
                            "no {} uploader is configured; choose one in Settings > Destinations (destinations.{}) or in this workflow",
                            type_label(ty),
                            type_key(ty)
                        ),
                    )
                })?
                .to_owned();
            let path_for_source = item.local_path.clone();
            let source = match (&bytes, &path_for_source) {
                (Some(b), _) => UploadSource::Bytes(b),
                (None, Some(p)) => UploadSource::LocalFile(p),
                (None, None) => return Err(StepEnd::fail(FailureKind::Internal, "nothing to upload")),
            };
            let mime = mime_for(ext.as_deref());
            let req = UploadRequest { destination: &uploader, kind: ty, file_name: &file_name, mime, source };
            let progress = |p: UploadProgress| self.progress(Some(idx), StepKind::Upload, p.sent, p.total);
            let out = self.svc.uploaders.upload(&req, &progress, self.cancel)?;
            if out.url.trim().is_empty() {
                return Err(StepEnd::fail(
                    FailureKind::Service,
                    format!("the {uploader} uploader reported success but returned no URL, so the upload is not confirmed"),
                ));
            }
            let detail = format!("uploaded via {uploader}: {}", out.url);
            item.url = Some(out.url.clone());
            item.upload = Some(out);
            item.uploader = Some(uploader);
            item.confirmed = true;
            Ok(Some(detail))
        })
    }

    fn delete_local(&self, item: &mut Item) -> StepReport {
        let idx = Some(item.index);
        let kind = StepKind::DeleteLocalFile;
        if !item.confirmed {
            return self.skipped(idx, kind, SkipReason::UploadNotConfirmed);
        }
        let Some(path) = item.local_path.clone() else {
            return self.skipped(idx, kind, SkipReason::NoLocalFile);
        };
        if !item.created || self.protected.contains(&path) {
            return self.skipped(idx, kind, SkipReason::NotCreatedByWorkflow);
        }
        self.step(idx, kind, || {
            match self.svc.fs.remove_file(&path) {
                Ok(()) => {}
                // Already gone: the goal state is reached (idempotent).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(StepEnd::fail(
                        FailureKind::Io,
                        format!("uploaded, but could not delete {}: {e}", path_str(&path)),
                    ));
                }
            }
            if item.ephemeral.as_deref() == Some(path.as_path()) {
                item.ephemeral = None;
            }
            item.local_path = None;
            item.created = false;
            Ok(Some(format!("deleted {}", path_str(&path))))
        })
    }

    // ---- after upload --------------------------------------------------------------

    fn eligible(&self, item: &Item) -> bool {
        !item.cancelled && item.halted.is_none() && (!self.wf.uploads() || item.confirmed)
    }

    fn with_url(&self, items: &[Item]) -> Vec<usize> {
        (0..items.len()).filter(|&i| self.eligible(&items[i]) && items[i].url.is_some()).collect()
    }

    fn user_cancelled(&self, items: &[Item], run_steps: &[StepReport]) -> bool {
        if items.is_empty() {
            return run_steps.iter().any(|s| matches!(s.status, StepStatus::Cancelled))
                || self.cancel.is_cancelled();
        }
        self.cancel.is_cancelled() || items.iter().all(|i| i.cancelled)
    }

    pub fn after_upload_phase(&self, items: &mut [Item], run_steps: &mut Vec<StepReport>) {
        for step in &self.wf.after_upload {
            let kind = after_upload_kind(step);
            if self.cancel.is_cancelled() {
                run_steps.push(self.skipped(None, kind, SkipReason::Cancelled));
                continue;
            }
            match step {
                AfterUpload::ShortenUrl => {
                    let targets = self.with_url(items);
                    if targets.is_empty() {
                        run_steps.push(self.skipped(None, kind, SkipReason::NoUrl));
                    }
                    for i in targets {
                        let rep = self.shorten(&mut items[i], true);
                        items[i].push(rep);
                    }
                }
                AfterUpload::CopyUrl => {
                    let urls: Vec<String> = self
                        .with_url(items)
                        .into_iter()
                        .filter_map(|i| items[i].url.clone())
                        .collect();
                    run_steps.push(self.copy_text(kind, &urls, SkipReason::NoUrl));
                }
                AfterUpload::CopyShortUrl => {
                    for i in self.with_url(items) {
                        if items[i].short_url.is_none() {
                            let rep = self.shorten(&mut items[i], false);
                            items[i].push(rep);
                        }
                    }
                    let urls: Vec<String> = self
                        .with_url(items)
                        .into_iter()
                        .filter_map(|i| items[i].short_url.clone())
                        .collect();
                    run_steps.push(self.copy_text(kind, &urls, na("no short URL available")));
                }
                AfterUpload::OpenUrl => {
                    let targets = self.with_url(items);
                    if targets.is_empty() {
                        run_steps.push(self.skipped(None, kind, SkipReason::NoUrl));
                    }
                    for i in targets {
                        let url = items[i].url.clone().unwrap_or_default();
                        let rep = self.step(Some(i), kind, || {
                            self.svc.opener.open(&url)?;
                            Ok(Some(format!("opened {url}")))
                        });
                        items[i].push(rep);
                    }
                }
                AfterUpload::ShowQrCode => {
                    let targets = self.with_url(items);
                    match targets.as_slice() {
                        [] => run_steps.push(self.skipped(None, kind, SkipReason::NoUrl)),
                        [i] => {
                            let i = *i;
                            let url = items[i].url.clone().unwrap_or_default();
                            let rep = self.step(Some(i), kind, || {
                                let img = self.svc.qr.render(&url)?;
                                self.svc.notifier.show_qr(&url, &img)?;
                                Ok(None)
                            });
                            items[i].push(rep);
                        }
                        _ => run_steps.push(self.skipped(
                            None,
                            kind,
                            na("multiple files were posted"),
                        )),
                    }
                }
                AfterUpload::ShowNotification => {
                    run_steps.push(self.notify(items, run_steps));
                }
                AfterUpload::RunCommand { program, args } => {
                    let targets: Vec<usize> =
                        (0..items.len()).filter(|&i| self.eligible(&items[i])).collect();
                    if targets.is_empty() {
                        run_steps.push(self.skipped(
                            None,
                            kind,
                            na("no item finished successfully"),
                        ));
                    }
                    for i in targets {
                        let vars = template_vars(&items[i]);
                        let rep = self.step(Some(i), kind, || {
                            let args = expand_all(args, &vars)
                                .map_err(|e| StepEnd::fail(FailureKind::Invalid, e.to_string()))?;
                            let spec = CommandSpec {
                                program: program.clone(),
                                args,
                                timeout: COMMAND_TIMEOUT,
                            };
                            let out = self.svc.commands.run(&spec, self.cancel)?;
                            if out.success {
                                Ok(Some(format!("{program} finished")))
                            } else {
                                let code = out
                                    .exit_code
                                    .map_or_else(|| "a signal".to_owned(), |c| format!("code {c}"));
                                let tail = if out.stderr_tail.is_empty() {
                                    String::new()
                                } else {
                                    format!(": {}", out.stderr_tail)
                                };
                                Err(StepEnd::fail(
                                    FailureKind::Service,
                                    format!("{program} exited with {code}{tail}"),
                                ))
                            }
                        });
                        items[i].push(rep);
                    }
                }
            }
        }
    }

    fn copy_text(&self, kind: StepKind, lines: &[String], if_empty: SkipReason) -> StepReport {
        if lines.is_empty() {
            return self.skipped(None, kind, if_empty);
        }
        let text = lines.join("\n");
        self.step(None, kind, || {
            self.svc.clipboard.set_text(&text)?;
            Ok(Some(format!("copied {} URL(s)", lines.len())))
        })
    }

    /// Shortens the item's URL. `replace_current` makes the short URL the one later steps use.
    fn shorten(&self, item: &mut Item, replace_current: bool) -> StepReport {
        let idx = Some(item.index);
        let Some(url) = item.url.clone() else {
            return self.skipped(idx, StepKind::ShortenUrl, SkipReason::NoUrl);
        };
        // Shorten the original upload URL, not an already-shortened one.
        let source = item.upload.as_ref().map_or(url, |u| u.url.clone());
        self.step(idx, StepKind::ShortenUrl, || {
            let provider = self
                .settings()
                .destinations
                .resolve(DestinationType::UrlShortener, &self.wf.destination, None)
                .ok_or_else(|| {
                    StepEnd::fail(
                        FailureKind::NotConfigured,
                        "no URL shortener is configured; choose one in Settings > Destinations (destinations.url_shortener)",
                    )
                })?
                .to_owned();
            let short = self.svc.shortener.shorten(&provider, &source, self.cancel)?;
            if short.trim().is_empty() {
                return Err(StepEnd::fail(
                    FailureKind::Service,
                    format!("the {provider} shortener returned an empty URL"),
                ));
            }
            let detail = format!("shortened via {provider}: {short}");
            item.short_url = Some(short.clone());
            if replace_current {
                item.url = Some(short);
            }
            Ok(Some(detail))
        })
    }

    fn notify(&self, items: &[Item], run_steps: &[StepReport]) -> StepReport {
        let kind = StepKind::ShowNotification;
        if self.user_cancelled(items, run_steps) {
            return self.skipped(None, kind, SkipReason::Cancelled);
        }
        if !self.settings().general.show_notifications {
            return self.skipped(None, kind, na("notifications are disabled in settings"));
        }
        let n = Self::build_notification(items, run_steps);
        self.step(None, kind, || {
            self.svc.notifier.notify(&n)?;
            Ok(Some(n.title.clone()))
        })
    }

    fn build_notification(items: &[Item], run_steps: &[StepReport]) -> Notification {
        let failures: Vec<String> = run_steps
            .iter()
            .map(|s| (None, s))
            .chain(items.iter().flat_map(|i| i.steps.iter().map(move |s| (Some(i), s))))
            .filter_map(|(item, s)| match &s.status {
                StepStatus::Failed(f) if s.kind.importance() != Importance::Optional => {
                    let who = item
                        .and_then(|i| i.best_path())
                        .and_then(|p| p.file_name())
                        .map(|n| format!("{}: ", n.to_string_lossy()))
                        .unwrap_or_default();
                    Some(format!("{who}{}: {}", s.kind, f.message))
                }
                _ => None,
            })
            .collect();
        let urls: Vec<String> = items.iter().filter_map(|i| i.url.clone()).collect();
        let kept: Vec<String> = items
            .iter()
            .filter(|i| i.created && i.local_path.is_some() && !i.confirmed)
            .filter_map(|i| i.local_path.as_deref().map(path_str))
            .collect();
        let saved: Vec<&Path> = items.iter().filter_map(|i| i.local_path.as_deref()).collect();

        if failures.is_empty() {
            let (title, body) = if items.len() > 1 && !urls.is_empty() {
                (format!("{} uploads complete", urls.len()), urls.join("\n"))
            } else if let Some(url) = urls.first() {
                ("Upload complete".to_owned(), url.clone())
            } else if let Some(p) = saved.first() {
                ("Saved".to_owned(), path_str(p))
            } else {
                ("Done".to_owned(), String::new())
            };
            return Notification {
                level: NotificationLevel::Success,
                title,
                body,
                url: urls.first().cloned(),
                path: saved.first().map(|p| p.to_path_buf()),
            };
        }
        let achieved = !urls.is_empty() || !kept.is_empty();
        let upload_failed = items
            .iter()
            .any(|i| i.steps.iter().any(|s| s.kind == StepKind::Upload && s.status.is_failure()));
        let title = if upload_failed && urls.is_empty() {
            "Upload failed".to_owned()
        } else if upload_failed {
            "Upload failed for some files".to_owned()
        } else if achieved {
            "Completed with problems".to_owned()
        } else {
            "ssx: something went wrong".to_owned()
        };
        let mut body = failures.join("\n");
        if !kept.is_empty() {
            body.push_str("\nThe local file was kept: ");
            body.push_str(&kept.join(", "));
        }
        Notification {
            level: if achieved { NotificationLevel::Warning } else { NotificationLevel::Error },
            title,
            body,
            url: urls.first().cloned(),
            path: kept.first().map(PathBuf::from),
        }
    }

    // ---- history -------------------------------------------------------------------

    fn thumbnail(&self, item: &Item) -> Option<Vec<u8>> {
        let opts = ThumbnailOptions {
            max_edge: self.settings().history.thumbnail_max_edge,
            ..ThumbnailOptions::default()
        };
        if let Some(frame) = &item.frame {
            return thumbnail_from_frame(frame, opts)
                .map_err(|e| tracing::debug!(error = %e, "no history thumbnail"))
                .ok();
        }
        let path = item.local_path.as_deref()?;
        let image_file = matches!(item.origin, Origin::UserFile | Origin::Recording)
            && is_image_ext(extension_of(path).as_deref());
        if !image_file || self.svc.fs.file_len(path).ok()? > MAX_THUMBNAIL_SOURCE_BYTES {
            return None;
        }
        let bytes = self.svc.fs.read(path).ok()?;
        thumbnail_from_bytes(&bytes, opts)
            .map_err(|e| tracing::debug!(error = %e, "no history thumbnail"))
            .ok()
    }

    /// Writes the item to the history (if enabled) and applies the retention policy.
    /// Runs even after cancellation: whatever was saved or uploaded must be findable.
    pub fn record_history(&self, item: &mut Item) {
        let Some(history) = self.svc.history else { return };
        let cfg = &self.settings().history;
        if !cfg.enabled {
            return;
        }
        let local = match item.origin {
            Origin::Folder => item.input_path.clone(),
            _ => item.local_path.clone(),
        };
        if local.is_none() && item.upload.is_none() && item.text.is_none() {
            return;
        }
        let mut id = None;
        let report = self.step_always(Some(item.index), StepKind::RecordHistory, || {
            let (width, height) = item.dimensions();
            let now = self.engine.naming.clock.now().timestamp_millis();
            let mut e = NewEntry::new(item.entry_kind());
            e.created_at = now;
            e.size_bytes = local
                .as_deref()
                .and_then(|p| self.svc.fs.file_len(p).ok())
                .or_else(|| item.encoded.as_ref().map(|b| b.bytes.len() as u64))
                .or_else(|| item.text.as_ref().map(|t| t.len() as u64));
            e.sha256 = match (&local, &item.encoded) {
                (Some(p), _) if item.origin != Origin::Folder => {
                    let small = self.svc.fs.file_len(p).is_ok_and(|l| l <= MAX_HASH_BYTES);
                    if small {
                        self.svc.fs.open_read(p).ok().and_then(|mut r| sha256_reader(&mut r).ok())
                    } else {
                        None
                    }
                }
                (None, Some(enc)) => Some(sha256_hex(&enc.bytes)),
                _ => None,
            };
            e.thumbnail = self.thumbnail(item);
            e.local_path = local;
            e.upload_url = item.upload.as_ref().map(|u| u.url.clone());
            e.thumbnail_url = item.upload.as_ref().and_then(|u| u.thumbnail_url.clone());
            e.deletion_url = item.upload.as_ref().and_then(|u| u.deletion_url.clone());
            e.uploader.clone_from(&item.uploader);
            e.window_title.clone_from(&item.window_title);
            e.process_name.clone_from(&item.process_name);
            e.width = width;
            e.height = height;
            e.workflow_id = Some(self.wf.id.clone());
            e.note.clone_from(&item.text);
            let new_id = history.insert(&e).map_err(|err| {
                StepEnd::fail(FailureKind::Io, format!("could not record the history entry: {err}"))
            })?;
            id = Some(new_id);
            // Retention is best effort and must never fail the step.
            let policy = PrunePolicy {
                max_entries: (cfg.max_entries > 0).then_some(cfg.max_entries),
                max_age: (cfg.max_age_days > 0)
                    .then(|| std::time::Duration::from_secs(u64::from(cfg.max_age_days) * 86_400)),
            };
            if policy != PrunePolicy::default()
                && let Err(err) = history.prune(&policy, now)
            {
                tracing::warn!(error = %err, "history retention failed");
            }
            Ok(Some(format!("history entry {new_id}")))
        });
        item.history_id = id;
        item.steps.push(report);
    }
}

fn after_upload_kind(step: &AfterUpload) -> StepKind {
    match step {
        AfterUpload::CopyUrl => StepKind::CopyUrl,
        AfterUpload::CopyShortUrl => StepKind::CopyShortUrl,
        AfterUpload::OpenUrl => StepKind::OpenUrl,
        AfterUpload::ShortenUrl => StepKind::ShortenUrl,
        AfterUpload::ShowQrCode => StepKind::ShowQrCode,
        AfterUpload::ShowNotification => StepKind::ShowNotification,
        AfterUpload::RunCommand { .. } => StepKind::RunCommand,
    }
}

fn template_vars(item: &Item) -> TemplateVars {
    let path = item.best_path();
    TemplateVars {
        path: path.map(path_str),
        dir: path.and_then(Path::parent).map(path_str),
        file_name: path.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()),
        url: item.url.clone(),
        short_url: item.short_url.clone(),
        thumbnail_url: item.upload.as_ref().and_then(|u| u.thumbnail_url.clone()),
        deletion_url: item.upload.as_ref().and_then(|u| u.deletion_url.clone()),
    }
}

fn type_label(ty: DestinationType) -> &'static str {
    match ty {
        DestinationType::Image => "image",
        DestinationType::Text => "text",
        DestinationType::File => "file",
        DestinationType::Video => "video",
        DestinationType::UrlShortener => "URL shortener",
        DestinationType::UrlSharing => "URL sharing",
    }
}

fn type_key(ty: DestinationType) -> &'static str {
    match ty {
        DestinationType::Image => "image",
        DestinationType::Text => "text",
        DestinationType::File => "file",
        DestinationType::Video => "video",
        DestinationType::UrlShortener => "url_shortener",
        DestinationType::UrlSharing => "url_sharing",
    }
}
