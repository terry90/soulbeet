//! One recommendation run for a user: the profile built from their Navidrome
//! library and, when saved, their ListenBrainz account, then each discovery
//! profile's candidates from the generators the user's settings turn on.

use std::sync::Arc;

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::navidrome::LastRun;
use shared::recommendation::{
    Candidate, DiscoveryProfile, EngineReport, EngineState, ProfileConfig, SourceError,
    SourceErrors, NO_LISTENING_DATA,
};
use soulbeet::deezer::DeezerClient;
use soulbeet::engine::catalog::CatalogGenerator;
use soulbeet::engine::deezer_radio::DeezerGenerator;
use soulbeet::engine::listenbrainz_account::ListenBrainzAccountGenerator;
use soulbeet::engine::{build_profile, recommend};
use soulbeet::history::listenbrainz::ListenBrainzHistory;
use soulbeet::history::HistoryError;
use soulbeet::knowledge::metabrainz::MetaBrainzKnowledge;
use soulbeet::knowledge::KnowledgeHosts;
use soulbeet::listenbrainz::client::ListenBrainzClient;
use soulbeet::listenbrainz::labs::LabsClient;
use soulbeet::musicbrainz::MusicBrainzProvider;
use soulbeet::polite::{Pace, RemoteError};
use soulbeet::traits::MusicBrainzLookup;
use soulbeet::{CandidateGenerator, KnowledgeCache, ListeningHistory};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::auth::UserId;
use crate::core::database_failure;
use crate::core::discovery::parse_profiles;
use crate::core::index::NavidromeOf;
use crate::db::DB;
use crate::models::discovery_candidate::DiscoveryCandidateRow;
use crate::models::engine_report::EngineReportRow;
use crate::models::knowledge_cache::SqliteKnowledgeCache;
use crate::models::user::User;
use crate::models::user_settings::UserSettings;

/// Candidates kept per discovery profile and run.
const CANDIDATES_PER_PROFILE: usize = 50;
/// Engine reports kept per user.
const REPORTS_KEPT: u32 = 30;

/// Which optional sources a user's settings turn on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourcePlan {
    pub listenbrainz_user: Option<String>,
    pub listenbrainz_token: Option<String>,
    pub deezer: bool,
}

/// ListenBrainz needs only a username, the token is optional; a blank value counts as unset.
pub(crate) fn plan_for(settings: &UserSettings) -> SourcePlan {
    let given = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    SourcePlan {
        listenbrainz_user: given(&settings.listenbrainz_username),
        listenbrainz_token: given(&settings.listenbrainz_token),
        deezer: settings.deezer_recommendations,
    }
}

/// What a run reaches beyond the user's own settings: hosts, the shared fact cache,
/// MusicBrainz and the paces of the remote services.
pub(crate) struct DiscoveryServices {
    pub hosts: KnowledgeHosts,
    pub cache: Arc<dyn KnowledgeCache>,
    pub musicbrainz: Arc<dyn MusicBrainzLookup>,
    pub metabrainz_pace: Pace,
    pub deezer_pace: Pace,
}

impl DiscoveryServices {
    /// The server's services: the real hosts, unless the environment overrides them.
    pub(crate) fn live() -> DiscoveryServices {
        DiscoveryServices {
            hosts: KnowledgeHosts::from_env(),
            cache: Arc::new(SqliteKnowledgeCache::new((*DB).clone())),
            musicbrainz: Arc::new(MusicBrainzProvider::new()),
            metabrainz_pace: Pace::METABRAINZ,
            deezer_pace: Pace::DEEZER,
        }
    }
}

/// The users the background cycle refreshes, each with the discovery profiles they selected:
/// connected to Navidrome, discovery on and a discovery folder chosen. No one else's library
/// is read or sent to MetaBrainz or Deezer.
pub(crate) async fn due_in(db: &SqlitePool) -> Result<Vec<(User, Vec<DiscoveryProfile>)>, String> {
    let mut due = Vec::new();
    for user in User::get_connected_users_in(db).await? {
        let settings = UserSettings::get_in(db, &user.id).await?;
        if settings.discovery_enabled && settings.discovery_folder().is_some() {
            due.push((user, parse_profiles(&settings.discovery_profiles)));
        }
    }
    Ok(due)
}

