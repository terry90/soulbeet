//! One recommendation run for a user: the profile built from their Navidrome
//! library and, when saved, their ListenBrainz account, then each discovery
//! profile's candidates from the generators the user's settings turn on.

use std::sync::Arc;

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::navidrome::LastRun;
use shared::recommendation::{
    Candidate, DiscoveryProfile, EngineReport, ProfileConfig, NO_LISTENING_DATA,
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
use tracing::info;

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
/// be read, since without it the user's own music could be recommended back to them, and
/// when the profile has nothing to start from, after storing the reports that say so.
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
    let plan = plan_for(&settings);

    let navi = navidrome.navidrome_of(user).await.map_err(|e| {
        e.failure(
            Stage::Input,
            format!("Discovery needs your Navidrome library: {e}"),
        )
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
        return Err(no_listening_data());
    }
    Ok(total)
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

/// The latest run as the Discovery page shows it. None when there is no run, or its report
/// does not parse.
pub(crate) fn last_run_of(row: Option<EngineReportRow>) -> Option<LastRun> {
    let row = row?;
    let report: EngineReport = serde_json::from_str(&row.report_json).ok()?;
    Some(LastRun {
        created_at: row.created_at,
        state: report.state,
        sources: report.sources,
        source_errors: report.source_errors,
    })
}

/// The Navidrome library could not be read in full.
fn library_failure(e: HistoryError) -> Failure {
    match &e {
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
    }
}

/// A remote client that cannot be set up: its host is not a URL, or no HTTP client builds.
fn client_failure(e: RemoteError) -> Failure {
    Failure::new(
        FailureCode::MetadataUnavailable,
        Stage::Input,
        RetryAdvice::NotRetryable,
        e.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use crate::models::user_settings::UpdateUserSettings;
    use shared::recommendation::{EngineState, SourceError};
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
            last_run_of(Some(report_row(REPORT_0_9_0))),
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
        let run = last_run_of(Some(row)).unwrap();
        assert_eq!(
            (run.state, run.sources),
            (EngineState::NoListeningData, vec!["Navidrome".to_string()])
        );
        assert_eq!(last_run_of(Some(report_row("not json"))), None);
        assert_eq!(last_run_of(None), None);
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
