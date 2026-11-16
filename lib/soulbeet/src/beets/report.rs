//! The report import.py writes after every run: the candidates beets weighed
//! for each task and the files it read, what kept a pick or an as-is import
//! from running, what beets did with each duplicate it found or the removal it
//! wanted instead, and each file it imported.

use crate::{
    DriverChoice, FailKind, ImportMode, ImportOutcome, ImportResult, ImportedItem, SkipReason,
    SourceFile, WantedRemoval,
};
use serde::Deserialize;
use shared::download::{MatchCandidate, MatchChoices, MatchKind, TrackFit};
use shared::failure::{cut_chars, TEXT_MAX_CHARS};
use std::path::{Path, PathBuf};

/// How many source files a result keeps: their tags are a peer's text, and an album folder
/// rarely holds more.
const SOURCE_FILES_MAX: usize = 200;

#[derive(Debug)]
pub(crate) struct Report {
    /// One entry per task, in the order beets ran them.
    tasks: Vec<MatchChoices>,
    /// The files of every task, in the same order.
    files: Vec<SourceFile>,
    /// The picked source returned no single match.
    pick_missing: bool,
    /// Why an as-is import was not run.
    refused: Option<String>,
    /// One entry per duplicate decision beets logged as replace, keep or
    /// skip, or per removal it wanted.
    duplicates: Vec<RawDuplicate>,
    /// One entry per file beets imported, once it was in place.
    items: Vec<RawItem>,
}

#[derive(Deserialize)]
struct RawReport {
    tasks: Vec<RawTask>,
    pick_missing: bool,
    refused: Option<String>,
    duplicates: Vec<RawDuplicate>,
    items: Vec<RawItem>,
}

/// One duplicate decision, with the library files it is about.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RawDuplicate {
    Replaced {
        existing: Vec<PathBuf>,
    },
    Kept {
        existing: Vec<PathBuf>,
    },
    Skipped {
        existing: Vec<RawItem>,
    },
    /// beets kept the earlier items it would have removed.
    RemovalWanted {
        existing: Vec<RawItem>,
        /// Empty until the task's files were in place.
        imported: Vec<RawImported>,
    },
}

#[derive(Debug, Deserialize)]
struct RawItem {
    /// Where beets stored the file.
    path: PathBuf,
    recording_mbid: Option<String>,
    artist: String,
    title: String,
    beets_id: u64,
    release_mbid: Option<String>,
    album: String,
    match_per_mille: Option<u16>,
}

impl RawItem {
    fn imported_item(self, target: &Path) -> ImportedItem {
        ImportedItem {
            path: in_library(&self.path, target),
            recording_mbid: self.recording_mbid,
            artist: self.artist,
            title: self.title,
            beets_id: Some(self.beets_id),
            release_mbid: self.release_mbid,
            album: self.album,
            match_per_mille: self.match_per_mille,
        }
    }
}

/// `file` with each tag cut to `TEXT_MAX_CHARS`.
fn bounded(file: SourceFile) -> SourceFile {
    let cut = |text| cut_chars(text, TEXT_MAX_CHARS);
    SourceFile {
        artist: cut(file.artist),
        title: cut(file.title),
        album: cut(file.album),
        ..file
    }
}

#[derive(Debug, Deserialize)]
struct RawImported {
    id: u64,
    path: PathBuf,
}

/// A file beets reported, relative to `target` when it is inside it.
fn in_library(path: &Path, target: &Path) -> PathBuf {
    path.strip_prefix(target).unwrap_or(path).to_path_buf()
}

/// What became of the library's earlier copy of an import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateNote<'a> {
    Replaced,
    Kept,
    /// Deleted before a later file of the same copy could not be.
    Removed,
    /// Kept because its removal was refused, for this reason.
    Blocked(&'a str),
}

