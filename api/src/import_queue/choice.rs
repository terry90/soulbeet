//! Turns what the user chose for a skipped import into what the importer
//! runs, refusing anything the kept job does not allow.

#![allow(
    clippy::result_large_err,
    reason = "import refusals carry a Failure by value, as core functions do"
)]

use super::ImportJob;
use shared::download::{ImportChoice, ImportRefusal, KeptImport};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::{DriverChoice, ImportMode};

const NOTHING_TO_RETRY: &str = "Nothing to retry";
const FILES_GONE: &str = "The kept files are gone";
const NO_CHOICES: &str = "This import has no matches to pick from";
const UNKNOWN_CANDIDATE: &str = "That match is not one beets found for this import";
const NOT_AN_ID: &str = "That is not a MusicBrainz link or ID";

/// beets' name for its MusicBrainz source.
const MUSICBRAINZ: &str = "MusicBrainz";

/// The entities of musicbrainz.org links, by their path segment.
const MUSICBRAINZ_ENTITIES: [&str; 15] = [
    "area",
    "artist",
    "collection",
    "event",
    "genre",
    "instrument",
    "label",
    "place",
    "recording",
    "release",
    "release-group",
    "series",
    "track",
    "url",
    "work",
];

fn refusal(
    code: FailureCode,
    stage: Stage,
    retry: RetryAdvice,
    message: impl Into<String>,
    kept: KeptImport,
) -> ImportRefusal {
    ImportRefusal {
        failure: Failure::new(code, stage, retry, message),
        kept,
    }
}

/// The row has no kept job: none was kept, it was lost on restart or
/// dropped as the oldest, or a retry from another row of it took it.
pub(super) fn nothing_to_retry() -> ImportRefusal {
    refusal(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        NOTHING_TO_RETRY,
        KeptImport::Gone,
    )
}

/// The kept files were deleted. The retry took the job, so it is gone too.
pub(super) fn files_gone() -> ImportRefusal {
    refusal(
        FailureCode::ImportFilesGone,
        Stage::Import,
        RetryAdvice::NotRetryable,
        FILES_GONE,
        KeptImport::Gone,
    )
}

/// A listed match was picked, but no automatic run of the kept job left
/// matches to pick from. The job stays kept for as-is and MusicBrainz.
pub(super) fn no_choices() -> ImportRefusal {
    refusal(
        FailureCode::ImportChoiceInvalid,
        Stage::Input,
        RetryAdvice::NeedsDecision,
        NO_CHOICES,
        KeptImport::Kept,
    )
}

/// The job and its matches stay kept; only this match cannot be imported.
pub(super) fn unknown_candidate() -> ImportRefusal {
    refusal(
        FailureCode::ImportChoiceInvalid,
        Stage::Input,
        RetryAdvice::NeedsDecision,
        UNKNOWN_CANDIDATE,
        KeptImport::Kept,
    )
}

/// The typed text is not a MusicBrainz link or ID. The job stays kept.
pub(super) fn not_an_id() -> ImportRefusal {
    refusal(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NeedsDecision,
        NOT_AN_ID,
        KeptImport::Kept,
    )
}

/// A link to an entity the import mode cannot use. The job stays kept.
fn wrong_entity(message: String) -> ImportRefusal {
    refusal(
        FailureCode::ImportChoiceInvalid,
        Stage::Input,
        RetryAdvice::NeedsDecision,
        message,
        KeptImport::Kept,
    )
}

/// The importer's choice for `choice` on `job`. As-is and a MusicBrainz link
/// work on any kept job; a listed match needs the matches of a skipped
/// automatic run and must be one of them.
pub(super) fn resolve(
    choice: &ImportChoice,
    job: &ImportJob,
) -> Result<DriverChoice, ImportRefusal> {
    let choices = || job.choices.as_deref().ok_or_else(no_choices);
    match choice {
        ImportChoice::Auto => Ok(DriverChoice::Auto),
        ImportChoice::AsIs => Ok(DriverChoice::AsIs),
        ImportChoice::Candidate { data_source, id } => {
            let found = choices()?
                .candidates
                .iter()
                .any(|c| c.data_source == *data_source && c.id == *id);
            match found {
                true => Ok(DriverChoice::Pick {
                    data_source: data_source.clone(),
                    id: id.clone(),
                }),
                false => Err(unknown_candidate()),
            }
        }
        ImportChoice::MusicBrainz(text) => Ok(DriverChoice::Pick {
            data_source: MUSICBRAINZ.to_string(),
            id: musicbrainz_id(text, job.mode)?,
        }),
    }
}

