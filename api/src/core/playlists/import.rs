//! URL playlist import orchestration: preview against Navidrome library and commit into
//! `core::jobs::ensure_with_origin` + `core::playlists::fill`.

use std::sync::Arc;

use futures::StreamExt;
use shared::backends::{delivery, Delivery};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::playlist_import::{
    CommitPlaylistImportRequest, CommitPlaylistImportResponse, ExternalPlaylist,
    ImportJobCandidate, ImportJobSummary, ImportMatchStatus, ImportPreviewTrack,
    PlaylistImportPreview, PlaylistImportTarget,
};
use soulbeet::playlist_import::{resolve_playlist_url, PlaylistImportError, MAX_PLAYLIST_TRACKS};
use tracing::warn;

use super::fill::{self, PlaylistTarget};
use super::NewPlaylist;
use crate::auth::UserCtx;
use crate::core::acquire::AUTO_SELECT_SCORE_THRESHOLD;
use crate::core::database_failure;
use crate::core::folders::for_user_in;
use crate::core::index::NavidromeOf;
use crate::core::jobs::retry::{cancel, download_source, RetryAnswer, SourceTarget};
use crate::core::jobs::runner::Acquire;
use crate::core::jobs::{
    ensure_with_origin, job_failure, job_not_found, job_view, recent_failures, untried_given,
    views_of, EnsureItem, EnsureRefusal, Wanted,
};
use crate::core::library::{find, navidrome_for, MatchKind, Reference};
use crate::core::sources::{candidate_id, parse_candidate_id};
use crate::ledger::store;
use crate::ledger::{BatchOrigin, JobOptions, JobState, Pick, SourceFilters};
use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;
use crate::services::NavidromeLookupError;

const PREVIEW_LOOKUP_CONCURRENCY: usize = 8;
const MAX_PLAYLIST_NAME_CHARS: usize = 255;
const MAX_PLAYLIST_COMMENT_CHARS: usize = 2000;
const RECENT_IMPORT_JOBS_WINDOW_SECS: i64 = 24 * 60 * 60;
const MAX_IMPORT_JOBS_LISTED: usize = 50;
const MAX_IMPORT_JOB_CANDIDATES: usize = 5;
const LOSSLESS_EXTS: [&str; 6] = ["flac", "wav", "aiff", "aif", "ape", "wv"];

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

    // Resolve the user's single personal library folder up front before touching Navidrome or the ledger.
    let folder = for_user_in(&acquire.db, &ctx.user_id).await?;

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
    // Pre-flight target validation without holding the playlist mutex across library lookups.
    {
        let _preflight_locks = fill::prepare(&navi, ctx, &target)
            .await
            .map_err(|r| r.failure())?;
    }

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

    // Re-acquire the playlist mutex only for the actual playlist creation/update.
    // If re-validation or playlist creation fails after jobs were created, cancel those newly
    // created jobs so they are not silently orphaned.
    let _locks = match fill::prepare(&navi, ctx, &target).await {
        Ok(locks) => locks,
        Err(refusal) => {
            cancel_created_jobs(acquire, ctx, &ensured.items).await;
            return Err(refusal.failure());
        }
    };

    let written = match fill::begin(
        &acquire.ledger,
        &navi,
        ctx,
        &mut ensured.batch_id,
        &target,
        &ensured.items,
    )
    .await
    {
        Ok(written) => written,
        Err(refusal) => {
            cancel_created_jobs(acquire, ctx, &ensured.items).await;
            return Err(refusal.failure());
        }
    };

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

async fn cancel_created_jobs(acquire: &Acquire, ctx: &UserCtx, items: &[EnsureItem]) {
    for item in items {
        if let EnsureItem::Created { job_id, .. } = item {
            if let Err(err) = cancel(acquire, ctx, job_id).await {
                warn!(
                    "Could not cancel orphaned playlist import job {job_id} after playlist failure: {}",
                    err.message
                );
            }
        }
    }
}

fn candidate_quality(item: Option<&shared::download::DownloadableItem>) -> (String, bool) {
    let Some(item) = item else {
        return ("Unknown".to_string(), false);
    };
    let lossy_backend = delivery(&item.backend) == Delivery::LossyOnly;
    let ext = item
        .facts
        .extension
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());
    let is_lossy = lossy_backend
        || ext.is_some_and(|e| !LOSSLESS_EXTS.contains(&e.to_ascii_lowercase().as_str()));
    let quality = match (ext, item.facts.bitrate_kbps) {
        (Some(e), Some(kbps)) if !LOSSLESS_EXTS.contains(&e.to_ascii_lowercase().as_str()) => {
            format!("{} {}k", e.to_ascii_uppercase(), kbps)
        }
        (Some(e), _) => e.to_ascii_uppercase(),
        (None, Some(kbps)) => format!("{kbps}k"),
        (None, None) => shared::backends::meta(&item.backend)
            .map(|m| m.name.to_string())
            .unwrap_or_else(|| item.backend.clone()),
    };
    (quality, is_lossy)
}

