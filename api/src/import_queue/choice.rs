//! Turns what the user chose for a skipped import into what the importer
//! runs, refusing anything the kept job does not allow.

use super::ImportJob;
use shared::download::{ImportChoice, KeptImport};
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

/// A retry or matches request the server refused: the text for the user and
/// what stays kept for the row, which the client applies as is. Every such
/// refusal is built here, so each text says one thing about what is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub message: String,
    pub kept: KeptImport,
}

impl Refusal {
    fn new(message: impl Into<String>, kept: KeptImport) -> Self {
        Refusal {
            message: message.into(),
            kept,
        }
    }

    /// The row has no kept job: none was kept, it was lost on restart or
    /// dropped as the oldest, or a retry from another row of it took it.
    pub(super) fn nothing_to_retry() -> Self {
        Self::new(NOTHING_TO_RETRY, KeptImport::Gone)
    }

    /// The kept files were deleted. The retry took the job, so it is gone too.
    pub(super) fn files_gone() -> Self {
        Self::new(FILES_GONE, KeptImport::Gone)
    }

    /// The job is kept, but no automatic run of it left matches to pick from.
    pub(super) fn no_choices() -> Self {
        Self::new(NO_CHOICES, KeptImport::WithoutMatches)
    }

    /// The job and its matches stay kept; only this choice cannot be imported.
    fn bad_choice(message: impl Into<String>) -> Self {
        Self::new(message, KeptImport::WithMatches)
    }
}

/// The importer's choice for `choice` on `job`. Anything but Auto needs the
/// matches of a skipped automatic run, and a listed match must be one of them.
pub(super) fn resolve(choice: &ImportChoice, job: &ImportJob) -> Result<DriverChoice, Refusal> {
    let choices = || job.choices.as_deref().ok_or_else(Refusal::no_choices);
    match choice {
        ImportChoice::Auto => Ok(DriverChoice::Auto),
        ImportChoice::AsIs => choices().map(|_| DriverChoice::AsIs),
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
                false => Err(Refusal::bad_choice(UNKNOWN_CANDIDATE)),
            }
        }
        ImportChoice::MusicBrainz(text) => {
            choices()?;
            Ok(DriverChoice::Pick {
                data_source: MUSICBRAINZ.to_string(),
                id: musicbrainz_id(text, job.mode).map_err(Refusal::bad_choice)?,
            })
        }
    }
}

/// The MBID in a bare ID or a musicbrainz.org link, refusing a link to an
/// entity that `mode` cannot import.
fn musicbrainz_id(text: &str, mode: ImportMode) -> Result<String, String> {
    let text = text.trim().to_ascii_lowercase();
    if !text.contains('/') {
        return mbid(&text).ok_or_else(|| NOT_AN_ID.to_string());
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
        Some(_) => return Err(NOT_AN_ID.to_string()),
        None => host,
    };
    let on_musicbrainz = host == "musicbrainz.org" || host.ends_with(".musicbrainz.org");
    let (Some(entity), Some(id)) = (parts.next(), parts.next()) else {
        return Err(NOT_AN_ID.to_string());
    };
    let is_entity = MUSICBRAINZ_ENTITIES.contains(&entity);
    let id = match (on_musicbrainz && is_entity, mbid(id)) {
        (true, Some(id)) => id,
        _ => return Err(NOT_AN_ID.to_string()),
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
        return Err(format!(
            "{kind} imports need a MusicBrainz {needed}, not {article} {entity}"
        ));
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

    #[test]
    fn every_other_choice_needs_the_kept_matches() {
        let plain = job(PathBuf::from("/dl/a.flac"), Path::new("/music"));
        for choice in [
            ImportChoice::AsIs,
            ImportChoice::Candidate {
                data_source: "FakeSrc".into(),
                id: "x".into(),
            },
            ImportChoice::MusicBrainz(MBID.into()),
        ] {
            assert_eq!(
                resolve(&choice, &plain),
                Err(Refusal::no_choices()),
                "{choice:?}"
            );
        }
        // An empty list still allows as-is and a typed ID.
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
        let unknown = Refusal::bad_choice(UNKNOWN_CANDIDATE);
        assert_eq!(choose("OtherSrc", "a"), Err(unknown.clone()));
        assert_eq!(choose("FakeSrc", "c"), Err(unknown));
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
            assert_eq!(musicbrainz_id(&link, mode), Err(text.into()), "{link}");
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
                Err(NOT_AN_ID.into()),
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
            Err(Refusal::bad_choice(
                "Album imports need a MusicBrainz release, not a recording"
            ))
        );
    }

    #[test]
    fn each_refusal_says_what_stays_kept() {
        let refusals = [
            (
                Refusal::nothing_to_retry(),
                "Nothing to retry",
                KeptImport::Gone,
            ),
            (
                Refusal::files_gone(),
                "The kept files are gone",
                KeptImport::Gone,
            ),
            (
                Refusal::no_choices(),
                "This import has no matches to pick from",
                KeptImport::WithoutMatches,
            ),
            (
                Refusal::bad_choice(NOT_AN_ID),
                "That is not a MusicBrainz link or ID",
                KeptImport::WithMatches,
            ),
        ];
        for (refusal, message, kept) in refusals {
            assert_eq!(refusal, Refusal::new(message, kept));
        }
    }
}