/// The MBID in a bare ID or a musicbrainz.org link, refusing a link to an
/// entity that `mode` cannot import.
fn musicbrainz_id(text: &str, mode: ImportMode) -> Result<String, ImportRefusal> {
    let text = text.trim().to_ascii_lowercase();
    if !text.contains('/') {
        return mbid(&text).ok_or_else(not_an_id);
    }
    let link = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .unwrap_or(&text);
    let path = link.split(['?', '#']).next().unwrap_or_default();
    let mut parts = path.split('/');
    let host = parts.next().unwrap_or_default();
    let host = match host.split_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        Some(_) => return Err(not_an_id()),
        None => host,
    };
    let on_musicbrainz = host == "musicbrainz.org" || host.ends_with(".musicbrainz.org");
    let (Some(entity), Some(id)) = (parts.next(), parts.next()) else {
        return Err(not_an_id());
    };
    let is_entity = MUSICBRAINZ_ENTITIES.contains(&entity);
    let id = match (on_musicbrainz && is_entity, mbid(id)) {
        (true, Some(id)) => id,
        _ => return Err(not_an_id()),
    };
    let (kind, needed) = match mode {
        ImportMode::Album => ("Album", "release"),
        ImportMode::Singleton => ("Track", "recording"),
    };
    if entity != needed {
        // The entity names that start with a vowel sound: area, artist,
        // event, instrument.
        let article = match entity.starts_with(['a', 'e', 'i', 'o']) {
            true => "an",
            false => "a",
        };
        let entity = entity.replace('-', " ");
        return Err(wrong_entity(format!(
            "{kind} imports need a MusicBrainz {needed}, not {article} {entity}"
        )));
    }
    Ok(id)
}

