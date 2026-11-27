//! URL playlist import orchestration: preview against Navidrome library and commit into
//! `core::jobs::ensure_with_origin` + `core::playlists::fill`.

use std::sync::Arc;

use futures::StreamExt;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::playlist_import::{
    CommitPlaylistImportRequest, CommitPlaylistImportResponse, ExternalPlaylist, ImportMatchStatus,
    ImportPreviewTrack, PlaylistImportPreview, PlaylistImportTarget,
};
use soulbeet::playlist_import::{resolve_playlist_url, PlaylistImportError, MAX_PLAYLIST_TRACKS};
use tracing::warn;

use super::fill::{self, PlaylistTarget};
use super::NewPlaylist;
use crate::auth::UserCtx;
use crate::core::acquire::AUTO_SELECT_SCORE_THRESHOLD;
use crate::core::folders::owned_in;
use crate::core::index::NavidromeOf;
use crate::core::jobs::runner::Acquire;
use crate::core::jobs::{ensure_with_origin, EnsureItem, EnsureRefusal, Wanted};
use crate::core::library::{find, navidrome_for, MatchKind, Reference};
use crate::ledger::{BatchOrigin, JobOptions, Pick, SourceFilters};
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
use crate::services::NavidromeLookupError;

const PREVIEW_LOOKUP_CONCURRENCY: usize = 8;
const MAX_PLAYLIST_NAME_CHARS: usize = 255;
const MAX_PLAYLIST_COMMENT_CHARS: usize = 2000;

fn map_import_error(err: PlaylistImportError) -> Failure {
    match err {
        PlaylistImportError::InvalidUrl(msg) => {
            Failure::new(FailureCode::InvalidInput, Stage::Input, RetryAdvice::NotRetryable, msg)
        }
        PlaylistImportError::NotFound(msg) => Failure::new(
            FailureCode::UnknownHandle,
            Stage::Input,
            RetryAdvice::NotRetryable,
            msg,
        ),
        PlaylistImportError::EmptyPlaylist => Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "This playlist contains no importable tracks.",
        ),
        PlaylistImportError::Upstream(msg) => Failure::new(
            FailureCode::MetadataUnavailable,
            Stage::Input,
            RetryAdvice::SameSourceLater,
            msg,
        ),
    }
}

/// Resolves an external playlist URL and annotates every track against the caller's Navidrome
/// library using bounded concurrency.
pub(crate) async fn preview_playlist_import(
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    url: &str,
) -> Result<PlaylistImportPreview, Failure> {
    let external = resolve_playlist_url(url).await.map_err(map_import_error)?;
    preview_external_playlist(navidrome, ctx, external).await
}

/// Annotates an already-resolved `ExternalPlaylist` against the caller's Navidrome library.
pub(crate) async fn preview_external_playlist(
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    external: ExternalPlaylist,
) -> Result<PlaylistImportPreview, Failure> {
    let navi = match navidrome.navidrome_of(&ctx.id()).await {
        Ok(navi) => Some(navi),
        Err(NavidromeLookupError::NotLinked | NavidromeLookupError::NoServer) => None,
        Err(e) => return Err(e.failure(Stage::LibraryCheck, e.to_string())),
    };

    let library_checked = navi.is_some();
    let total = external.tracks.len();

    let tracks = if let Some(navi) = navi {
        let mut results: Vec<(usize, ImportPreviewTrack)> =
            futures::stream::iter(external.tracks.into_iter().enumerate().map(|(index, t)| {
                let navi = navi.clone();
                async move {
                    let reference = Reference {
                        artist: t.artist.clone(),
                        title: t.title.clone(),
                        album: t.album.clone(),
                        recording_mbid: t.recording_mbid.clone(),
                        duration_s: t.duration_s,
                    };
                    let (match_status, matched_song_id, matched_label) =
                        match find(&navi, &reference).await {
                            Ok(found) => match found.matches.into_iter().next() {
                                Some(m) => {
                                    let status = match m.how.kind {
                                        MatchKind::Mbid | MatchKind::Exact => {
                                            ImportMatchStatus::InLibrary
                                        }
                                        MatchKind::Fuzzy => ImportMatchStatus::Fuzzy,
                                    };
                                    let label = match m.song.artist {
                                        Some(ref a) if !a.is_empty() => {
                                            format!("{a} - {}", m.song.title)
                                        }
                                        _ => m.song.title.clone(),
                                    };
                                    (status, Some(m.song.song_id), Some(label))
                                }
                                None => (ImportMatchStatus::Missing, None, None),
                            },
                            Err(failure) => {
                                warn!(
                                    "Playlist preview library lookup failed for {} - {}: {}",
                                    t.artist, t.title, failure.message
                                );
                                (ImportMatchStatus::Missing, None, None)
                            }
                        };
                    (
                        index,
                        ImportPreviewTrack {
                            index,
                            artist: t.artist,
                            title: t.title,
                            album: t.album,
                            duration_s: t.duration_s,
                            recording_mbid: t.recording_mbid,
                            match_status,
                            matched_song_id,
                            matched_label,
                        },
                    )
                }
            }))
            .buffer_unordered(PREVIEW_LOOKUP_CONCURRENCY)
            .collect()
            .await;
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, track)| track).collect()
    } else {
        let mut out = Vec::with_capacity(total);
        for (index, t) in external.tracks.into_iter().enumerate() {
            out.push(ImportPreviewTrack {
                index,
                artist: t.artist,
                title: t.title,
                album: t.album,
                duration_s: t.duration_s,
                recording_mbid: t.recording_mbid,
                match_status: ImportMatchStatus::Missing,
                matched_song_id: None,
                matched_label: None,
            });
        }
        out
    };

    Ok(PlaylistImportPreview {
        platform: external.platform,
        source_url: external.source_url,
        title: external.title,
        description: external.description,
        truncated_note: external.truncated_note,
        library_checked,
        tracks,
    })
}

