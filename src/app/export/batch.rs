//! Exporting several photos at once. Planning is separated from writing so the
//! files that already exist can be settled once for the whole batch, rather than
//! one dialog per exported photo.
use super::{Existing, ExportSettings, Replace, job};
use crate::app::library::EditSource;
use crate::export::{job::Photo, settings::unique};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

/// One photo, and the file it will be written to. The recipe is not resolved
/// here: it needs the file open, which the export thread does anyway.
#[derive(Clone)]
pub struct Write {
    pub source: PathBuf,
    pub edit: EditSource,
    pub values: crate::export::assemble::Values,
    pub watermark: Option<crate::watermark::Watermark>,
    pub target: PathBuf,
    pub replace: Replace,
}

/// Why a queued photo is waiting for an answer.
#[derive(Clone, Copy)]
struct Clash {
    /// Two selected photos resolved to one file, so the name has to move
    /// whatever the user says about files that were already there.
    duplicate: bool,
}

/// One queued photo, before the files that already exist have been settled.
#[derive(Clone)]
pub struct Entry {
    source: PathBuf,
    edit: EditSource,
    values: crate::export::assemble::Values,
    watermark: Option<crate::watermark::Watermark>,
    target: PathBuf,
    /// Set when the target needs an answer: either the file is already there, or
    /// another selected photo has already claimed the name.
    clash: Option<Clash>,
}

/// What a batch will do, once every target is known. Entries stay in the order
/// the photos were selected, which is the order they are exported in.
#[derive(Default, Clone)]
pub struct Plan {
    entries: Vec<Entry>,
}

impl Plan {
    /// The photos held back until the user has said what to do about them.
    fn clashes(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.clash.is_some())
    }
    /// How many photos the batch holds, whichever way each is written.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Works out where each photo will go. Nothing is written and no photo is
/// decoded, so a target that cannot be worked out fails the batch before any
/// work starts.
pub fn plan(photos: Vec<Queued>, settings: &ExportSettings) -> Result<Plan> {
    ensure!(!photos.is_empty(), "No photos to export");
    let mut entries = Vec::with_capacity(photos.len());
    // A second photo resolving to a name the batch already claimed would
    // overwrite the first, whatever the existing-files setting says.
    let mut claimed = HashSet::new();
    for photo in photos {
        let target = settings
            .target(&photo.source)
            .with_context(|| format!("No export folder for {}", photo.source.display()))?;
        let duplicate = !claimed.insert(target.clone());
        let clash = (duplicate || target.exists()).then_some(Clash { duplicate });
        entries.push(Entry {
            source: photo.source,
            edit: photo.edit,
            values: photo.values,
            watermark: photo.watermark,
            target,
            clash,
        });
    }
    Ok(Plan { entries })
}

/// A selected photo, before it has been opened.
pub struct Queued {
    pub source: PathBuf,
    pub edit: EditSource,
    pub values: crate::export::assemble::Values,
    pub watermark: Option<crate::watermark::Watermark>,
}

/// Resolves a queued photo into the one `job::run` writes: the file is opened
/// for its metadata, which is what the recipe and the render both need.
fn resolve_recipe(write: &Write) -> Result<Photo> {
    let raw = crate::raw::Raw::open(&write.source)?;
    Ok(Photo {
        image: None,
        source: write.source.clone(),
        recipe: EditSource::recipe(Some(&write.edit), &raw)?,
        values: write.values.clone(),
        watermark: write.watermark.clone(),
    })
}

/// Settles the files that already exist and returns everything to write, in the
/// order the photos were selected. `answer` is what the user chose for them;
/// `Existing::Ask` must have been settled before this, since nothing here can
/// ask.
pub fn resolve(plan: Plan, answer: Existing) -> Vec<Write> {
    plan.entries
        .into_iter()
        .filter_map(|entry| {
            let Some(clash) = entry.clash else {
                return Some(Write {
                    source: entry.source,
                    edit: entry.edit,
                    values: entry.values,
                    watermark: entry.watermark,
                    target: entry.target,
                    replace: Replace::NoClobber,
                });
            };
            match (clash.duplicate, answer) {
                // Skipped, and not counted here: the summary says how many.
                (_, Existing::Skip) => None,
                (true, _) => Some(Write {
                    source: entry.source,
                    edit: entry.edit,
                    values: entry.values,
                    watermark: entry.watermark,
                    target: unique(&entry.target),
                    replace: Replace::NoClobber,
                }),
                (false, Existing::Overwrite) => Some(Write {
                    source: entry.source,
                    edit: entry.edit,
                    values: entry.values,
                    watermark: entry.watermark,
                    target: entry.target,
                    replace: Replace::Overwrite,
                }),
                (false, _) => Some(Write {
                    source: entry.source,
                    edit: entry.edit,
                    values: entry.values,
                    watermark: entry.watermark,
                    target: unique(&entry.target),
                    replace: Replace::NoClobber,
                }),
            }
        })
        .collect()
}