fn mbid(text: &str) -> Option<String> {
    uuid::Uuid::try_parse(text)
        .ok()
        .map(|id| id.hyphenated().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_queue::test_support::candidate;
    use crate::import_queue::tests::job;
    use shared::download::{MatchCandidate, MatchChoices, MatchKind};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const MBID: &str = "6a09041b-cc7f-45d3-b302-39752c525af6";

    fn with_choices(mode: ImportMode, candidates: Vec<MatchCandidate>) -> ImportJob {
        let kind = match mode {
            ImportMode::Album => MatchKind::Album,
            ImportMode::Singleton => MatchKind::Track,
        };
        ImportJob {
            mode,
            choices: Some(Arc::new(MatchChoices { kind, candidates })),
            ..job(PathBuf::from("/dl/a.flac"), Path::new("/music"))
        }
    }

    fn pick(data_source: &str, id: &str) -> DriverChoice {
        DriverChoice::Pick {
            data_source: data_source.into(),
            id: id.into(),
        }
    }

    #[test]
    fn auto_needs_no_choices() {
        let plain = job(PathBuf::from("/dl/a.flac"), Path::new("/music"));
        assert_eq!(resolve(&ImportChoice::Auto, &plain), Ok(DriverChoice::Auto));
    }

    fn job_without_choices(mode: ImportMode) -> ImportJob {
        ImportJob {
            mode,
            ..job(PathBuf::from("/dl/a.flac"), Path::new("/music"))
        }
    }

    #[test]
    fn as_is_and_musicbrainz_need_no_kept_candidates() {
        let job = job_without_choices(ImportMode::Singleton);
        assert_eq!(resolve(&ImportChoice::AsIs, &job), Ok(DriverChoice::AsIs));
        assert_eq!(
            resolve(
                &ImportChoice::MusicBrainz(format!("https://musicbrainz.org/recording/{MBID}")),
                &job
            ),
            Ok(pick(MUSICBRAINZ, MBID))
        );
        assert_eq!(
            resolve(
                &ImportChoice::Candidate {
                    data_source: "MusicBrainz".into(),
                    id: MBID.into()
                },
                &job
            ),
            Err(no_choices())
        );
        assert_eq!(no_choices().kept, KeptImport::Kept);
    }

    #[test]
    fn an_empty_candidate_list_allows_as_is_and_a_typed_id() {
        let empty = with_choices(ImportMode::Singleton, vec![]);
        assert_eq!(resolve(&ImportChoice::AsIs, &empty), Ok(DriverChoice::AsIs));
        assert_eq!(
            resolve(&ImportChoice::MusicBrainz(MBID.into()), &empty),
            Ok(pick(MUSICBRAINZ, MBID))
        );
    }

    #[test]
    fn a_listed_match_must_be_one_beets_found() {
        let kept = with_choices(
            ImportMode::Singleton,
            vec![candidate("FakeSrc", "a"), candidate("OtherSrc", "b")],
        );
        let choose = |data_source: &str, id: &str| {
            resolve(
                &ImportChoice::Candidate {
                    data_source: data_source.into(),
                    id: id.into(),
                },
                &kept,
            )
        };
        assert_eq!(choose("FakeSrc", "a"), Ok(pick("FakeSrc", "a")));
        assert_eq!(choose("OtherSrc", "b"), Ok(pick("OtherSrc", "b")));
        // Same id under another source, and an id nobody listed.
        assert_eq!(choose("OtherSrc", "a"), Err(unknown_candidate()));
        assert_eq!(choose("FakeSrc", "c"), Err(unknown_candidate()));
    }

    #[test]
    fn musicbrainz_ids_come_bare_or_from_a_link() {
        use ImportMode::{Album, Singleton};
        let upper = MBID.to_uppercase();
        let ok = [
            (MBID.to_string(), Singleton),
            (format!("  {upper}\n"), Album),
            (
                format!("https://musicbrainz.org/recording/{MBID}"),
                Singleton,
            ),
            (
                format!("http://beta.musicbrainz.org/recording/{MBID}/"),
                Singleton,
            ),
            (format!("musicbrainz.org/release/{MBID}?tab=1#disc1"), Album),
            (
                format!("https://musicbrainz.org/release/{MBID}/disc/1"),
                Album,
            ),
            (
                format!("https://musicbrainz.org:443/recording/{MBID}"),
                Singleton,
            ),
        ];
        for (text, mode) in ok {
            assert_eq!(musicbrainz_id(&text, mode), Ok(MBID.to_string()), "{text}");
        }
    }

    #[test]
    fn a_link_to_an_entity_the_mode_cannot_import_is_refused() {
        use ImportMode::{Album, Singleton};
        let refused = [
            (
                "release",
                Singleton,
                "Track imports need a MusicBrainz recording, not a release",
            ),
            (
                "release-group",
                Singleton,
                "Track imports need a MusicBrainz recording, not a release group",
            ),
            // A track ID is not a recording ID, so beets could not look it up.
            (
                "track",
                Singleton,
                "Track imports need a MusicBrainz recording, not a track",
            ),
            (
                "recording",
                Album,
                "Album imports need a MusicBrainz release, not a recording",
            ),
            (
                "track",
                Album,
                "Album imports need a MusicBrainz release, not a track",
            ),
            (
                "artist",
                Album,
                "Album imports need a MusicBrainz release, not an artist",
            ),
            (
                "url",
                Singleton,
                "Track imports need a MusicBrainz recording, not a url",
            ),
        ];
        for (entity, mode, text) in refused {
            let link = format!("https://musicbrainz.org/{entity}/{MBID}");
            assert_eq!(
                musicbrainz_id(&link, mode),
                Err(wrong_entity(text.into())),
                "{link}"
            );
        }
    }

    #[test]
    fn anything_else_is_not_a_musicbrainz_id() {
        for text in [
            String::new(),
            "not an id".into(),
            "6a09041b-cc7f-45d3".into(),
            format!("https://example.com/recording/{MBID}"),
            format!("https://notmusicbrainz.org/recording/{MBID}"),
            "https://musicbrainz.org/recording/".into(),
            "https://musicbrainz.org/recording/nope".into(),
            format!("https://musicbrainz.org/{MBID}"),
            format!("https://musicbrainz.org/Recording!/{MBID}"),
            // Not a MusicBrainz entity.
            format!("https://musicbrainz.org/foo/{MBID}"),
            format!("https://musicbrainz.org/doc/{MBID}"),
            format!("https://musicbrainz.org:/recording/{MBID}"),
            format!("https://musicbrainz.org:https/recording/{MBID}"),
            format!("https://example.com:443/recording/{MBID}"),
        ] {
            assert_eq!(
                musicbrainz_id(&text, ImportMode::Singleton),
                Err(not_an_id()),
                "{text}"
            );
        }
    }

    #[test]
    fn a_wrong_link_is_refused_before_the_pick() {
        let kept = with_choices(ImportMode::Album, vec![]);
        assert_eq!(
            resolve(
                &ImportChoice::MusicBrainz(format!("https://musicbrainz.org/recording/{MBID}")),
                &kept
            ),
            Err(wrong_entity(
                "Album imports need a MusicBrainz release, not a recording".into()
            ))
        );
    }

    #[test]
    fn each_refusal_says_what_stays_kept() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (refusal, code, stage, retry, message, kept) in [
            (
                nothing_to_retry(),
                UnknownHandle,
                Stage::Input,
                NotRetryable,
                "Nothing to retry",
                KeptImport::Gone,
            ),
            (
                files_gone(),
                ImportFilesGone,
                Stage::Import,
                NotRetryable,
                "The kept files are gone",
                KeptImport::Gone,
            ),
            (
                no_choices(),
                ImportChoiceInvalid,
                Stage::Input,
                NeedsDecision,
                "This import has no matches to pick from",
                KeptImport::Kept,
            ),
            (
                unknown_candidate(),
                ImportChoiceInvalid,
                Stage::Input,
                NeedsDecision,
                "That match is not one beets found for this import",
                KeptImport::Kept,
            ),
            (
                not_an_id(),
                InvalidInput,
                Stage::Input,
                NeedsDecision,
                "That is not a MusicBrainz link or ID",
                KeptImport::Kept,
            ),
        ] {
            let f = &refusal.failure;
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str(), refusal.kept),
                (code, stage, retry, message, kept)
            );
        }
    }
}