/// Lists the caller's active and recent (last 24h) `BatchOrigin::Import` jobs, capped at 50,
/// including selectable candidates for any job waiting in `needs_source_choice`.
pub(crate) async fn list_import_jobs(
    acquire: &Acquire,
    ctx: &UserCtx,
) -> Result<Vec<ImportJobSummary>, Failure> {
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - RECENT_IMPORT_JOBS_WINDOW_SECS;

    let mut records: Vec<_> = store::jobs_of(&acquire.db, &ctx.id(), None, None, &[])
        .await
        .map_err(|e| database_failure(Stage::Input, e))?
        .into_iter()
        .filter(|job| {
            job.origin == BatchOrigin::Import
                && (!job.state.is_terminal() || job.updated_at >= cutoff)
        })
        .collect();

    // Active jobs first (needs_source_choice at the top), then recent terminal jobs newest-first.
    records.sort_by(|a, b| {
        let rank = |s: JobState| -> u8 {
            if s == JobState::NeedsSourceChoice {
                0
            } else if !s.is_terminal() {
                1
            } else {
                2
            }
        };
        rank(a.state)
            .cmp(&rank(b.state))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    records.truncate(MAX_IMPORT_JOBS_LISTED);

    let views = views_of(&acquire.db, &acquire.ledger, ctx, records).await?;
    let needs_choice = views
        .iter()
        .any(|v| v.job.state == JobState::NeedsSourceChoice);
    let failures = if needs_choice {
        recent_failures(&acquire.db, now).await?
    } else {
        std::collections::HashMap::new()
    };

    let mut summaries = Vec::with_capacity(views.len());
    for view in views {
        let (artist, title) = if let Some(track) = view.job.wanted.tracks.first() {
            (track.artist.clone(), track.title.clone())
        } else if let Some(album) = &view.job.wanted.album {
            (album.artist.clone(), album.title.clone())
        } else {
            (String::new(), "Unknown Track".to_string())
        };

        let candidates = if view.job.state == JobState::NeedsSourceChoice {
            let untried = untried_given(&view, &failures, now);
            untried
                .candidates
                .into_iter()
                .filter_map(|c| {
                    let (_, candidate_index) = parse_candidate_id(&c.id)?;
                    let first_item = c.group.items.first();
                    let (quality, is_lossy) = candidate_quality(first_item);
                    let source = if !c.group.source.trim().is_empty() {
                        c.group.source.clone()
                    } else {
                        first_item
                            .map(|i| i.backend.clone())
                            .unwrap_or_else(|| "source".to_string())
                    };
                    let label = if !c.group.title.trim().is_empty() {
                        c.group.title.clone()
                    } else {
                        first_item
                            .map(|i| i.title.clone())
                            .filter(|t| !t.trim().is_empty())
                            .unwrap_or_else(|| c.group.group_id.clone())
                    };
                    Some(ImportJobCandidate {
                        candidate_index,
                        source,
                        label,
                        quality,
                        is_lossy,
                    })
                })
                .take(MAX_IMPORT_JOB_CANDIDATES)
                .collect()
        } else {
            Vec::new()
        };

        summaries.push(ImportJobSummary {
            job_id: view.job.id.clone(),
            batch_id: view.job.batch_id.clone(),
            artist,
            title,
            state: view.job.state.as_str().to_string(),
            error: job_failure(&view).map(|f| f.message),
            candidates,
        });
    }

    Ok(summaries)
}

/// Resolves a `BatchOrigin::Import` job waiting for a source choice: downloads the chosen
/// candidate (`Some(candidate_index)`) or cancels/skips the job (`None`), enforcing job ownership.
pub(crate) async fn resolve_import_job_choice(
    acquire: &Arc<Acquire>,
    ctx: &UserCtx,
    job_id: &str,
    candidate_index: Option<usize>,
) -> Result<(), Failure> {
    let view = job_view(acquire, ctx, job_id)
        .await?
        .ok_or_else(job_not_found)?;
    if view.job.origin != BatchOrigin::Import {
        return Err(job_not_found());
    }

    let answer = match candidate_index {
        Some(idx) => {
            let search_id = view.job.search_id.as_deref().ok_or_else(|| {
                Failure::new(
                    FailureCode::UnknownHandle,
                    Stage::Input,
                    RetryAdvice::NotRetryable,
                    "No search results available for this import job",
                )
            })?;
            let cid = candidate_id(search_id, idx);
            download_source(acquire, ctx, SourceTarget::Job(job_id.to_string()), &cid).await?
        }
        None => cancel(acquire, ctx, job_id).await?,
    };

    match answer {
        RetryAnswer::Applied(_) | RetryAnswer::AppliedUnread { .. } => Ok(()),
        RetryAnswer::Refused(f, _) | RetryAnswer::Unavailable(f, _) => Err(f),
        RetryAnswer::InvalidInput { field, expected } => Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            format!("Invalid {field}: {expected}"),
        )),
    }
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

        navi.answer(
            "/rest/getPlaylists",
            &[&playlists_body(&[]), &playlists_body(&[])],
        );
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

    #[tokio::test]
    async fn commit_cancels_created_jobs_when_playlist_creation_fails() {
        let (acq, _effects) = Acquire::for_alice().await;
        // Hold the search turn so the spawned runner stays in Searching while playlist creation fails.
        let _hold = acq.searches.hold_turn_for_test().await;
        let ctx = test_ctx("alice");
        let navi = NavidromeStub::start();

        navi.answer(
            "/rest/getPlaylists",
            &[&playlists_body(&[]), &playlists_body(&[])],
        );
        navi.answer_when(
            "/api/song",
            "title=Carbon+Based+Lifeforms+Fauna",
            200,
            "[]",
        );
        navi.answer_with_status("/rest/createPlaylist", &[(500, "internal error")]);

        let req = CommitPlaylistImportRequest {
            target: PlaylistImportTarget::Create {
                name: "Will Fail".into(),
                comment: None,
                allow_duplicate_name: false,
            },
            tracks: vec![CommitPlaylistImportTrack {
                artist: "Carbon Based Lifeforms".into(),
                title: "Fauna".into(),
                album: None,
                duration_s: Some(312),
                recording_mbid: None,
            }],
        };

        let err = commit_playlist_import(&acq, &NavidromeFor(Ok(navi.client())), &ctx, req)
            .await
            .unwrap_err();
        assert_eq!(err.stage, Stage::Playlist);

        // Every created job must have been cancelled rather than left searching/orphaned.
        let jobs = store::jobs_of(&acq.db, &ctx.id(), None, None, &[])
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn list_and_resolve_import_jobs_supports_choice_cancel_and_ownership() {
        use crate::core::jobs::test_support::{choosing_track_job, view_of};

        let (acq, _effects) = Acquire::for_alice().await;
        let alice_ctx = test_ctx("alice");
        let bob_ctx = test_ctx("bob");

        // Create a job in NeedsSourceChoice and flip its batch origin to 'import'.
        let job_id = choosing_track_job(&acq).await;
        let view = view_of(&acq, &job_id).await;
        sqlx::query("UPDATE batches SET origin = 'import' WHERE id = ?")
            .bind(&view.job.batch_id)
            .execute(&acq.db)
            .await
            .unwrap();

        let listed = list_import_jobs(&acq, &alice_ctx).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].job_id, job_id);
        assert_eq!(listed[0].state, "needs_source_choice");
        assert!(!listed[0].candidates.is_empty());

        // Another user cannot see or resolve Alice's import job.
        assert!(list_import_jobs(&acq, &bob_ctx).await.unwrap().is_empty());
        let err = resolve_import_job_choice(&acq, &bob_ctx, &job_id, None)
            .await
            .unwrap_err();
        assert_eq!(err.code, FailureCode::UnknownHandle);

        // Alice can skip/cancel the NeedsSourceChoice import job.
        resolve_import_job_choice(&acq, &alice_ctx, &job_id, None)
            .await
            .unwrap();
        let after = view_of(&acq, &job_id).await;
        assert_eq!(after.job.state, JobState::Cancelled);

        // Second NeedsSourceChoice job: Alice approves candidate 0 to queue download.
        let job_id_2 = choosing_track_job(&acq).await;
        let view_2 = view_of(&acq, &job_id_2).await;
        sqlx::query("UPDATE batches SET origin = 'import' WHERE id = ?")
            .bind(&view_2.job.batch_id)
            .execute(&acq.db)
            .await
            .unwrap();
        let listed_2 = list_import_jobs(&acq, &alice_ctx).await.unwrap();
        let summary_2 = listed_2.iter().find(|j| j.job_id == job_id_2).unwrap();
        let cand_idx = summary_2.candidates[0].candidate_index;
        resolve_import_job_choice(&acq, &alice_ctx, &job_id_2, Some(cand_idx))
            .await
            .unwrap();
        let after_2 = view_of(&acq, &job_id_2).await;
        assert_ne!(after_2.job.state, JobState::NeedsSourceChoice);
    }
}