/// How many photos a batch is holding back for an answer.
pub fn clashing(plan: &Plan) -> usize {
    plan.clashes().count()
}

/// How a batch ended, for the status line.
#[derive(Default)]
pub struct Outcome {
    pub exported: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
}

impl Outcome {
    /// The one line the status bar shows when a batch finishes.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("Exported {}", self.exported)];
        if self.skipped > 0 {
            parts.push(format!("{} skipped", self.skipped));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.cancelled {
            parts.push("cancelled".into());
        }
        parts.join(" · ")
    }
}

/// Writes each photo in turn, reporting how far along the batch is as
/// `(done, total, this photo's fraction)`. Stops between photos once `cancel` is
/// set, and the running export sees the same flag.
///
/// The first failure comes back with the outcome, so a batch that did not finish
/// still says why. One unreadable photo does not stop the rest.
pub fn run(
    writes: &[Write],
    settings: &ExportSettings,
    skipped: usize,
    cancel: &AtomicBool,
    progress: impl Fn(usize, usize, f32),
) -> (Outcome, Option<String>) {
    let total = writes.len();
    let mut outcome = Outcome {
        skipped,
        ..Default::default()
    };
    let mut first_error = None;
    for (index, write) in writes.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }
        progress(index, total, 0.);
        let photo = match resolve_recipe(write) {
            Ok(photo) => photo,
            Err(e) => {
                outcome.failed += 1;
                first_error.get_or_insert_with(|| {
                    format!(
                        "{}: {e:#}",
                        write
                            .source
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    )
                });
                continue;
            }
        };
        match job::run(
            photo,
            settings,
            &write.target,
            write.replace,
            cancel,
            |fraction| progress(index, total, fraction),
        ) {
            Ok(_) => outcome.exported += 1,
            Err(_) if cancel.load(Ordering::Relaxed) => {
                outcome.cancelled = true;
                break;
            }
            Err(e) => {
                outcome.failed += 1;
                first_error.get_or_insert_with(|| {
                    format!(
                        "{}: {e:#}",
                        write
                            .target
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    )
                });
            }
        }
    }
    (outcome, first_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::{Destination, Existing, ExportSettings, settings::unique};
    use std::{fs, path::Path};

    /// A photo named for `stem`; planning only ever looks at the source path.
    fn photo(stem: &str) -> Queued {
        Queued {
            source: Path::new("/photos").join(format!("{stem}.ARW")),
            edit: EditSource::Defaults(Default::default()),
            values: Default::default(),
            watermark: None,
        }
    }

    fn settings(folder: &Path) -> ExportSettings {
        ExportSettings {
            folder: Some(folder.to_path_buf()),
            destination: Destination::Folder,
            ..Default::default()
        }
    }

    /// The file names a batch would write, in order.
    fn names(writes: &[Write]) -> Vec<String> {
        writes
            .iter()
            .map(|w| w.target.file_name().unwrap().to_string_lossy().into())
            .collect()
    }

    #[test]
    fn a_batch_of_free_names_is_all_writes() -> Result<()> {
        let d = tempfile::tempdir()?;
        let plan = plan(vec![photo("a"), photo("b")], &settings(d.path()))?;
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.len() - clashing(&plan), 2);
        assert_eq!(clashing(&plan), 0);
        let writes = resolve(plan, Existing::Overwrite);
        assert!(writes.iter().all(|w| w.replace == Replace::NoClobber));
        assert_eq!(names(&writes), ["a.jpg", "b.jpg"]);
        Ok(())
    }

    #[test]
    fn an_empty_selection_is_not_a_batch() {
        let d = tempfile::tempdir().unwrap();
        assert!(plan(Vec::new(), &settings(d.path())).is_err());
    }

    #[test]
    fn existing_files_are_held_back_and_the_answer_applies_to_all() -> Result<()> {
        let d = tempfile::tempdir()?;
        fs::write(d.path().join("a.jpg"), b"old")?;
        fs::write(d.path().join("b.jpg"), b"old")?;
        let plan = plan(
            vec![photo("a"), photo("b"), photo("c")],
            &settings(d.path()),
        )?;
        assert_eq!(
            plan.len() - clashing(&plan),
            1,
            "only the free name is a write"
        );
        assert_eq!(clashing(&plan), 2);

        let overwrite = resolve(plan.clone(), Existing::Overwrite);
        assert_eq!(
            overwrite
                .iter()
                .filter(|w| w.replace == Replace::Overwrite)
                .count(),
            2
        );
        assert!(names(&overwrite).contains(&"a.jpg".to_string()));
        assert!(names(&overwrite).contains(&"b.jpg".to_string()));

        assert_eq!(names(&resolve(plan.clone(), Existing::Skip)), ["c.jpg"]);

        let renamed = resolve(plan, Existing::Unique);
        assert_eq!(renamed.len(), 3);
        let a = renamed
            .iter()
            .map(|w| &w.target)
            .find(|p| p.file_stem().unwrap().to_string_lossy().starts_with('a'))
            .unwrap();
        assert_eq!(a, &unique(&d.path().join("a.jpg")));
        assert_ne!(a, &d.path().join("a.jpg"));
        Ok(())
    }

    #[test]
    fn two_photos_one_name_are_renamed_even_when_overwriting() -> Result<()> {
        let d = tempfile::tempdir()?;
        let settings = settings(d.path());
        // Two selected photos in different folders that share a file name.
        let photos = vec![
            Queued {
                source: Path::new("/a").join("shot.ARW"),
                ..photo("x")
            },
            Queued {
                source: Path::new("/b").join("shot.ARW"),
                ..photo("x")
            },
        ];
        let plan = plan(photos, &settings)?;
        assert_eq!(
            plan.len() - clashing(&plan),
            1,
            "the first photo claims the name"
        );
        assert_eq!(clashing(&plan), 1);

        // Overwrite is about files that were already there, not about a second
        // photo landing on the first one's output.
        let writes = resolve(plan, Existing::Overwrite);
        assert_eq!(names(&writes), ["shot.jpg", "shot-2.jpg"]);
        assert!(writes.iter().all(|w| w.replace == Replace::NoClobber));
        Ok(())
    }

    #[test]
    fn the_order_photos_were_selected_is_kept() -> Result<()> {
        let d = tempfile::tempdir()?;
        let plan = plan(
            vec![photo("c"), photo("a"), photo("b")],
            &settings(d.path()),
        )?;
        assert_eq!(
            names(&resolve(plan, Existing::Skip)),
            ["c.jpg", "a.jpg", "b.jpg"]
        );
        Ok(())
    }

    #[test]
    fn the_order_survives_files_needing_an_answer() -> Result<()> {
        let d = tempfile::tempdir()?;
        fs::write(d.path().join("b.jpg"), b"old")?;
        fs::write(d.path().join("d.jpg"), b"old")?;
        // a, b (exists), c, d (exists): the answer must not reorder the rest.
        let plan = plan(
            vec![photo("a"), photo("b"), photo("c"), photo("d")],
            &settings(d.path()),
        )?;
        assert_eq!(plan.len() - clashing(&plan), 2);
        assert_eq!(clashing(&plan), 2);
        assert_eq!(
            names(&resolve(plan, Existing::Overwrite)),
            ["a.jpg", "b.jpg", "c.jpg", "d.jpg"]
        );
        Ok(())
    }

    #[test]
    fn a_summary_counts_what_happened() {
        let outcome = Outcome {
            exported: 8,
            skipped: 2,
            failed: 1,
            cancelled: false,
        };
        assert_eq!(outcome.summary(), "Exported 8 · 2 skipped · 1 failed");
        assert_eq!(Outcome::default().summary(), "Exported 0");
    }

    #[test]
    fn a_cancelled_batch_writes_nothing_and_is_not_a_failure() -> Result<()> {
        let d = tempfile::tempdir()?;
        let settings = settings(d.path());
        let writes = resolve(
            plan(vec![photo("a"), photo("b")], &settings)?,
            Existing::Overwrite,
        );
        let (outcome, error) = run(&writes, &settings, 0, &AtomicBool::new(true), |_, _, _| {});
        assert!(outcome.cancelled);
        assert_eq!(outcome.exported, 0);
        assert!(error.is_none(), "a cancel is not a failure: {error:?}");
        assert!(!d.path().join("a.jpg").exists());
        Ok(())
    }

    /// A committed synthetic chart, which is a real decodable RAW.
    fn chart(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/charts")
            .join(format!("{name}.dng"))
    }

    /// The end-to-end path: a real RAW queued, planned and written.
    #[test]
    fn a_batch_writes_every_photo_it_is_given() -> Result<()> {
        let d = tempfile::tempdir()?;
        let settings = ExportSettings {
            folder: Some(d.path().to_path_buf()),
            destination: Destination::Folder,
            // Small, so the test does not render a full-size chart twice.
            resize: true,
            long_edge: 64,
            ..Default::default()
        };
        let source = chart("synthetic-d65");
        if !source.exists() {
            return Ok(()); // charts are generated, not always present
        }
        // The same chart twice, under two names, so both are written.
        let a = d.path().join("a.dng");
        let b = d.path().join("b.dng");
        std::fs::copy(&source, &a)?;
        std::fs::copy(&source, &b)?;
        let queued = |path: &Path| Queued {
            source: path.to_path_buf(),
            edit: EditSource::Defaults(Default::default()),
            values: Default::default(),
            watermark: None,
        };
        let writes = resolve(
            plan(vec![queued(&a), queued(&b)], &settings)?,
            Existing::Overwrite,
        );
        assert_eq!(names(&writes), ["a.jpg", "b.jpg"]);

        let seen = std::cell::RefCell::new(Vec::new());
        let (outcome, error) = run(&writes, &settings, 0, &AtomicBool::new(false), |d, t, _| {
            seen.borrow_mut().push((d, t))
        });
        assert_eq!(error, None, "no photo should have failed");
        assert_eq!(outcome.exported, 2, "both photos are written");
        assert_eq!(outcome.failed, 0);
        assert!(!outcome.cancelled);
        // Each is a real JPEG of the size asked for.
        for name in ["a.jpg", "b.jpg"] {
            let written = d.path().join(name);
            assert!(written.exists(), "{name} was not written");
            let (width, _) = image::image_dimensions(&written)?;
            assert!(width <= 64, "{name} was not resized: {width}px wide");
        }
        assert!(seen.into_inner().iter().all(|(_, t)| *t == 2));
        Ok(())
    }

    #[test]
    fn a_photo_that_cannot_be_read_fails_alone_and_the_rest_are_written() -> Result<()> {
        let d = tempfile::tempdir()?;
        let settings = ExportSettings {
            folder: Some(d.path().to_path_buf()),
            destination: Destination::Folder,
            resize: true,
            long_edge: 64,
            ..Default::default()
        };
        let good = chart("synthetic-d65");
        if !good.exists() {
            return Ok(());
        }
        let a = d.path().join("a.dng");
        std::fs::copy(&good, &a)?;
        // Not a RAW at all: it cannot be decoded, so it must not stop the batch.
        let broken = d.path().join("broken.dng");
        std::fs::write(&broken, b"not a raw file")?;
        let queued = |path: &Path| Queued {
            source: path.to_path_buf(),
            edit: EditSource::Defaults(Default::default()),
            values: Default::default(),
            watermark: None,
        };
        let writes = resolve(
            plan(vec![queued(&broken), queued(&a)], &settings)?,
            Existing::Overwrite,
        );
        let (outcome, error) = run(&writes, &settings, 0, &AtomicBool::new(false), |_, _, _| {});
        assert_eq!(outcome.exported, 1, "the readable photo is still written");
        assert_eq!(outcome.failed, 1);
        // The reason is named once, by file.
        assert!(
            error.unwrap().contains("broken.dng"),
            "the failed photo is named"
        );
        assert!(d.path().join("a.jpg").exists());
        assert!(!d.path().join("broken.jpg").exists());
        Ok(())
    }

    #[test]
    fn progress_is_reported_across_the_whole_batch() -> Result<()> {
        let d = tempfile::tempdir()?;
        let settings = settings(d.path());
        let writes = resolve(
            plan(vec![photo("a"), photo("b")], &settings)?,
            Existing::Overwrite,
        );
        let seen = std::cell::RefCell::new(Vec::new());
        let _ = run(
            &writes,
            &settings,
            0,
            &AtomicBool::new(false),
            |done, total, _| {
                seen.borrow_mut().push((done, total));
            },
        );
        let seen = seen.into_inner();
        assert!(!seen.is_empty());
        assert!(
            seen.iter().all(|(_, total)| *total == 2),
            "the total is the batch, not the photo"
        );
        assert!(
            seen.windows(2).all(|w| w[0].0 <= w[1].0),
            "progress only moves on"
        );
        Ok(())
    }
}
