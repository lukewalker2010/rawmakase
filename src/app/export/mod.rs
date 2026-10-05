//! Export commands and background exports. The dialog lives in `dialog`; the
//! export itself (settings, pipeline, file writing) in `crate::export`. Each
//! export runs on its own thread and the top bar shows its progress while you
//! keep editing.
mod batch;
mod dialog;
mod watermark_editor;

use super::{Editor, worker::Event};
use crate::app::theme;
use crate::app::widgets::plural;
use crate::export::{
    Existing, ExportSettings, Replace,
    assemble::Values,
    job::{self, Photo},
    settings::unique,
};
use eframe::egui::{self, Color32, Sense, Vec2};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

/// A running export, shown in the top bar until it finishes.
struct Job {
    /// Progress in thousandths.
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

/// A file that already exists, waiting for Overwrite, Use Unique Name or Skip.
#[derive(Clone)]
struct Conflict {
    target: PathBuf,
    settings: ExportSettings,
    photo: Photo,
}

/// The photos a batch will export, and what it had to leave out.
#[derive(Default)]
struct Selection {
    photos: Vec<batch::Queued>,
    /// Selected photos that cannot be exported: not a camera RAW, offline, or
    /// their metadata could not be read.
    unusable: usize,
    /// Why the first one could not be, for the status line.
    problem: Option<String>,
}

impl Selection {
    /// Notes a photo left out of the batch.
    fn unusable(&mut self, why: String, path: &std::path::Path) {
        self.unusable += 1;
        self.problem.get_or_insert_with(|| {
            format!(
                "{} not exported: {why}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        });
    }
}

/// A batch of selected photos, waiting to be exported when the dialog closes.
#[derive(Clone)]
struct Batch {
    /// The selection as it was when Export was chosen.
    ids: Vec<i64>,
    /// Where every photo will go, once the folder is known, and the settings
    /// that were chosen to work it out. Kept so the answer to the
    /// existing-files question can start the batch they belong to.
    plan: Option<batch::Plan>,
    settings: ExportSettings,
    /// Selected photos the batch could not carry, kept with the plan so the
    /// summary can count them wherever the batch ends up being started from.
    unusable: usize,
    problem: Option<String>,
}

#[derive(Default)]
pub(super) struct Exports {
    /// The dialog is open, editing `draft`.
    dialog: bool,
    draft: ExportSettings,
    /// The folder chooser's answer, from its own thread.
    picked: Arc<Mutex<Option<PathBuf>>>,
    picking: Arc<AtomicBool>,
    jobs: Vec<Job>,
    conflict: Option<Conflict>,
    /// A batch planned from a selection, before the existing files are settled.
    batch: Option<Batch>,
    /// The Watermark Editor, over the dialog.
    watermark_editor: Option<watermark_editor::WatermarkEditor>,
    /// Saved watermarks, read when the dialog opens and after the editor
    /// saves one.
    watermarks: Vec<crate::watermark::Watermark>,
}

impl Editor {
    /// Export…: the dialog, starting from the last export's choices.
    pub(super) fn open_export_dialog(&mut self) {
        // Exporting the open photo is never a batch, even if a selection was
        // left over from a dialog that was closed without exporting.
        self.exports.batch = None;
        if self.export_photo().is_some() {
            self.exports.draft = ExportSettings::load().unwrap_or_default();
            self.exports.watermarks = crate::watermark::presets();
            self.exports.dialog = true;
        }
    }
    /// Closing the Export dialog without exporting.
    pub(in crate::app) fn cancel_export_dialog(&mut self) {
        self.exports.dialog = false;
        // Closing without exporting must not leave the selection armed: the next
        // Export would then send it instead of the open photo.
        self.exports.batch = None;
    }

    /// Whether a selection is still armed to be exported, which it is only
    /// between choosing Export for it and exporting or closing the dialog.
    #[cfg(test)]
    pub(in crate::app) fn export_armed(&self) -> bool {
        self.exports.batch.is_some()
    }

    /// Whether the Export dialog is showing.
    #[cfg(test)]
    pub(in crate::app) fn export_dialog_open(&self) -> bool {
        self.exports.dialog
    }

    /// The files a selection would export, and how many it left out, so a test
    /// can see which photos a batch would carry.
    #[cfg(test)]
    pub(in crate::app) fn export_selection(&mut self, ids: &[i64]) -> (Vec<PathBuf>, usize) {
        let selection = self.photos_for_export(ids);
        (
            selection.photos.into_iter().map(|p| p.source).collect(),
            selection.unusable,
        )
    }

    /// Export… for photos chosen in the Library. One photo exports the open one,
    /// so the toolbar and this menu behave the same; more than one exports the
    /// selection, as Lightroom does.
    pub(super) fn open_export_of(&mut self, ids: &[i64]) {
        if ids.len() > 1 {
            return self.open_batch_export_dialog(ids.to_vec());
        }
        self.open_export_dialog();
    }

    /// Export… with photos selected in the Library: the same dialog, exporting
    /// every one of them when it closes.
    fn open_batch_export_dialog(&mut self, ids: Vec<i64>) {
        if ids.is_empty() {
            self.status = "Select photos to export".into();
            return;
        }
        self.exports.batch = Some(Batch {
            ids,
            plan: None,
            settings: ExportSettings::load().unwrap_or_default(),
            unusable: 0,
            problem: None,
        });
        self.exports.draft = ExportSettings::load().unwrap_or_default();
        self.exports.watermarks = crate::watermark::presets();
        self.exports.dialog = true;
    }
    /// Export with Previous: the last export's choices, without the dialog.
    pub(super) fn export_with_previous(&mut self) {
        match ExportSettings::load() {
            Some(settings) => self.export(settings),
            None => self.open_export_dialog(),
        }
    }
    /// The Export dialog or its existing-file question is showing.
    pub(super) fn export_modal(&self) -> bool {
        self.exports.dialog
            || self.exports.conflict.is_some()
            || self
                .exports
                .batch
                .as_ref()
                .is_some_and(|b| b.plan.is_some())
            || self.exports.watermark_editor.is_some()
    }
    pub(super) fn exporting(&self) -> bool {
        self.exports
            .jobs
            .iter()
            .any(|j| !j.done.load(Ordering::Relaxed))
    }

    /// The open photo as it is now, with its catalog rating, label and keywords.
    fn export_photo(&mut self) -> Option<Photo> {
        let catalog = self
            .document
            .catalog_photo
            .and_then(|id| self.library.as_ref()?.photo(id));
        let values = match (catalog, self.library.as_ref()) {
            (Some(p), Some(library)) => {
                let read = library.catalog.descriptive(p.id).and_then(|descriptive| {
                    let keywords = library.catalog.keywords(p.id)?;
                    Ok(Values {
                        descriptive,
                        keywords: keywords
                            .into_iter()
                            .map(|k| crate::xmp::write::KeywordPath {
                                path: k.path,
                                exported: k.exported,
                            })
                            .collect(),
                        rating: p.rating,
                        label: p.label.clone(),
                    })
                });
                read.map_err(|e| format!("Metadata could not be read for export: {e}"))
            }
            _ => Ok(Values::default()),
        };
        let values = match values {
            Ok(values) => values,
            Err(e) => {
                self.status = e;
                return None;
            }
        };
        Some(Photo {
            // None while the viewport still holds only a preview: the export
            // thread decodes the full image itself.
            image: self.document.full().cloned(),
            source: self.document.path.clone()?,
            recipe: self.document.recipe.clone(),
            values,
            watermark: None,
        })
    }

    fn export(&mut self, settings: ExportSettings) {
        if self.exports.batch.is_some() {
            return self.export_batch(settings);
        }
        if let Err(e) = settings.save() {
            self.status = format!("Export settings not saved: {e:#}");
        }
        let Some(mut photo) = self.export_photo() else {
            return;
        };
        if settings.watermark {
            match watermark_for(&settings.watermark_name) {
                Some(w) => photo.watermark = Some(w),
                None => {
                    self.status = format!("Watermark not found: {}", settings.watermark_name);
                    return;
                }
            }
        }
        let Some(target) = settings.target(&photo.source) else {
            self.status = "Choose a folder to export to".into();
            return;
        };
        if !target.exists() {
            return self.start_export(photo, target, settings, Replace::NoClobber);
        }
        match settings.existing {
            Existing::Ask => {
                self.exports.conflict = Some(Conflict {
                    target,
                    settings,
                    photo,
                })
            }
            Existing::Unique => {
                self.start_export(photo, unique(&target), settings, Replace::NoClobber)
            }
            Existing::Overwrite => self.start_export(photo, target, settings, Replace::Overwrite),
            Existing::Skip => self.status = format!("Skipped: {} already exists", target.display()),
        }
    }

    /// Export… for the photos selected in the Library. Every photo carries its
    /// own saved edit and catalog metadata, and none is decoded here: the export
    /// thread reads each file as it reaches it.
    fn export_batch(&mut self, settings: ExportSettings) {
        if let Err(e) = settings.save() {
            self.status = format!("Export settings not saved: {e:#}");
        }
        let Some(batch) = self.exports.batch.clone() else {
            return;
        };
        let selection = self.photos_for_export(&batch.ids);
        if selection.photos.is_empty() {
            self.exports.batch = None;
            self.status = selection
                .problem
                .unwrap_or_else(|| "None of the selected photos could be read".into());
            return;
        }
        if settings.watermark {
            let watermark = watermark_for(&settings.watermark_name);
            if watermark.is_none() {
                self.exports.batch = None;
                self.status = format!("Watermark not found: {}", settings.watermark_name);
                return;
            }
            // The Simple Copyright Watermark takes its text from each photo, so
            // it is attached by name and filled in per photo as it is exported.
            return self.export_batch_with(settings, selection, watermark);
        }
        self.export_batch_with(settings, selection, None);
    }

    fn export_batch_with(
        &mut self,
        settings: ExportSettings,
        selection: Selection,
        watermark: Option<crate::watermark::Watermark>,
    ) {
        let Selection {
            photos,
            unusable,
            problem,
        } = selection;
        let photos = photos
            .into_iter()
            .map(|mut photo| {
                photo.watermark = watermark.clone();
                photo
            })
            .collect();
        let plan = match batch::plan(photos, &settings) {
            Ok(plan) => plan,
            Err(e) => {
                self.exports.batch = None;
                self.status = format!("Export not started: {e:#}");
                return;
            }
        };
        // What the selection had to leave out is skipped, not lost: it belongs
        // in the summary with the files the existing-files answer skipped.
        // Files that are already there are settled once for the whole batch;
        // asking per photo would mean a dialog per exported photo.
        if batch::clashing(&plan) > 0 && settings.existing == Existing::Ask {
            if let Some(batch) = self.exports.batch.as_mut() {
                batch.plan = Some(plan);
                batch.settings = settings;
                batch.unusable = unusable;
                batch.problem = problem;
            }
            return;
        }
        let answer = settings.existing;
        self.start_batch(settings, plan, answer, unusable, problem);
    }

    /// The selected photos as they will be exported: each one's saved edit,
    /// left unresolved because that needs the file open.
    ///
    /// A photo that cannot be exported is left out and counted rather than
    /// stopping the batch: one offline photo or one unreadable keyword row
    /// should not cost the other two hundred.
    fn photos_for_export(&mut self, ids: &[i64]) -> Selection {
        let Some(library) = self.library.as_ref() else {
            return Selection::default();
        };
        let mut selection = Selection {
            photos: Vec::with_capacity(ids.len()),
            ..Default::default()
        };
        for id in ids {
            let Some(record) = library.photo(*id) else {
                continue;
            };
            let available = library.is_available(&record.path);
            if let Some(refusal) = crate::app::library::develop_refusal(record, available) {
                selection.unusable(refusal.label(), &record.path);
                continue;
            }
            let values = match library.catalog.descriptive(*id).and_then(|descriptive| {
                let keywords = library.catalog.keywords(*id)?;
                Ok(Values {
                    descriptive,
                    keywords: keywords
                        .into_iter()
                        .map(|k| crate::xmp::write::KeywordPath {
                            path: k.path,
                            exported: k.exported,
                        })
                        .collect(),
                    rating: record.rating,
                    label: record.label.clone(),
                })
            }) {
                Ok(values) => values,
                Err(e) => {
                    selection.unusable(format!("metadata unreadable ({e:#})"), &record.path);
                    continue;
                }
            };
            selection.photos.push(batch::Queued {
                source: record.path.clone(),
                edit: library.edit_of(*id),
                values,
                watermark: None,
            });
        }
        selection
    }

    fn start_batch(
        &mut self,
        settings: ExportSettings,
        plan: batch::Plan,
        answer: Existing,
        unusable: usize,
        problem: Option<String>,
    ) {
        let total = plan.len();
        let writes = batch::resolve(plan, answer);
        // Photos the answer left out never become writes, so the summary is told
        // how many they were, along with the ones the selection could not carry.
        let skipped = total - writes.len() + unusable;
        if writes.is_empty() {
            self.exports.batch = None;
            self.status = "Nothing to export".into();
            return;
        }
        let job = Job {
            progress: Default::default(),
            cancel: Default::default(),
            done: Default::default(),
        };
        let (progress, cancel, done) = (job.progress.clone(), job.cancel.clone(), job.done.clone());
        self.exports.jobs.push(job);
        let (tx, ctx) = (self.tx.clone(), self.context.clone());
        self.exports.batch = None;
        self.status = format!("Exporting {}…", plural(writes.len(), "photo", "photos"));
        let total = writes.len() as f32;
        std::thread::spawn(move || {
            let (outcome, error) =
                batch::run(&writes, &settings, skipped, &cancel, |done, _, fraction| {
                    progress.store(
                        ((done as f32 + fraction) / total * 1000.) as u32,
                        Ordering::Relaxed,
                    );
                    ctx.request_repaint();
                });
            let mut status = outcome.summary();
            // Why a photo was left out is said once, not once per photo.
            if let Some(problem) = problem {
                status = format!("{status} · {problem}");
            }
            if let Some(e) = error {
                status = format!("{status} · {e}");
            }
            done.store(true, Ordering::Relaxed);
            let _ = tx.send(Event::Exported(status));
            ctx.request_repaint();
        });
    }

    fn start_export(
        &mut self,
        photo: Photo,
        target: PathBuf,
        settings: ExportSettings,
        replace: Replace,
    ) {
        let job = Job {
            progress: Default::default(),
            cancel: Default::default(),
            done: Default::default(),
        };
        let (progress, cancel, done) = (job.progress.clone(), job.cancel.clone(), job.done.clone());
        self.exports.jobs.push(job);
        let (tx, ctx) = (self.tx.clone(), self.context.clone());
        self.status = format!(
            "Exporting {}…",
            target.file_name().unwrap_or_default().to_string_lossy()
        );
        std::thread::spawn(move || {
            let result = job::run(photo, &settings, &target, replace, &cancel, |p| {
                progress.store((p * 1000.) as u32, Ordering::Relaxed);
                ctx.request_repaint();
            });
            let status = match result {
                Ok(None) => format!("Exported {}", target.display()),
                Ok(Some(notice)) => format!("Exported {} · {notice}", target.display()),
                Err(_) if cancel.load(Ordering::Relaxed) => "Export cancelled".into(),
                Err(e) => format!("Export failed: {e:#}"),
            };
            done.store(true, Ordering::Relaxed);
            let _ = tx.send(Event::Exported(status));
            ctx.request_repaint();
        });
    }

    /// Lightroom's activity indicator: a bar in the top bar while exports run,
    /// with a button that cancels them.
    pub(super) fn export_progress(&mut self, ui: &mut egui::Ui) {
        let jobs = &mut self.exports.jobs;
        jobs.retain(|j| !j.done.load(Ordering::Relaxed));
        if jobs.is_empty() {
            return;
        }
        let n = jobs.len();
        let fraction = jobs
            .iter()
            .map(|j| j.progress.load(Ordering::Relaxed) as f32 / 1000.)
            .sum::<f32>()
            / n as f32;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(210., 28.), Sense::hover());
        let painter = ui.painter();
        painter.text(
            egui::pos2(rect.left(), rect.top() + 7.),
            egui::Align2::LEFT_CENTER,
            format!("Exporting {}", plural(n, "photo", "photos")),
            egui::FontId::proportional(11.),
            theme::gray(190),
        );
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + 17.),
            Vec2::new(rect.width() - 26., 4.),
        );
        painter.rect_filled(bar, 2., theme::gray(50));
        painter.rect_filled(
            egui::Rect::from_min_size(bar.min, Vec2::new(bar.width() * fraction, 4.)),
            2.,
            Color32::from_rgb(110, 150, 190),
        );
        let close = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 9., rect.center().y),
            Vec2::splat(18.),
        );
        let response = ui
            .interact(close, ui.id().with("cancel-export"), Sense::click())
            .on_hover_text("Cancel export");
        let color = theme::gray(if response.hovered() { 235 } else { 150 });
        crate::app::icons::paint_at(
            ui.painter(),
            crate::app::icons::Icon::Close,
            close.center(),
            13.,
            color,
        );
        if response.clicked() {
            for job in jobs.iter() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }
}

/// The watermark named in the Export dialog: a saved preset, or the Simple
/// Copyright Watermark, whose text comes from each photo at export.
fn watermark_for(name: &str) -> Option<crate::watermark::Watermark> {
    use crate::watermark::{SIMPLE_COPYRIGHT, Watermark, presets};
    if name == SIMPLE_COPYRIGHT {
        return Some(Watermark {
            name: SIMPLE_COPYRIGHT.into(),
            ..Default::default()
        });
    }
    presets().into_iter().find(|w| w.name == name)
}