/// The note an imported row gets about the library's earlier copy
/// `existing`, each file named relative to `target` when it is inside it.
pub fn duplicate_note(existing: &[PathBuf], target: &Path, note: DuplicateNote<'_>) -> String {
    let files = existing
        .iter()
        .map(|p| in_library(p, target).to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    match note {
        DuplicateNote::Replaced => format!("replaced the library's earlier copy {files}"),
        DuplicateNote::Kept => format!("kept the library's earlier copy {files} too"),
        DuplicateNote::Removed => format!("removed the library's earlier copy {files}"),
        DuplicateNote::Blocked(reason) => {
            format!("kept the library's earlier copy {files}: {reason}")
        }
    }
}

#[derive(Deserialize)]
struct RawTask {
    album: bool,
    /// Read once the task's kind is known.
    candidates: Vec<serde_json::Value>,
    /// The task's files with the tags beets read; a report may leave them out.
    #[serde(default)]
    files: Vec<SourceFile>,
}

#[derive(Deserialize)]
struct Common {
    data_source: String,
    id: String,
    artist: String,
    year: Option<u32>,
    country: Option<String>,
    disambig: Option<String>,
    per_mille: u16,
    differs: Vec<String>,
}

#[derive(Deserialize)]
struct AlbumCandidate {
    #[serde(flatten)]
    common: Common,
    album: String,
    track_count: u32,
    matched: u32,
    extra_items: u32,
}

#[derive(Deserialize)]
struct TrackCandidate {
    #[serde(flatten)]
    common: Common,
    title: String,
    album: Option<String>,
    length: Option<u32>,
}

impl Common {
    fn candidate(
        self,
        title: String,
        album: Option<String>,
        length_secs: Option<u32>,
        tracks: Option<TrackFit>,
    ) -> MatchCandidate {
        MatchCandidate {
            data_source: self.data_source,
            id: self.id,
            artist: self.artist,
            title,
            album,
            year: self.year,
            country: self.country,
            disambig: self.disambig,
            per_mille: self.per_mille,
            differs: self.differs,
            length_secs,
            tracks,
        }
    }
}

fn candidate(kind: MatchKind, value: serde_json::Value) -> serde_json::Result<MatchCandidate> {
    Ok(match kind {
        MatchKind::Album => {
            let c: AlbumCandidate = serde_json::from_value(value)?;
            let fit = TrackFit {
                matched: c.matched,
                total: c.track_count,
                extra_files: c.extra_items,
            };
            c.common.candidate(c.album, None, None, Some(fit))
        }
        MatchKind::Track => {
            let c: TrackCandidate = serde_json::from_value(value)?;
            c.common.candidate(c.title, c.album, c.length, None)
        }
    })
}

impl Report {
    pub(crate) fn parse(text: &str) -> serde_json::Result<Report> {
        let raw: RawReport = serde_json::from_str(text)?;
        let mut files = Vec::new();
        let tasks = raw
            .tasks
            .into_iter()
            .map(|task| {
                files.extend(task.files);
                let kind = match task.album {
                    true => MatchKind::Album,
                    false => MatchKind::Track,
                };
                let candidates = task
                    .candidates
                    .into_iter()
                    .map(|c| candidate(kind, c))
                    .collect::<serde_json::Result<_>>()?;
                Ok(MatchChoices { kind, candidates })
            })
            .collect::<serde_json::Result<_>>()?;
        Ok(Report {
            tasks,
            files,
            pick_missing: raw.pick_missing,
            refused: raw.refused,
            duplicates: raw.duplicates,
            items: raw.items,
        })
    }

    /// Completes the result read from beets' output with what only the
    /// driver knows, `target` being the library folder.
    pub(crate) fn apply(
        self,
        mut result: ImportResult,
        mode: ImportMode,
        choice: &DriverChoice,
        target: &Path,
    ) -> ImportResult {
        result.files = self
            .files
            .into_iter()
            .take(SOURCE_FILES_MAX)
            .map(bounded)
            .collect();
        let mut decisions = Vec::new();
        for d in self.duplicates {
            match d {
                RawDuplicate::RemovalWanted { existing, imported } => {
                    result.removals.push(WantedRemoval {
                        existing: existing
                            .into_iter()
                            .map(|i| i.imported_item(target))
                            .collect(),
                        imported: imported
                            .into_iter()
                            .map(|i| (i.id, in_library(&i.path, target)))
                            .collect(),
                    })
                }
                other => decisions.push(other),
            }
        }
        if let Some(message) = self.refused {
            result.outcome = ImportOutcome::Failed {
                kind: FailKind::AsIsMissingTags,
                message,
            };
            return result;
        }
        if let ImportOutcome::Imported {
            warnings, items, ..
        } = &mut result.outcome
        {
            // A skipped decision gets no note: it is the outcome's own reason,
            // since a run that logs one is `Skipped(AlreadyInLibrary)`
            // (`classify.rs` `exited_cleanly`).
            warnings.extend(decisions.iter().filter_map(|d| match d {
                RawDuplicate::Replaced { existing } => {
                    Some(duplicate_note(existing, target, DuplicateNote::Replaced))
                }
                RawDuplicate::Kept { existing } => {
                    Some(duplicate_note(existing, target, DuplicateNote::Kept))
                }
                RawDuplicate::Skipped { .. } | RawDuplicate::RemovalWanted { .. } => None,
            }));
            *items = self
                .items
                .into_iter()
                .map(|i| i.imported_item(target))
                .collect();
            return result;
        }
        if let ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { .. }) = result.outcome {
            let existing = decisions
                .into_iter()
                .filter_map(|d| match d {
                    RawDuplicate::Skipped { existing } => Some(existing),
                    _ => None,
                })
                .flatten()
                .map(|i| i.imported_item(target))
                .collect();
            result.outcome = ImportOutcome::Skipped(SkipReason::AlreadyInLibrary { existing });
            return result;
        }
        // A pick that found nothing and a weak automatic match both end in a
        // skip; any other outcome says more than the report does.
        if !matches!(
            result.outcome,
            ImportOutcome::Skipped(SkipReason::NoMatch { .. })
        ) {
            return result;
        }
        match choice {
            DriverChoice::Auto => {
                if let Ok([task]) = <[MatchChoices; 1]>::try_from(self.tasks) {
                    result.choices = Some(task);
                }
            }
            DriverChoice::Pick { data_source, id } if self.pick_missing => {
                let entity = match mode {
                    ImportMode::Singleton => "recording",
                    ImportMode::Album => "release",
                };
                result.outcome = ImportOutcome::Failed {
                    kind: FailKind::ChoiceInvalid,
                    message: format!("{data_source} has no {entity} {id}"),
                };
            }
            DriverChoice::Pick { .. } | DriverChoice::AsIs => {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskCount;

    const ALBUM_TASK: &str = r#"{"paths": ["/src/a"], "album": true, "candidates": [
        {"data_source": "MusicBrainz", "id": "6a09041b-0f79-3278-88d9-0552dd28d37f",
         "artist": "Radiohead", "album": "OK Computer", "year": 1997, "country": "US",
         "disambig": null, "per_mille": 526, "differs": ["missing tracks"],
         "track_count": 12, "matched": 2, "extra_items": 1}]}"#;

    const TRACK_TASK: &str = r#"{"paths": ["/src/a/one.mp3"], "album": false, "candidates": [
        {"data_source": "MusicBrainz", "id": "dfeb96a7-f78a-4b04-93ce-c39e28c9a173",
         "artist": "Radiohead", "title": "Karma Police", "album": "OK Computer",
         "year": null, "country": null, "disambig": "live", "per_mille": 714,
         "differs": ["length"], "length": 263},
        {"data_source": "FakeSrc", "id": "fake-t-w0", "artist": "", "title": "",
         "album": null, "year": null, "country": null, "disambig": null,
         "per_mille": 215, "differs": ["title", "length", "artist"], "length": null}]}"#;

    fn report(tasks: &[&str], pick_missing: bool, refused: Option<&str>) -> Report {
        let text = serde_json::json!({
            "tasks": tasks
                .iter()
                .map(|t| serde_json::from_str::<serde_json::Value>(t).unwrap())
                .collect::<Vec<_>>(),
            "pick_missing": pick_missing,
            "refused": refused,
            "duplicates": [],
            "items": [],
        });
        Report::parse(&text.to_string()).unwrap()
    }

    fn result(outcome: ImportOutcome) -> ImportResult {
        ImportResult {
            outcome,
            output_tail: "tail".into(),
            tasks: TaskCount {
                imported: 0,
                total: 1,
            },
            choices: None,
            user_action: None,
            removals: vec![],
            files: vec![],
        }
    }

    fn no_match() -> ImportResult {
        result(ImportOutcome::Skipped(SkipReason::NoMatch {
            candidates: Some(1),
        }))
    }

    fn pick(id: &str) -> DriverChoice {
        DriverChoice::Pick {
            data_source: "MusicBrainz".into(),
            id: id.into(),
        }
    }

    #[test]
    fn album_candidates_name_the_album_and_how_the_files_fit() {
        let parsed = report(&[ALBUM_TASK], false, None);
        assert_eq!(
            parsed.tasks,
            [MatchChoices {
                kind: MatchKind::Album,
                candidates: vec![MatchCandidate {
                    data_source: "MusicBrainz".into(),
                    id: "6a09041b-0f79-3278-88d9-0552dd28d37f".into(),
                    artist: "Radiohead".into(),
                    title: "OK Computer".into(),
                    album: None,
                    year: Some(1997),
                    country: Some("US".into()),
                    disambig: None,
                    per_mille: 526,
                    differs: vec!["missing tracks".into()],
                    length_secs: None,
                    tracks: Some(TrackFit {
                        matched: 2,
                        total: 12,
                        extra_files: 1,
                    }),
                }],
            }]
        );
    }

    #[test]
    fn track_candidates_keep_their_album_length_and_order() {
        let parsed = report(&[TRACK_TASK], false, None);
        let [task] = &parsed.tasks[..] else {
            panic!("{parsed:?}")
        };
        assert_eq!(task.kind, MatchKind::Track);
        let ids: Vec<&str> = task.candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["dfeb96a7-f78a-4b04-93ce-c39e28c9a173", "fake-t-w0"]);
        let best = &task.candidates[0];
        assert_eq!(best.title, "Karma Police");
        assert_eq!(best.album.as_deref(), Some("OK Computer"));
        assert_eq!(best.disambig.as_deref(), Some("live"));
        assert_eq!(best.length_secs, Some(263));
        assert_eq!(best.tracks, None);
        assert_eq!(task.candidates[1].length_secs, None);
        assert_eq!(task.candidates[1].album, None);
    }

    #[test]
    fn a_candidate_that_does_not_fit_its_kind_fails_the_report() {
        // An album task whose candidate is shaped like a track.
        let text = ALBUM_TASK.replace(r#""album": true"#, r#""album": false"#);
        let wrong = format!(r#"{{"tasks": [{text}], "pick_missing": false, "refused": null}}"#);
        assert!(Report::parse(&wrong).is_err());
        assert!(Report::parse("").is_err());
        assert!(Report::parse(r#"{"tasks": []}"#).is_err());
    }

    #[test]
    fn a_weak_automatic_import_of_one_task_offers_its_candidates() {
        let applied = report(&[TRACK_TASK], false, None).apply(
            no_match(),
            ImportMode::Singleton,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        assert_eq!(applied.outcome, no_match().outcome);
        let choices = applied.choices.expect("no choices");
        assert_eq!(choices.kind, MatchKind::Track);
        assert_eq!(choices.candidates.len(), 2);

        // No candidates at all still leaves a manual ID and as-is.
        let empty = r#"{"paths": ["/src/a"], "album": true, "candidates": []}"#;
        let applied = report(&[empty], false, None).apply(
            no_match(),
            ImportMode::Album,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        assert_eq!(
            applied.choices,
            Some(MatchChoices {
                kind: MatchKind::Album,
                candidates: vec![],
            })
        );
    }

    #[test]
    fn choices_need_one_skipped_task_from_an_automatic_import() {
        let two = report(&[ALBUM_TASK, ALBUM_TASK], false, None);
        assert_eq!(
            two.apply(
                no_match(),
                ImportMode::Album,
                &DriverChoice::Auto,
                Path::new("/music")
            )
            .choices,
            None
        );
        let none = report(&[], false, None);
        assert_eq!(
            none.apply(
                no_match(),
                ImportMode::Album,
                &DriverChoice::Auto,
                Path::new("/music")
            )
            .choices,
            None
        );

        let imported = result(ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
            items: vec![],
            failure: None,
        });
        let already = result(ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
            existing: vec![],
        }));
        for other in [imported, already] {
            let applied = report(&[ALBUM_TASK], false, None).apply(
                other.clone(),
                ImportMode::Album,
                &DriverChoice::Auto,
                Path::new("/music"),
            );
            assert_eq!(applied, other);
        }

        for choice in [
            DriverChoice::AsIs,
            pick("6a09041b-0f79-3278-88d9-0552dd28d37f"),
        ] {
            let applied = report(&[ALBUM_TASK], false, None).apply(
                no_match(),
                ImportMode::Album,
                &choice,
                Path::new("/music"),
            );
            assert_eq!(applied, no_match());
        }
    }

    #[test]
    fn a_pick_that_found_nothing_names_the_source_and_id() {
        let id = "dfeb96a7-f78a-4b04-93ce-c39e28c9a173";
        let single = report(&[], true, None).apply(
            no_match(),
            ImportMode::Singleton,
            &pick(id),
            Path::new("/music"),
        );
        assert_eq!(
            single.outcome,
            ImportOutcome::Failed {
                kind: FailKind::ChoiceInvalid,
                message: format!("MusicBrainz has no recording {id}"),
            }
        );
        assert_eq!(single.output_tail, "tail");
        assert_eq!(single.choices, None);

        let album = report(&[], true, None).apply(
            no_match(),
            ImportMode::Album,
            &pick(id),
            Path::new("/music"),
        );
        assert_eq!(
            album.to_string(),
            format!("MusicBrainz has no release {id}")
        );
    }

    #[test]
    fn a_lookup_failure_during_a_pick_is_not_a_missing_id() {
        let failed = result(ImportOutcome::LookupFailed {
            source: "MusicBrainz".into(),
            message: "400 Client Error".into(),
        });
        let applied = report(&[], true, None).apply(
            failed.clone(),
            ImportMode::Singleton,
            &pick("x"),
            Path::new("/music"),
        );
        assert_eq!(applied, failed);
    }

    #[test]
    fn an_imported_result_names_the_library_files_its_duplicates_touched() {
        let text = r#"{"tasks": [], "pick_missing": false, "refused": null, "items": [], "duplicates": [
            {"action": "replaced", "existing": ["/music/Zzqx Artist/Qzz Album/01 Song Number 1.mp3", "/elsewhere/02.mp3"]},
            {"action": "skipped", "existing": [{"path": "/music/Zzqx Artist/Qzz Album/03 Song Number 3.mp3",
              "recording_mbid": null, "artist": "Zzqx Artist", "title": "Song Number 3", "beets_id": 3,
              "release_mbid": null, "album": "Qzz Album", "match_per_mille": null}]},
            {"action": "kept", "existing": ["/music/Zzqx Artist/Qzz Album/04 Song Number 4.mp3"]}]}"#;
        let imported = result(ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
            items: vec![],
            failure: None,
        });
        let applied = Report::parse(text).unwrap().apply(
            imported,
            ImportMode::Singleton,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        // A skipped decision is the outcome's own reason in a skipped run, so
        // it gives an imported row no note.
        let ImportOutcome::Imported { warnings, .. } = applied.outcome else {
            panic!("{applied:?}")
        };
        assert_eq!(
            warnings,
            [
                "replaced the library's earlier copy Zzqx Artist/Qzz Album/01 Song Number 1.mp3, /elsewhere/02.mp3",
                "kept the library's earlier copy Zzqx Artist/Qzz Album/04 Song Number 4.mp3 too",
            ]
        );
        assert_eq!(applied.removals, vec![]);
    }

    #[test]
    fn a_wanted_removal_is_carried_as_data_and_not_as_a_note() {
        let text = r#"{"tasks": [], "pick_missing": false, "refused": null, "items": [], "duplicates": [
            {"action": "removal_wanted", "existing": [{"path": "/music/Marrow Choir/Winter Songs/03 O Holy Night.flac",
              "recording_mbid": "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c", "artist": "Marrow Choir", "title": "O Holy Night",
              "beets_id": 4, "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f", "album": "Winter Songs",
              "match_per_mille": null}],
             "imported": [{"id": 7, "path": "/music/Marrow Choir/Winter Songs [1]/03 O Holy Night.flac"}]},
            {"action": "kept", "existing": ["/music/Marrow Choir/Winter Songs/04 Silent Night.flac"]}]}"#;
        let imported = result(ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
            items: vec![],
            failure: None,
        });
        let result = Report::parse(text).unwrap().apply(
            imported,
            ImportMode::Singleton,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        assert_eq!(
            result.removals,
            vec![WantedRemoval {
                existing: vec![ImportedItem {
                    path: "Marrow Choir/Winter Songs/03 O Holy Night.flac".into(),
                    recording_mbid: Some("0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c".into()),
                    artist: "Marrow Choir".into(),
                    title: "O Holy Night".into(),
                    beets_id: Some(4),
                    release_mbid: Some("6a09041b-0f79-3278-88d9-0552dd28d37f".into()),
                    album: "Winter Songs".into(),
                    match_per_mille: None,
                }],
                imported: vec![(
                    7,
                    PathBuf::from("Marrow Choir/Winter Songs [1]/03 O Holy Night.flac")
                )],
            }]
        );
        let ImportOutcome::Imported { warnings, .. } = result.outcome else {
            panic!()
        };
        assert_eq!(
            warnings,
            vec![
                "kept the library's earlier copy Marrow Choir/Winter Songs/04 Silent Night.flac too"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_wanted_removal_is_carried_whatever_the_outcome() {
        let text = r#"{"tasks": [], "pick_missing": false, "refused": null, "items": [], "duplicates": [
            {"action": "removal_wanted", "existing": [{"path": "/music/A/x.flac", "recording_mbid": null,
              "artist": "A", "title": "x", "beets_id": 2, "release_mbid": null, "album": "B",
              "match_per_mille": null}], "imported": []}]}"#;
        let failed = result(ImportOutcome::Failed {
            kind: FailKind::Beets,
            message: "beets exited with code 1".into(),
        });
        let applied = Report::parse(text).unwrap().apply(
            failed.clone(),
            ImportMode::Album,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        assert_eq!(applied.outcome, failed.outcome);
        assert_eq!(
            applied.removals,
            vec![WantedRemoval {
                existing: vec![ImportedItem {
                    path: "A/x.flac".into(),
                    recording_mbid: None,
                    artist: "A".into(),
                    title: "x".into(),
                    beets_id: Some(2),
                    release_mbid: None,
                    album: "B".into(),
                    match_per_mille: None,
                }],
                imported: vec![],
            }]
        );
    }

    #[test]
    fn every_duplicate_note_names_its_files_inside_the_library() {
        let files = [
            PathBuf::from("/music/A/x.flac"),
            PathBuf::from("/elsewhere/y.flac"),
        ];
        let note = |n| duplicate_note(&files, Path::new("/music"), n);
        assert_eq!(
            note(DuplicateNote::Replaced),
            "replaced the library's earlier copy A/x.flac, /elsewhere/y.flac"
        );
        assert_eq!(
            note(DuplicateNote::Kept),
            "kept the library's earlier copy A/x.flac, /elsewhere/y.flac too"
        );
        assert_eq!(
            note(DuplicateNote::Removed),
            "removed the library's earlier copy A/x.flac, /elsewhere/y.flac"
        );
        assert_eq!(
            note(DuplicateNote::Blocked(
                "Another listener rated or starred this track"
            )),
            "kept the library's earlier copy A/x.flac, /elsewhere/y.flac: Another listener rated or starred this track"
        );
    }

    #[test]
    fn a_replacing_import_says_so_in_its_text() {
        let text = r#"{"tasks": [], "pick_missing": false, "refused": null, "items": [], "duplicates": [
            {"action": "replaced",
             "existing": ["/music/Zzqx Artist/Qzz Album/01 Song Number 1.mp3"]}]}"#;
        let target = Path::new("/music");
        let imported = result(ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
            items: vec![],
            failure: None,
        });
        let applied = Report::parse(text).unwrap().apply(
            imported,
            ImportMode::Album,
            &DriverChoice::Auto,
            target,
        );
        assert_eq!(
            applied.to_string(),
            "Imported; replaced the library's earlier copy Zzqx Artist/Qzz Album/01 Song Number 1.mp3"
        );

        // A merge ends in a skip with no match.
        let already = result(ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
            existing: vec![],
        }));
        let merged = result(ImportOutcome::Skipped(SkipReason::NoMatch {
            candidates: None,
        }));
        for other in [already, merged] {
            let applied = Report::parse(text).unwrap().apply(
                other.clone(),
                ImportMode::Album,
                &DriverChoice::Auto,
                target,
            );
            assert_eq!(applied, other);
        }
    }

    #[test]
    fn a_skipped_duplicate_names_the_library_files_it_kept() {
        let text = r#"{"tasks": [], "pick_missing": false, "refused": null, "items": [], "duplicates": [
            {"action": "skipped", "existing": [{"path": "/music/Zzqx Artist/Qzz Album/01 Song Number 1.mp3",
              "recording_mbid": null, "artist": "Zzqx Artist", "title": "Song Number 1", "beets_id": 1,
              "release_mbid": null, "album": "Qzz Album", "match_per_mille": null}]}]}"#;
        let skipped = result(ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
            existing: vec![],
        }));
        let applied = Report::parse(text).unwrap().apply(
            skipped,
            ImportMode::Album,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        assert_eq!(
            applied.outcome,
            ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
                existing: vec![ImportedItem {
                    path: "Zzqx Artist/Qzz Album/01 Song Number 1.mp3".into(),
                    recording_mbid: None,
                    artist: "Zzqx Artist".into(),
                    title: "Song Number 1".into(),
                    beets_id: Some(1),
                    release_mbid: None,
                    album: "Qzz Album".into(),
                    match_per_mille: None,
                }],
            })
        );
        assert_eq!(
            applied.to_string(),
            "Already in library: Zzqx Artist/Qzz Album/01 Song Number 1.mp3"
        );
        assert_eq!(
            result(ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
                existing: vec![]
            }))
            .to_string(),
            "Already in library"
        );
    }

    #[test]
    fn a_skipped_duplicate_and_the_task_files_come_back_typed() {
        let text = r#"{"tasks": [{"paths": ["/dl/X [1f2e3d4c]/one.mp3"], "album": false, "candidates": [],
            "files": [{"path": "/dl/X [1f2e3d4c]/one.mp3", "artist": "Zzqx Artist", "title": "Song Number 1",
                       "album": "", "track": null, "length_secs": 2}]}],
            "pick_missing": false, "refused": null, "items": [], "duplicates": [
            {"action": "skipped", "existing": [{"path": "/music/Zzqx Artist/Qzz Album/01 Song Number 1.mp3",
              "recording_mbid": "dfeb96a7-f78a-4b04-93ce-c39e28c9a173", "artist": "Zzqx Artist", "title": "Song Number 1",
              "beets_id": 7, "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f", "album": "Qzz Album",
              "match_per_mille": null}]}]}"#;
        let skipped = result(ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
            existing: vec![],
        }));
        let applied = Report::parse(text).unwrap().apply(
            skipped,
            ImportMode::Singleton,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        assert_eq!(
            applied.outcome,
            ImportOutcome::Skipped(SkipReason::AlreadyInLibrary {
                existing: vec![ImportedItem {
                    path: "Zzqx Artist/Qzz Album/01 Song Number 1.mp3".into(),
                    recording_mbid: Some("dfeb96a7-f78a-4b04-93ce-c39e28c9a173".into()),
                    artist: "Zzqx Artist".into(),
                    title: "Song Number 1".into(),
                    beets_id: Some(7),
                    release_mbid: Some("6a09041b-0f79-3278-88d9-0552dd28d37f".into()),
                    album: "Qzz Album".into(),
                    match_per_mille: None,
                }]
            })
        );
        assert_eq!(
            applied.to_string(),
            "Already in library: Zzqx Artist/Qzz Album/01 Song Number 1.mp3"
        );
        assert_eq!(
            applied.files,
            vec![SourceFile {
                path: "/dl/X [1f2e3d4c]/one.mp3".into(),
                artist: "Zzqx Artist".into(),
                title: "Song Number 1".into(),
                album: String::new(),
                track: None,
                length_secs: Some(2),
            }]
        );
    }

    #[test]
    fn the_task_files_are_kept_bounded_whatever_the_outcome() {
        let long = "x".repeat(TEXT_MAX_CHARS + 1);
        let file = |n: usize| {
            serde_json::json!({"path": format!("/dl/X/{n:03}.mp3"), "artist": long, "title": "t",
                               "album": "a", "track": n, "length_secs": null})
        };
        let task = |range: std::ops::Range<usize>| {
            serde_json::json!({"paths": [], "album": true, "candidates": [],
                               "files": range.map(file).collect::<Vec<_>>()})
        };
        let text = serde_json::json!({"tasks": [task(0..150), task(150..210)], "pick_missing": false,
                                      "refused": null, "duplicates": [], "items": []});
        let imported = result(ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
            items: vec![],
            failure: None,
        });
        let applied = Report::parse(&text.to_string()).unwrap().apply(
            imported,
            ImportMode::Album,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        assert_eq!(applied.files.len(), SOURCE_FILES_MAX);
        assert_eq!(
            applied.files.last().unwrap().path,
            PathBuf::from(format!("/dl/X/{:03}.mp3", SOURCE_FILES_MAX - 1))
        );
        assert!(applied
            .files
            .iter()
            .all(|f| f.artist.chars().count() == TEXT_MAX_CHARS && f.track.is_some()));
    }

    #[test]
    fn an_imported_result_lists_its_items_relative_to_the_library() {
        let text = r#"{"tasks": [], "pick_missing": false, "refused": null, "duplicates": [], "items": [
            {"path": "/music/Zzqx Artist/Song Number 1.mp3", "recording_mbid": "dfeb96a7-f78a-4b04-93ce-c39e28c9a173",
             "artist": "Zzqx Artist", "title": "Song Number 1", "beets_id": 5,
             "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f", "album": "Qzz Album", "match_per_mille": 962},
            {"path": "/elsewhere/02.mp3", "recording_mbid": null, "artist": "Zzqx Artist", "title": "Song Number 2",
             "beets_id": 6, "release_mbid": null, "album": "", "match_per_mille": null}]}"#;
        let imported = result(ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
            items: vec![],
            failure: None,
        });
        let applied = Report::parse(text).unwrap().apply(
            imported,
            ImportMode::Singleton,
            &DriverChoice::Auto,
            Path::new("/music"),
        );
        let ImportOutcome::Imported { items, .. } = applied.outcome else {
            panic!("{applied:?}")
        };
        assert_eq!(
            items,
            vec![
                ImportedItem {
                    path: "Zzqx Artist/Song Number 1.mp3".into(),
                    recording_mbid: Some("dfeb96a7-f78a-4b04-93ce-c39e28c9a173".into()),
                    artist: "Zzqx Artist".into(),
                    title: "Song Number 1".into(),
                    beets_id: Some(5),
                    release_mbid: Some("6a09041b-0f79-3278-88d9-0552dd28d37f".into()),
                    album: "Qzz Album".into(),
                    match_per_mille: Some(962),
                },
                ImportedItem {
                    path: "/elsewhere/02.mp3".into(),
                    recording_mbid: None,
                    artist: "Zzqx Artist".into(),
                    title: "Song Number 2".into(),
                    beets_id: Some(6),
                    release_mbid: None,
                    album: String::new(),
                    match_per_mille: None,
                },
            ]
        );
    }

    #[test]
    fn a_refused_as_is_import_fails_with_the_reason() {
        let why = "Import as-is needs artist and title tags, and 1 file lacks them: one.mp3";
        let applied = report(&[], false, Some(why)).apply(
            result(ImportOutcome::Skipped(SkipReason::NothingToImport)),
            ImportMode::Singleton,
            &DriverChoice::AsIs,
            Path::new("/music"),
        );
        assert_eq!(
            applied.outcome,
            ImportOutcome::Failed {
                kind: FailKind::AsIsMissingTags,
                message: why.into()
            }
        );
        assert_eq!(applied.to_string(), why);
    }
}