fn invalid_input(msg: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        msg,
    )
}

/// Validates input, checks playlist target and folder ownership, starts download jobs for
/// missing tracks (with `BatchOrigin::Import` and `DuplicateRule::Skip`), and creates/fills
/// the Navidrome playlist in track order.
pub(crate) async fn commit_playlist_import(
    acquire: &Arc<Acquire>,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    req: CommitPlaylistImportRequest,
) -> Result<CommitPlaylistImportResponse, Failure> {
    if req.tracks.is_empty() {
        return Err(invalid_input("Select at least one track to import."));
    }
    if req.tracks.len() > MAX_PLAYLIST_TRACKS {
        return Err(invalid_input(format!(
            "Playlists are capped at {MAX_PLAYLIST_TRACKS} tracks per import."
        )));
    }
    if req.folder_id.trim().is_empty() {
        return Err(invalid_input("Choose a target library folder."));
    }

    // Verify folder ownership up front before touching Navidrome or the ledger.
    let folder = owned_in(&acquire.db, ctx, &req.folder_id).await?;

    let (target, batch_label) = match req.target {
        PlaylistImportTarget::Create {
            name,
            comment,
            allow_duplicate_name,
        } => {
            let trimmed = name.trim().to_string();
            if trimmed.is_empty() {
                return Err(invalid_input("Playlist name cannot be empty."));
            }
            if trimmed.chars().count() > MAX_PLAYLIST_NAME_CHARS {
                return Err(invalid_input(format!(
                    "Playlist name must be {MAX_PLAYLIST_NAME_CHARS} characters or fewer."
                )));
            }
            let comment = comment
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
            if let Some(ref c) = comment {
                if c.chars().count() > MAX_PLAYLIST_COMMENT_CHARS {
                    return Err(invalid_input(format!(
                        "Playlist comment must be {MAX_PLAYLIST_COMMENT_CHARS} characters or fewer."
                    )));
                }
            }
            let label = format!("Import: {trimmed}");
            let target = PlaylistTarget::Create(NewPlaylist {
                name: trimmed,
                comment,
                public: None,
                song_ids: vec![],
                allow_duplicate_name,
            });
            (target, label)
        }
        PlaylistImportTarget::Existing { playlist_id } => {
            let trimmed = playlist_id.trim().to_string();
            if trimmed.is_empty() {
                return Err(invalid_input("Playlist ID cannot be empty."));
            }
            let label = "Playlist import".to_string();
            (PlaylistTarget::Existing(trimmed), label)
        }
    };

    let mut items = Vec::with_capacity(req.tracks.len());
    for track in req.tracks {
        let artist = track.artist.trim().to_string();
        let title = track.title.trim().to_string();
        if artist.is_empty() || title.is_empty() {
            return Err(invalid_input(
                "Every selected track must have an artist and a title.",
            ));
        }
        let album = track
            .album
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty());
        let recording_mbid = track
            .recording_mbid
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        items.push(Wanted::Track(Reference {
            artist,
            title,
            album,
            recording_mbid,
            duration_s: track.duration_s.filter(|&d| d > 0),
        }));
    }

    let navi = navidrome_for(navidrome, ctx, Stage::Playlist).await?;
    let _locks = fill::prepare(&navi, ctx, &target)
        .await
        .map_err(|r| r.failure())?;

    let options = JobOptions {
        pick: Pick::Auto,
        min_score: AUTO_SELECT_SCORE_THRESHOLD,
        filters: SourceFilters::default(),
        max_sources: MAX_SOURCES_PER_TRACK as u32,
        replace: None,
    };

    let mut ensured = ensure_with_origin(
        acquire,
        navidrome,
        ctx,
        Some(&folder.id),
        Some(batch_label),
        items,
        options,
        true,
        None,
        BatchOrigin::Import,
    )
    .await
    .map_err(|refusal| match refusal {
        EnsureRefusal::Call(c) => c.failure,
        EnsureRefusal::Token(t) => t.failure,
        EnsureRefusal::Taken { failure, .. } => failure,
    })?;

    let written = fill::begin(
        &acquire.ledger,
        &navi,
        ctx,
        &mut ensured.batch_id,
        &target,
        &ensured.items,
    )
    .await
    .map_err(|r| r.failure())?;

    let mut already_in_library = 0usize;
    let mut queued_for_download = 0usize;
    let mut already_running = 0usize;
    let mut refused = 0usize;
    for item in &ensured.items {
        match item {
            EnsureItem::InLibrary { .. } => already_in_library += 1,
            EnsureItem::Created { .. } => queued_for_download += 1,
            EnsureItem::AlreadyRunning { .. } => already_running += 1,
            EnsureItem::Refused(..) => refused += 1,
        }
    }

    Ok(CommitPlaylistImportResponse {
        batch_id: ensured.batch_id,
        playlist_id: written.playlist.playlist_id,
        playlist_name: written.playlist.name,
        already_in_library,
        queued_for_download,
        already_running,
        refused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_ctx;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{
        native_song, native_songs, playlist_body, playlist_rows, playlists_body, NavidromeFor,
        NavidromeStub, ADVENT, HOLY,
    };
    use shared::playlist_import::{CommitPlaylistImportTrack, ExternalTrack, PlaylistPlatform};

    #[tokio::test]
    async fn preview_classifies_exact_fuzzy_and_missing_tracks_in_order() {
        let navi = NavidromeStub::start();
        let holy = native_song(HOLY.0, "Marrow Choir", HOLY.1, 263.0, None);
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+O+Holy+Night",
            200,
            &format!("[{holy}]"),
        );
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+O+Holy+Night+Live",
            200,
            &format!("[{holy}]"),
        );
        navi.answer_when("/api/song", "title=Unknown+Artist+Missing+Song", 200, "[]");

        let external = ExternalPlaylist {
            platform: PlaylistPlatform::Spotify,
            source_url: "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M".into(),
            title: "Winter Favorites".into(),
            description: None,
            tracks: vec![
                ExternalTrack {
                    artist: "Marrow Choir".into(),
                    title: "O Holy Night".into(),
                    album: None,
                    duration_s: Some(263),
                    recording_mbid: None,
                },
                ExternalTrack {
                    artist: "Marrow Choir".into(),
                    title: "O Holy Night Live".into(),
                    album: None,
                    duration_s: None,
                    recording_mbid: None,
                },
                ExternalTrack {
                    artist: "Unknown Artist".into(),
                    title: "Missing Song".into(),
                    album: None,
                    duration_s: Some(200),
                    recording_mbid: None,
                },
            ],
            truncated_note: None,
        };

        let ctx = test_ctx("alice");
        let preview =
            preview_external_playlist(&NavidromeFor(Ok(navi.client())), &ctx, external)
                .await
                .unwrap();

        assert!(preview.library_checked);
        assert_eq!(preview.tracks.len(), 3);
        assert_eq!(preview.tracks[0].index, 0);
        assert_eq!(preview.tracks[0].match_status, ImportMatchStatus::InLibrary);
        assert_eq!(preview.tracks[0].matched_song_id.as_deref(), Some(HOLY.0));

        assert_eq!(preview.tracks[1].index, 1);
        assert_eq!(preview.tracks[1].match_status, ImportMatchStatus::Fuzzy);

        assert_eq!(preview.tracks[2].index, 2);
        assert_eq!(preview.tracks[2].match_status, ImportMatchStatus::Missing);
    }

    #[tokio::test]
    async fn preview_works_without_navidrome_link() {
        let external = ExternalPlaylist {
            platform: PlaylistPlatform::Deezer,
            source_url: "https://www.deezer.com/playlist/908622995".into(),
            title: "Test".into(),
            description: None,
            tracks: vec![ExternalTrack {
                artist: "Daft Punk".into(),
                title: "Around the World".into(),
                album: None,
                duration_s: Some(429),
                recording_mbid: None,
            }],
            truncated_note: None,
        };

        let ctx = test_ctx("alice");
        let preview = preview_external_playlist(
            &NavidromeFor(Err(|| NavidromeLookupError::NotLinked)),
            &ctx,
            external,
        )
        .await
        .unwrap();

        assert!(!preview.library_checked);
        assert_eq!(preview.tracks.len(), 1);
        assert_eq!(preview.tracks[0].match_status, ImportMatchStatus::Missing);
    }

    #[tokio::test]
    async fn commit_creates_import_batch_and_playlist_and_queues_missing_tracks() {
        let (acq, _effects) = Acquire::for_alice().await;
        let ctx = test_ctx("alice");
        let navi = NavidromeStub::start();
        let holy = native_song(HOLY.0, "Marrow Choir", HOLY.1, 263.0, None);

        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+O+Holy+Night",
            200,
            &format!("[{holy}]"),
        );
        navi.answer_when(
            "/api/song",
            "title=Carbon+Based+Lifeforms+Fauna",
            200,
            "[]",
        );
        navi.answer_when(
            "/api/song",
            &format!("id={}", HOLY.0),
            200,
            &native_songs(&[(HOLY.0, false)]),
        );
        navi.answer(
            "/rest/createPlaylist",
            &[&playlist_body(
                ADVENT,
                "Imported Mix",
                "alice",
                false,
                "",
                &[HOLY],
            )],
        );
        navi.answer(
            "/rest/updatePlaylist",
            &[r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#],
        );
        navi.answer(
            &format!("/api/playlist/{ADVENT}/tracks"),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(
                ADVENT,
                "Imported Mix",
                "alice",
                false,
                "Imported from Spotify",
                &[HOLY],
            )],
        );

        let req = CommitPlaylistImportRequest {
            target: PlaylistImportTarget::Create {
                name: "Imported Mix".into(),
                comment: Some("Imported from Spotify".into()),
                allow_duplicate_name: false,
            },
            folder_id: "alice-folder".into(),
            tracks: vec![
                CommitPlaylistImportTrack {
                    artist: "Marrow Choir".into(),
                    title: "O Holy Night".into(),
                    album: None,
                    duration_s: Some(263),
                    recording_mbid: None,
                },
                CommitPlaylistImportTrack {
                    artist: "Carbon Based Lifeforms".into(),
                    title: "Fauna".into(),
                    album: None,
                    duration_s: Some(312),
                    recording_mbid: None,
                },
            ],
        };

        let resp = commit_playlist_import(&acq, &NavidromeFor(Ok(navi.client())), &ctx, req)
            .await
            .unwrap();

        assert!(resp.batch_id.is_some());
        assert_eq!(resp.playlist_id, ADVENT);
        assert_eq!(resp.playlist_name, "Imported Mix");
        assert_eq!(resp.already_in_library, 1);
        assert_eq!(resp.queued_for_download, 1);
        assert_eq!(resp.already_running, 0);
        assert_eq!(resp.refused, 0);

        let batch_id = resp.batch_id.unwrap();
        let origin: String = sqlx::query_scalar("SELECT origin FROM batches WHERE id = ?")
            .bind(&batch_id)
            .fetch_one(&acq.db)
            .await
            .unwrap();
        assert_eq!(origin, "import");
        assert_eq!(
            crate::ledger::DuplicateRule::for_job(BatchOrigin::Import, None),
            crate::ledger::DuplicateRule::Skip
        );

        // CHECK constraint still rejects arbitrary invalid origins.
        let invalid = sqlx::query(
            "INSERT INTO batches (id, user_id, created_at, origin) VALUES ('bad-b', 'alice', 1, 'unknown')",
        )
        .execute(&acq.db)
        .await;
        assert!(invalid.is_err());
    }
}
