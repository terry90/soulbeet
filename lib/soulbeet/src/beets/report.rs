//! The report import.py writes after every run: the candidates beets weighed
//! for each task, and what kept a pick or an as-is import from running.

use crate::{DriverChoice, ImportMode, ImportOutcome, ImportResult, SkipReason};
use serde::Deserialize;
use shared::download::{MatchCandidate, MatchChoices, MatchKind, TrackFit};

#[derive(Debug)]
pub(crate) struct Report {
    /// One entry per task, in the order beets ran them.
    tasks: Vec<MatchChoices>,
    /// The picked source returned no single match.
    pick_missing: bool,
    /// Why an as-is import was not run.
    refused: Option<String>,
}

#[derive(Deserialize)]
struct RawReport {
    tasks: Vec<RawTask>,
    pick_missing: bool,
    refused: Option<String>,
}

#[derive(Deserialize)]
struct RawTask {
    album: bool,
    /// Read once the task's kind is known.
    candidates: Vec<serde_json::Value>,
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
        let tasks = raw
            .tasks
            .into_iter()
            .map(|task| {
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
            pick_missing: raw.pick_missing,
            refused: raw.refused,
        })
    }

    /// Completes the result read from beets' output with what only the
    /// driver knows.
    pub(crate) fn apply(
        self,
        mut result: ImportResult,
        mode: ImportMode,
        choice: &DriverChoice,
    ) -> ImportResult {
        if let Some(message) = self.refused {
            result.outcome = ImportOutcome::Failed { message };
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
        );
        assert_eq!(applied.outcome, no_match().outcome);
        let choices = applied.choices.expect("no choices");
        assert_eq!(choices.kind, MatchKind::Track);
        assert_eq!(choices.candidates.len(), 2);

        // No candidates at all still leaves a manual ID and as-is.
        let empty = r#"{"paths": ["/src/a"], "album": true, "candidates": []}"#;
        let applied =
            report(&[empty], false, None).apply(no_match(), ImportMode::Album, &DriverChoice::Auto);
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
            two.apply(no_match(), ImportMode::Album, &DriverChoice::Auto)
                .choices,
            None
        );
        let none = report(&[], false, None);
        assert_eq!(
            none.apply(no_match(), ImportMode::Album, &DriverChoice::Auto)
                .choices,
            None
        );

        let imported = result(ImportOutcome::Imported {
            untagged: false,
            warnings: vec![],
        });
        let already = result(ImportOutcome::Skipped(SkipReason::AlreadyInLibrary));
        for other in [imported, already] {
            let applied = report(&[ALBUM_TASK], false, None).apply(
                other.clone(),
                ImportMode::Album,
                &DriverChoice::Auto,
            );
            assert_eq!(applied, other);
        }

        for choice in [
            DriverChoice::AsIs,
            pick("6a09041b-0f79-3278-88d9-0552dd28d37f"),
        ] {
            let applied =
                report(&[ALBUM_TASK], false, None).apply(no_match(), ImportMode::Album, &choice);
            assert_eq!(applied, no_match());
        }
    }

    #[test]
    fn a_pick_that_found_nothing_names_the_source_and_id() {
        let id = "dfeb96a7-f78a-4b04-93ce-c39e28c9a173";
        let single = report(&[], true, None).apply(no_match(), ImportMode::Singleton, &pick(id));
        assert_eq!(
            single.outcome,
            ImportOutcome::Failed {
                message: format!("MusicBrainz has no recording {id}"),
            }
        );
        assert_eq!(single.output_tail, "tail");
        assert_eq!(single.choices, None);

        let album = report(&[], true, None).apply(no_match(), ImportMode::Album, &pick(id));
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
        let applied =
            report(&[], true, None).apply(failed.clone(), ImportMode::Singleton, &pick("x"));
        assert_eq!(applied, failed);
    }

    #[test]
    fn a_refused_as_is_import_fails_with_the_reason() {
        let why = "Import as-is needs artist and title tags, and 1 file lacks them: one.mp3";
        let applied = report(&[], false, Some(why)).apply(
            result(ImportOutcome::Skipped(SkipReason::NothingToImport)),
            ImportMode::Singleton,
            &DriverChoice::AsIs,
        );
        assert_eq!(
            applied.outcome,
            ImportOutcome::Failed {
                message: why.into()
            }
        );
        assert_eq!(applied.to_string(), why);
    }
}
