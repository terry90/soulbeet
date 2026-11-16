//! MCP acquisition: what a request asks for, and the seam the tools and the job runner act
//! through.

use crate::core::library::Reference;
use crate::ledger::JobKind;
use shared::download::DownloadQuery;
use shared::metadata::{Album, Track};

pub mod runner;
#[cfg(test)]
pub(crate) mod test_support;

/// An album asked for by its MusicBrainz release (D1).
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumRef {
    pub artist: String,
    pub title: String,
    pub release_mbid: String,
}

/// What a request asks for (D1): a track as `find_in_library` describes one, or an album.
#[derive(Debug, Clone, PartialEq)]
pub enum Wanted {
    Track(Reference),
    Album(AlbumRef),
}

impl Wanted {
    pub fn kind(&self) -> JobKind {
        match self {
            Wanted::Track(_) => JobKind::Track,
            Wanted::Album(_) => JobKind::Album,
        }
    }

    /// The query a job and its search store: one track (its duration as "m:ss"), or an album
    /// whose id and mbid are the release MBID, its tracks empty until hydrated.
    pub fn query(&self) -> DownloadQuery {
        match self {
            Wanted::Track(r) => DownloadQuery::new(vec![Track {
                id: String::new(),
                title: r.title.clone(),
                artist: r.artist.clone(),
                album_id: None,
                album_title: r.album.clone(),
                release_date: None,
                duration: r.duration_s.map(|s| format!("{}:{:02}", s / 60, s % 60)),
                mbid: r.recording_mbid.clone(),
                release_mbid: None,
            }]),
            Wanted::Album(a) => DownloadQuery::new(vec![]).album(Album {
                id: a.release_mbid.clone(),
                title: a.title.clone(),
                artist: a.artist.clone(),
                release_date: None,
                mbid: Some(a.release_mbid.clone()),
                cover_url: None,
                release_types: None,
            }),
        }
    }

    /// What it is called in labels: "{artist} - {title}".
    pub fn label(&self) -> String {
        let (artist, title) = match self {
            Wanted::Track(r) => (&r.artist, &r.title),
            Wanted::Album(a) => (&a.artist, &a.title),
        };
        format!("{artist} - {title}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_becomes_the_query_a_search_scores_against() {
        let track = Wanted::Track(Reference {
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: Some("Winter Songs".into()),
            recording_mbid: Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into()),
            duration_s: Some(263),
        });
        let query = track.query();
        assert_eq!(query.album, None);
        let t = &query.tracks[0];
        assert_eq!(
            (
                t.duration.as_deref(),
                t.duration_secs(),
                t.album_title.as_deref(),
                t.mbid.as_deref()
            ),
            (
                Some("4:23"),
                Some(263),
                Some("Winter Songs"),
                Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8")
            )
        );
        assert_eq!(
            (track.kind(), track.label()),
            (JobKind::Track, "Marrow Choir - O Holy Night".to_string())
        );

        let album = Wanted::Album(AlbumRef {
            artist: "Kowloon".into(),
            title: "Come Over".into(),
            release_mbid: "6a09041b-0f79-3278-88d9-0552dd28d37f".into(),
        });
        let query = album.query();
        let a = query.album.as_ref().unwrap();
        assert_eq!(
            (a.id.as_str(), a.mbid.as_deref(), query.tracks.len()),
            (
                "6a09041b-0f79-3278-88d9-0552dd28d37f",
                Some("6a09041b-0f79-3278-88d9-0552dd28d37f"),
                0
            )
        );
        assert_eq!(
            (album.kind(), album.label()),
            (JobKind::Album, "Kowloon - Come Over".to_string())
        );
    }
}