/// Builds the user's profile once, then stores each of `profiles`' candidates and report.
/// Returns how many candidates were stored in all. Fails when the Navidrome library cannot
/// be read, since without it the user's own music could be recommended back to them, when a
/// remote client cannot be set up, and when the profile has nothing to start from, each
/// after storing the reports that say so.
pub(crate) async fn generate_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    services: &DiscoveryServices,
    user: &UserId,
    profiles: &[DiscoveryProfile],
) -> Result<u32, Failure> {
    let user_id = user.to_string();
    let settings = UserSettings::get_in(db, &user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    let RunSources {
        histories,
        generators,
        knowledge,
    } = match sources_of(navidrome, services, user, plan_for(&settings)).await {
        Ok(sources) => sources,
        Err((source, failure)) => {
            store_failure_in(db, &user_id, profiles, source, &failure).await;
            return Err(failure);
        }
    };

    let build = build_profile(&histories, knowledge.as_ref()).await;
    let stored = |e: String| database_failure(Stage::Input, e);
    let mut total = 0u32;
    for profile in profiles {
        let config = ProfileConfig::from_profile(*profile);
        let (candidates, report) = recommend(
            &build,
            &generators,
            knowledge.as_ref(),
            &config,
            CANDIDATES_PER_PROFILE,
        )
        .await;
        let name = profile.name();
        store_run_in(db, &user_id, name, &candidates, &report)
            .await
            .map_err(stored)?;
        info!("{name}: {} candidates for {user_id}", candidates.len());
        total += candidates.len() as u32;
    }
    if !build.profile.has_seeds() {
        return Err(no_seeds(build.errors.into_vec()));
    }
    Ok(total)
}

/// What a run reads: the user's histories, the generators their settings turn on, and the
/// knowledge about music.
struct RunSources {
    histories: Vec<Arc<dyn ListeningHistory>>,
    generators: Vec<Arc<dyn CandidateGenerator>>,
    knowledge: Arc<MetaBrainzKnowledge>,
}

/// Reads the Navidrome library and sets up the remote clients `plan` turns on. Fails with
/// the name of the source that could not be read or set up.
async fn sources_of(
    navidrome: &dyn NavidromeOf,
    services: &DiscoveryServices,
    user: &UserId,
    plan: SourcePlan,
) -> Result<RunSources, (&'static str, Failure)> {
    let navi = navidrome.navidrome_of(user).await.map_err(|e| {
        let failure = e.failure(
            Stage::Input,
            format!("Discovery needs your Navidrome library: {e}"),
        );
        ("Navidrome", failure)
    })?;
    let library = soulbeet::history::navidrome::read(&navi)
        .await
        .map_err(library_failure)?;

    let listenbrainz = Arc::new(
        ListenBrainzClient::new(
            &services.hosts.listenbrainz,
            plan.listenbrainz_token,
            services.metabrainz_pace,
        )
        .map_err(client_failure)?,
    );
    let labs =
        LabsClient::new(&services.hosts.labs, services.metabrainz_pace).map_err(client_failure)?;
    let knowledge = Arc::new(MetaBrainzKnowledge::new(
        listenbrainz.clone(),
        labs,
        services.musicbrainz.clone(),
        services.cache.clone(),
    ));

    // ListenBrainz first: the first history with plays or stars is the profile's base.
    let mut histories: Vec<Arc<dyn ListeningHistory>> = Vec::new();
    let mut generators: Vec<Arc<dyn CandidateGenerator>> =
        vec![Arc::new(CatalogGenerator::new(knowledge.clone()))];
    if let Some(lb_user) = &plan.listenbrainz_user {
        histories.push(Arc::new(ListenBrainzHistory::new(
            listenbrainz.clone(),
            lb_user.clone(),
        )));
        generators.push(Arc::new(ListenBrainzAccountGenerator::new(
            listenbrainz.clone(),
            lb_user.clone(),
        )));
    }
    histories.push(Arc::new(library));
    if plan.deezer {
        let deezer = DeezerClient::new(
            &services.hosts.deezer,
            services.deezer_pace,
            services.cache.clone(),
        )
        .map_err(client_failure)?;
        generators.push(Arc::new(DeezerGenerator::new(Arc::new(deezer))));
    }
    Ok(RunSources {
        histories,
        generators,
        knowledge,
    })
}

/// Stores for each of `profiles` the report of a run that ended before recommending, because
/// `source` failed, so the Discovery page shows it. Their pools stay, as an outage's do. A
/// report that cannot be stored is logged: the run's own failure is what the caller needs.
async fn store_failure_in(
    db: &SqlitePool,
    user_id: &str,
    profiles: &[DiscoveryProfile],
    source: &str,
    failure: &Failure,
) {
    let mut errors = SourceErrors::default();
    errors.record(source, &failure.message);
    let report = EngineReport {
        source_errors: errors.into_vec(),
        ..Default::default()
    };
    for profile in profiles {
        if let Err(e) = store_run_in(db, user_id, profile.name(), &[], &report).await {
            warn!(
                "Could not store the failed run of {} for {user_id}: {e}",
                profile.name()
            );
        }
    }
}

/// The profile has no seeds. A history that `failed` may hold the user's listening, so the
/// run is retried later and the failure names it; with every history answered, there is no
/// listening data.
fn no_seeds(failed: Vec<SourceError>) -> Failure {
    if failed.is_empty() {
        return no_listening_data();
    }
    let failed: Vec<String> = (failed.iter())
        .map(|e| format!("{} failed: {}", e.source, e.message))
        .collect();
    Failure::new(
        FailureCode::MetadataUnavailable,
        Stage::Input,
        RetryAdvice::SameSourceLater,
        format!(
            "{}. Discovery has no listening data to start from until it answers. Try again later.",
            failed.join("; ")
        ),
    )
}

/// The profile has no seeds: nothing played or starred in Navidrome and no ListenBrainz history.
pub(crate) fn no_listening_data() -> Failure {
    Failure::new(
        FailureCode::NoListeningData,
        Stage::Input,
        RetryAdvice::NeedsUser,
        NO_LISTENING_DATA,
    )
    .with_user_action(UserAction::new(
        UserActionCode::ListenInNavidrome,
        "Play or star music in Navidrome",
        "Navidrome",
    ))
}

/// Stores a run's report, keeping the user's last `REPORTS_KEPT`, and its candidates. A run
/// that found candidates replaces the profile's pool. One that found none keeps the pool when
/// a source failed, so an outage does not empty it, and clears it when every source answered.
/// One transaction, so a concurrent run of the profile never leaves it empty or mixed.
pub(crate) async fn store_run_in(
    db: &SqlitePool,
    user_id: &str,
    profile: &str,
    candidates: &[Candidate],
    report: &EngineReport,
) -> Result<(), String> {
    let report_json = serde_json::to_string(report).map_err(|e| e.to_string())?;
    // The first statement writes, so the transaction waits for other writers instead of
    // failing as one that read first would.
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    EngineReportRow::insert_in(
        &mut tx,
        user_id,
        profile,
        &report_json,
        candidates.len() as u32,
    )
    .await?;
    EngineReportRow::prune_in(&mut tx, user_id, REPORTS_KEPT).await?;
    if !candidates.is_empty() || report.source_errors.is_empty() {
        DiscoveryCandidateRow::clear_for_user_profile_in(&mut tx, user_id, profile).await?;
    }
    DiscoveryCandidateRow::upsert_batch_in(&mut tx, user_id, profile, candidates).await?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// The latest run as the Discovery page shows it, from each profile's newest report: when the
/// newest was stored, every source they used and their distinct failures. No listening data
/// only when no profile had any. None when there is no report, or none parses.
pub(crate) fn last_run_of(rows: Vec<EngineReportRow>) -> Option<LastRun> {
    let mut runs = (rows.into_iter()).filter_map(|row| {
        let report: EngineReport = serde_json::from_str(&row.report_json).ok()?;
        Some((row.created_at, report))
    });
    let (created_at, first) = runs.next()?;
    let mut last = LastRun {
        created_at,
        state: first.state,
        sources: first.sources,
        source_errors: first.source_errors,
    };
    for (created_at, report) in runs {
        last.created_at = last.created_at.max(created_at);
        if report.state != EngineState::NoListeningData {
            last.state = report.state;
        }
        for source in report.sources {
            if !last.sources.contains(&source) {
                last.sources.push(source);
            }
        }
        for error in report.source_errors {
            let seen = (last.source_errors.iter())
                .any(|e| e.source == error.source && e.message == error.message);
            if !seen {
                last.source_errors.push(error);
            }
        }
    }
    Some(last)
}

/// The Navidrome library could not be read in full, with the source that failed.
fn library_failure(e: HistoryError) -> (&'static str, Failure) {
    let failure = match &e {
        HistoryError::Navidrome(n) => n.failure(
            Stage::Input,
            format!("Discovery needs your Navidrome library: {e}"),
        ),
        HistoryError::Incomplete { .. } | HistoryError::Remote(_) => Failure::new(
            FailureCode::NavidromeUnreachable,
            Stage::Input,
            RetryAdvice::SameSourceLater,
            format!("{e}. Try again later."),
        ),
    };
    (e.service(), failure)
}

/// A remote client that cannot be set up: its host is not a URL, or no HTTP client builds.
/// With the service it is for.
fn client_failure(e: RemoteError) -> (&'static str, Failure) {
    let failure = Failure::new(
        FailureCode::MetadataUnavailable,
        Stage::Input,
        RetryAdvice::NotRetryable,
        e.to_string(),
    );
    (e.service(), failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::models::user_settings::UpdateUserSettings;
    use crate::test_navidrome::{NavidromeFor, NavidromeStub, BAD_GATEWAY};
    use shared::system::NavidromeStatus;

    fn cand(artist: &str, track: &str) -> Candidate {
        Candidate {
            artist: artist.into(),
            track: track.into(),
            album: None,
            mbid: None,
            score: 1.0,
            signals: vec!["artist_hop1".into()],
            source: "catalog".into(),
            artist_listener_count: None,
            primary_genre: None,
            release_year: None,
            artist_mbid: None,
        }
    }

    /// Track names, sorted: candidates of equal score come back in no set order.
    fn tracks(candidates: Vec<Candidate>) -> Vec<String> {
        let mut names: Vec<String> = candidates.into_iter().map(|c| c.track).collect();
        names.sort();
        names
    }

    fn report_row(report_json: &str) -> EngineReportRow {
        EngineReportRow {
            id: "r".into(),
            user_id: "alice".into(),
            profile: "Balanced".into(),
            report_json: report_json.into(),
            candidate_count: 0,
            created_at: "2026-11-19 08:00:00".into(),
        }
    }

    #[test]
    fn no_listening_data_is_a_failure_the_user_can_act_on() {
        let f = no_listening_data();
        assert_eq!(
            (f.code.as_str(), f.message.as_str()),
            (
                "no_listening_data",
                "No listening data yet. Play or star music in Navidrome."
            )
        );
        assert_eq!(
            f.user_action.unwrap().code,
            UserActionCode::ListenInNavidrome
        );
    }

    #[tokio::test]
    async fn a_run_replaces_the_pool_unless_a_failed_source_left_it_with_nothing() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let pool = || async {
            tracks(
                DiscoveryCandidateRow::get_unused_in(&db, "alice", "Balanced", 10)
                    .await
                    .unwrap(),
            )
        };
        let answered = EngineReport::default();
        let failed = EngineReport {
            source_errors: vec![SourceError {
                source: "ListenBrainz Labs".into(),
                message: "down".into(),
                failures: 1,
            }],
            ..Default::default()
        };
        store_run_in(&db, "alice", "Balanced", &[cand("A", "a1")], &answered)
            .await
            .unwrap();
        store_run_in(&db, "alice", "Balanced", &[], &failed)
            .await
            .unwrap();
        assert_eq!(pool().await, ["a1"], "an outage does not empty the pool");
        store_run_in(&db, "alice", "Balanced", &[cand("B", "b1")], &failed)
            .await
            .unwrap();
        assert_eq!(pool().await, ["b1"], "a run with candidates replaces it");
        store_run_in(&db, "alice", "Balanced", &[], &answered)
            .await
            .unwrap();
        assert!(pool().await.is_empty(), "nothing found with no failure");
    }

    /// The literal of shared's `REPORT_0_9_0`: a report as 0.9.0 stored it.
    const REPORT_0_9_0: &str = r#"{"profile_source":"ListenBrainz+Last.fm","profile_summary":{"genre_count":2,"top_genres":["rock","pop"],"obscurity_score":0.4,"repeat_ratio":0.7,"freshness_half_life_days":90.0,"momentum_artists":["Khruangbin"],"comfort_tags":1,"exploration_tags":1},"pipeline_reports":[{"name":"lastfm_pipeline","signals":[{"name":"lfm_track_graph","candidates_produced":3,"top_candidates":[{"artist":"Muse","track":"Hysteria","score":0.9,"genre":"rock","listeners":3000000}]}],"total_candidates":3,"mbid_failures":0}],"blend_summary":{"sources":1,"total_after_blend":3,"cross_source_matches":0,"top_blended":[]},"freshness_summary":{"known_artists_penalized":1,"total_candidates":3,"penalty_factor":0.67,"new_release_boosted":0},"diversifier_summary":{"popularity_penalized":0,"artist_cap_skipped":0,"genre_quota_skipped":0,"exploration_backfilled":0,"effective_exploration_budget":0.2,"unique_artists":3,"unique_genres":1,"top_selected":[]},"final_count":3,"duration_secs":12.5}"#;

    #[test]
    fn the_last_run_reads_reports_of_both_shapes() {
        assert_eq!(
            last_run_of(vec![report_row(REPORT_0_9_0)]),
            Some(LastRun {
                created_at: "2026-11-19 08:00:00".into(),
                state: EngineState::Recommended,
                sources: vec![],
                source_errors: vec![],
            })
        );
        let empty = EngineReport {
            state: EngineState::NoListeningData,
            sources: vec!["Navidrome".into()],
            ..Default::default()
        };
        let row = report_row(&serde_json::to_string(&empty).unwrap());
        let run = last_run_of(vec![row]).unwrap();
        assert_eq!(
            (run.state, run.sources),
            (EngineState::NoListeningData, vec!["Navidrome".to_string()])
        );
        assert_eq!(last_run_of(vec![report_row("not json")]), None);
        assert_eq!(last_run_of(vec![]), None);
    }

    /// A report that used `sources` and saw `errors`, each (source, message), fail once.
    fn report_of(sources: &[&str], errors: &[(&str, &str)]) -> EngineReport {
        EngineReport {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            source_errors: (errors.iter())
                .map(|(source, message)| SourceError {
                    source: source.to_string(),
                    message: message.to_string(),
                    failures: 1,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn the_last_run_joins_the_newest_report_of_each_profile() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        // Stored within one second, as a cycle's profiles often are.
        for (profile, report) in [
            ("Balanced", report_of(&["Navidrome"], &[("Deezer", "old")])),
            ("Balanced", report_of(&["Navidrome", "catalog"], &[])),
            (
                "Adventurous",
                report_of(&["Navidrome", "deezer"], &[("ListenBrainz Labs", "down")]),
            ),
            (
                "Conservative",
                report_of(&["Navidrome"], &[("ListenBrainz Labs", "down")]),
            ),
        ] {
            store_run_in(&db, "alice", profile, &[], &report)
                .await
                .unwrap();
        }
        let rows = EngineReportRow::latest_per_profile_in(&db, "alice")
            .await
            .unwrap();
        let run = last_run_of(rows).unwrap();
        let mut sources = run.sources.clone();
        sources.sort();
        assert_eq!(sources, ["Navidrome", "catalog", "deezer"]);
        assert_eq!(
            run.source_errors,
            report_of(&[], &[("ListenBrainz Labs", "down")]).source_errors
        );
    }

    #[test]
    fn the_last_run_has_no_listening_data_only_when_no_profile_had_any() {
        let empty = EngineReport {
            state: EngineState::NoListeningData,
            ..Default::default()
        };
        let empty = report_row(&serde_json::to_string(&empty).unwrap());
        let state = |rows: Vec<EngineReportRow>| last_run_of(rows).unwrap().state;
        assert_eq!(
            state(vec![empty.clone(), empty.clone()]),
            EngineState::NoListeningData
        );
        let recommended = report_row(&serde_json::to_string(&EngineReport::default()).unwrap());
        assert_eq!(state(vec![empty, recommended]), EngineState::Recommended);
    }

    #[test]
    fn listenbrainz_needs_only_a_username_and_deezer_only_the_switch() {
        let mut s = UserSettings::defaults("alice");
        assert_eq!(plan_for(&s).listenbrainz_user, None);
        s.listenbrainz_username = Some(" rob ".into());
        s.listenbrainz_token = Some(String::new());
        let p = plan_for(&s);
        assert_eq!(
            (
                p.listenbrainz_user.as_deref(),
                p.listenbrainz_token,
                p.deezer
            ),
            (Some("rob"), None, false)
        );
        s.deezer_recommendations = true;
        assert!(plan_for(&s).deezer);
    }

    /// Makes `user` with a folder, Navidrome `status` and `discovery` settings.
    async fn user_with(db: &SqlitePool, user: &str, status: &str, discovery: UpdateUserSettings) {
        crate::core::folder_of(db, user).await;
        sqlx::query("UPDATE users SET navidrome_status = ? WHERE id = ?")
            .bind(status)
            .bind(user)
            .execute(db)
            .await
            .unwrap();
        UserSettings::upsert_in(db, user, discovery).await.unwrap();
    }

    fn discovery(enabled: bool, folder: Option<&str>) -> UpdateUserSettings {
        UpdateUserSettings {
            discovery_enabled: Some(enabled),
            discovery_folder_id: folder.map(str::to_string),
            ..Default::default()
        }
    }

    /// MusicBrainz for runs that must not reach it.
    struct NoMusicBrainz;

    #[async_trait::async_trait]
    impl MusicBrainzLookup for NoMusicBrainz {
        async fn search_artists(
            &self,
            name: &str,
            _: u8,
        ) -> Result<Vec<soulbeet::musicbrainz::ArtistMatch>, soulbeet::MetadataError> {
            panic!("this run asks MusicBrainz nothing, yet searched {name}")
        }

        async fn genre_names(&self) -> Result<Vec<String>, soulbeet::MetadataError> {
            panic!("this run asks MusicBrainz nothing, yet read its genres")
        }
    }

    /// A host nothing listens on, for services a run must not reach.
    const UNREACHED: &str = "http://127.0.0.1:9";

    /// The services of a run whose ListenBrainz is at `listenbrainz`, asked without pauses
    /// or retries.
    fn services_at(db: &SqlitePool, listenbrainz: &str) -> DiscoveryServices {
        let pace = Pace {
            min_interval: std::time::Duration::ZERO,
            backoff: std::time::Duration::ZERO,
            retries: 0,
            timeout: std::time::Duration::from_secs(5),
        };
        DiscoveryServices {
            hosts: KnowledgeHosts {
                listenbrainz: listenbrainz.into(),
                labs: UNREACHED.into(),
                deezer: UNREACHED.into(),
            },
            cache: Arc::new(SqliteKnowledgeCache::new(db.clone())),
            musicbrainz: Arc::new(NoMusicBrainz),
            metabrainz_pace: pace,
            deezer_pace: pace,
        }
    }

    #[tokio::test]
    async fn a_run_that_cannot_read_navidrome_says_so_for_each_profile_and_keeps_the_pool() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        store_run_in(
            &db,
            "alice",
            "Balanced",
            &[cand("A", "a1")],
            &EngineReport::default(),
        )
        .await
        .unwrap();
        let navi = NavidromeStub::start();
        navi.answer_when("/api/song", "_start=0", 502, BAD_GATEWAY);

        let f = generate_in(
            &db,
            &NavidromeFor(Ok(navi.client())),
            &services_at(&db, UNREACHED),
            &UserId::new("alice"),
            &[DiscoveryProfile::Balanced, DiscoveryProfile::Adventurous],
        )
        .await
        .unwrap_err();
        assert_eq!(f.code, FailureCode::NavidromeUnreachable);
        let rows = EngineReportRow::latest_per_profile_in(&db, "alice")
            .await
            .unwrap();
        let mut stored: Vec<&str> = rows.iter().map(|r| r.profile.as_str()).collect();
        stored.sort();
        assert_eq!(stored, ["Adventurous", "Balanced"]);
        let run = last_run_of(rows).unwrap();
        assert!(run.sources.is_empty());
        assert_eq!(
            run.source_errors,
            [SourceError {
                source: "Navidrome".into(),
                message: f.message.clone(),
                failures: 1,
            }]
        );
        assert_eq!(
            tracks(
                DiscoveryCandidateRow::get_unused_in(&db, "alice", "Balanced", 10)
                    .await
                    .unwrap()
            ),
            ["a1"]
        );
    }

    /// ListenBrainz's bot challenge page, captured.
    const CHALLENGE: &str =
        include_str!("../../../lib/soulbeet/src/listenbrainz/fixtures/challenge.html");

    /// The failure of alice's Balanced run with an empty Navidrome library, her ListenBrainz
    /// account being `listenbrainz_user` at `listenbrainz`.
    async fn empty_library_run(
        db: &SqlitePool,
        listenbrainz: &str,
        listenbrainz_user: Option<&str>,
    ) -> Failure {
        crate::core::folder_of(db, "alice").await;
        let settings = UpdateUserSettings {
            listenbrainz_username: listenbrainz_user.map(str::to_string),
            ..Default::default()
        };
        UserSettings::upsert_in(db, "alice", settings)
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        navi.answer("/api/song", &["[]"]);
        navi.header("/api/song", "X-Total-Count", "0");
        generate_in(
            db,
            &NavidromeFor(Ok(navi.client())),
            &services_at(db, listenbrainz),
            &UserId::new("alice"),
            &[DiscoveryProfile::Balanced],
        )
        .await
        .unwrap_err()
    }

    #[tokio::test]
    async fn no_listening_data_is_said_only_when_every_history_answered() {
        let db = test_db::migrated().await;
        let f = empty_library_run(&db, UNREACHED, None).await;
        assert_eq!(f, no_listening_data());
        let rows = EngineReportRow::latest_per_profile_in(&db, "alice")
            .await
            .unwrap();
        assert_eq!(
            last_run_of(rows).map(|run| run.state),
            Some(EngineState::NoListeningData)
        );
    }

    #[tokio::test]
    async fn no_seeds_while_a_history_failed_is_retried_later_and_names_it() {
        let db = test_db::migrated().await;
        // The stand-in answers any path, so it serves as ListenBrainz too.
        let listenbrainz = NavidromeStub::start();
        listenbrainz.answer("/1/stats/user/rob/artists", &[CHALLENGE]);
        let f = empty_library_run(&db, &listenbrainz.url, Some("rob")).await;
        assert_eq!(
            (f.code, f.retry, f.user_action),
            (
                FailureCode::MetadataUnavailable,
                RetryAdvice::SameSourceLater,
                None
            )
        );
        assert!(
            f.message
                .contains("ListenBrainz failed: ListenBrainz answered with a browser check"),
            "{}",
            f.message
        );
        let rows = EngineReportRow::latest_per_profile_in(&db, "alice")
            .await
            .unwrap();
        let run = last_run_of(rows).unwrap();
        assert_eq!(run.state, EngineState::Recommended);
        assert_eq!(
            (run.source_errors.iter())
                .map(|e| e.source.as_str())
                .collect::<Vec<_>>(),
            ["ListenBrainz"]
        );
    }

    #[tokio::test]
    async fn the_cycle_covers_only_connected_users_with_discovery_on_and_a_folder() {
        let db = test_db::migrated().await;
        let connected = NavidromeStatus::Connected.as_str();
        user_with(
            &db,
            "alice",
            connected,
            UpdateUserSettings {
                discovery_profiles: Some("Balanced,Adventurous".into()),
                ..discovery(true, Some("alice-folder"))
            },
        )
        .await;
        user_with(&db, "off", connected, discovery(false, Some("off-folder"))).await;
        user_with(&db, "nofolder", connected, discovery(true, None)).await;
        user_with(&db, "cleared", connected, discovery(true, Some(""))).await;
        let offline = NavidromeStatus::Offline.as_str();
        user_with(
            &db,
            "offline",
            offline,
            discovery(true, Some("offline-folder")),
        )
        .await;

        let due: Vec<(String, Vec<DiscoveryProfile>)> = due_in(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|(user, profiles)| (user.id, profiles))
            .collect();
        assert_eq!(
            due,
            [(
                "alice".to_string(),
                vec![DiscoveryProfile::Balanced, DiscoveryProfile::Adventurous]
            )]
        );
    }
}
