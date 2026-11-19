//! One recommendation run for a user: the profile built from their Navidrome
//! library and, when saved, their ListenBrainz account, then each discovery
//! profile's candidates from the generators the user's settings turn on.

use std::sync::Arc;

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::recommendation::{DiscoveryProfile, ProfileConfig};
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
use crate::core::index::NavidromeOf;
use crate::db::DB;
use crate::models::discovery_candidate::DiscoveryCandidateRow;
use crate::models::engine_report::EngineReportRow;
use crate::models::knowledge_cache::SqliteKnowledgeCache;
use crate::models::user_profile::UserProfileRow;
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

/// Builds the user's profile once, then stores each of `profiles`' candidates and report.
/// Returns how many candidates were stored in all. Fails when the Navidrome library cannot
/// be read: without it, the user's own music could be recommended back to them.
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
        let report_json = serde_json::to_string(&report).map_err(|e| stored(e.to_string()))?;
        EngineReportRow::insert_in(db, &user_id, name, &report_json, candidates.len() as u32)
            .await
            .map_err(stored)?;
        EngineReportRow::prune_in(db, &user_id, REPORTS_KEPT)
            .await
            .map_err(stored)?;
        DiscoveryCandidateRow::clear_for_user_profile_in(db, &user_id, name)
            .await
            .map_err(stored)?;
        DiscoveryCandidateRow::upsert_batch_in(db, &user_id, name, &candidates)
            .await
            .map_err(stored)?;
        info!("{name}: {} candidates for {user_id}", candidates.len());
        total += candidates.len() as u32;
    }
    UserProfileRow::upsert_in(db, &user_id, &build.profile)
        .await
        .map_err(stored)?;
    Ok(total)
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
    use shared::recommendation::UserMusicProfile;

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

    #[tokio::test]
    async fn a_profile_stored_before_the_hash_column_went_still_saves() {
        let db = test_db::migrated().await;
        UserProfileRow::upsert_in(&db, "alice", &UserMusicProfile::default())
            .await
            .unwrap();
        UserProfileRow::upsert_in(&db, "alice", &UserMusicProfile::default())
            .await
            .unwrap();
        assert_eq!(
            UserProfileRow::get_report_in(&db, "alice").await.unwrap(),
            None
        );
    }
}
