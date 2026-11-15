use serde::{Deserialize, Serialize};

/// Longest `Cause.raw` kept, in characters.
pub const RAW_MAX_CHARS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub code: FailureCode,
    pub stage: Stage,
    pub message: String,
    pub cause: Option<Cause>,
    pub retry: RetryAdvice,
    pub retry_after_seconds: Option<u32>,
    pub user_action: Option<UserAction>,
}

impl Failure {
    /// cause, retry_after_seconds and user_action start as None.
    pub fn new(
        code: FailureCode,
        stage: Stage,
        retry: RetryAdvice,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            stage,
            message: message.into(),
            cause: None,
            retry,
            retry_after_seconds: None,
            user_action: None,
        }
    }

    /// raw is cut to RAW_MAX_CHARS characters on a char boundary.
    pub fn with_cause(
        mut self,
        service: Service,
        status: Option<i32>,
        raw: impl Into<String>,
    ) -> Self {
        self.cause = Some(Cause::new(service, status, raw));
        self
    }

    pub fn with_user_action(mut self, action: UserAction) -> Self {
        self.user_action = Some(action);
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cause {
    pub service: Service,
    pub status: Option<i32>,
    pub raw: String,
}

impl Cause {
    /// raw is cut to RAW_MAX_CHARS characters on a char boundary.
    pub fn new(service: Service, status: Option<i32>, raw: impl Into<String>) -> Self {
        let raw: String = raw.into();
        let raw = match raw.char_indices().nth(RAW_MAX_CHARS) {
            Some((end, _)) => raw[..end].to_string(),
            None => raw,
        };
        Self {
            service,
            status,
            raw,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAction {
    pub code: UserActionCode,
    pub what: String,
    #[serde(rename = "where")]
    pub where_: String,
}

impl UserAction {
    pub fn configure_backend() -> Self {
        Self {
            code: UserActionCode::ConfigureBackend,
            what: "Set the slskd URL and API key".to_string(),
            where_: "Settings > Config".to_string(),
        }
    }

    pub fn fix_importer(message: &str) -> Self {
        Self {
            code: UserActionCode::FixImporter,
            what: format!("Fix the beets setup: {message}"),
            where_: "Settings > Config".to_string(),
        }
    }

    pub fn upgrade_backend() -> Self {
        Self {
            code: UserActionCode::UpgradeBackend,
            what: "Upgrade slskd to 0.26 or newer".to_string(),
            where_: "slskd".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryAdvice {
    SameSourceLater,
    OtherSource,
    OtherQuery,
    NeedsDecision,
    NeedsUser,
    NotRetryable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Auth,
    Input,
    Metadata,
    LibraryCheck,
    SourceSearch,
    Enqueue,
    Transfer,
    FileLookup,
    Import,
    Index,
    Playlist,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Slskd,
    Beets,
    Navidrome,
    Musicbrainz,
    Lastfm,
    Listenbrainz,
    Filesystem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserActionCode {
    ConfigureBackend,
    UpgradeBackend,
    FixImporter,
    RelinkNavidrome,
    EnableDeletion,
    EnableIndexing,
    AddFolder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    InvalidInput,
    UnknownHandle,
    ExpiredHandle,
    NotOwner,
    PermissionNotGranted,
    FolderRequired,
    RateLimited,
    MetadataUnavailable,
    MetadataNotFound,
    MetadataRateLimited,
    ProviderNotConfigured,
    NavidromeUnreachable,
    NavidromeAuthFailed,
    NavidromeNotLinked,
    NavidromeCircuitOpen,
    BackendNotConfigured,
    BackendUnreachable,
    SearchRateLimited,
    SearchNoResults,
    SearchAllFiltered,
    SearchTimeout,
    PeerOffline,
    PeerRejectedEnqueue,
    AlreadyRunning,
    AlreadyInFlightOtherUser,
    AlreadyInFlightOtherFolder,
    EnqueueFailed,
    SlskdTooOld,
    TransferConnectionFailed,
    TransferSizeMismatch,
    TransferRejectedNotShared,
    TransferRejectedPeerBusy,
    TransferRejectedFailureLimit,
    TransferRejectedOther,
    TransferTimedOut,
    TransferAborted,
    TransferCancelled,
    TransferNeverAppeared,
    TransferDisappeared,
    TransferStuckInRemoteQueue,
    TransferListUnreadable,
    FileNotFoundAfterDownload,
    ImportNoMatch,
    ImportAlreadyInLibrary,
    ImportNothingToImport,
    ImportLookupFailed,
    ImportBeetsFailed,
    ImportTargetUnwritable,
    ImportIdleTimeout,
    ImportTotalTimeout,
    ImporterBroken,
    ImportAsIsMissingTags,
    ImportChoiceInvalid,
    ImportFilesGone,
    ImportPartial,
    IndexNotFoundAfterScan,
    IndexNoScanPossible,
    ScanFailed,
    PlaylistNotFound,
    PlaylistNotOwner,
    PlaylistNameTaken,
    PlaylistTracksNotWritten,
    DeletePathUnresolved,
    DeleteOutsideLibrary,
    DeleteNotATrack,
    DeleteBlockedByOtherUser,
    DeleteVetoUnknown,
    DeleteInUse,
    DeleteChangedSincePreview,
    DeleteDailyLimit,
    ServerRestarted,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Design section 7 and 13 codes, by stage. The one normative list of FailureCode names.
    const NAMES: [&str; 71] = [
        "invalid_input",
        "unknown_handle",
        "expired_handle",
        "not_owner",
        "permission_not_granted",
        "folder_required",
        "rate_limited",
        "metadata_unavailable",
        "metadata_not_found",
        "metadata_rate_limited",
        "provider_not_configured",
        "navidrome_unreachable",
        "navidrome_auth_failed",
        "navidrome_not_linked",
        "navidrome_circuit_open",
        "backend_not_configured",
        "backend_unreachable",
        "search_rate_limited",
        "search_no_results",
        "search_all_filtered",
        "search_timeout",
        "peer_offline",
        "peer_rejected_enqueue",
        "already_running",
        "already_in_flight_other_user",
        "already_in_flight_other_folder",
        "enqueue_failed",
        "slskd_too_old",
        "transfer_connection_failed",
        "transfer_size_mismatch",
        "transfer_rejected_not_shared",
        "transfer_rejected_peer_busy",
        "transfer_rejected_failure_limit",
        "transfer_rejected_other",
        "transfer_timed_out",
        "transfer_aborted",
        "transfer_cancelled",
        "transfer_never_appeared",
        "transfer_disappeared",
        "transfer_stuck_in_remote_queue",
        "transfer_list_unreadable",
        "file_not_found_after_download",
        "import_no_match",
        "import_already_in_library",
        "import_nothing_to_import",
        "import_lookup_failed",
        "import_beets_failed",
        "import_target_unwritable",
        "import_idle_timeout",
        "import_total_timeout",
        "importer_broken",
        "import_as_is_missing_tags",
        "import_choice_invalid",
        "import_files_gone",
        "import_partial",
        "index_not_found_after_scan",
        "index_no_scan_possible",
        "scan_failed",
        "playlist_not_found",
        "playlist_not_owner",
        "playlist_name_taken",
        "playlist_tracks_not_written",
        "delete_path_unresolved",
        "delete_outside_library",
        "delete_not_a_track",
        "delete_blocked_by_other_user",
        "delete_veto_unknown",
        "delete_in_use",
        "delete_changed_since_preview",
        "delete_daily_limit",
        "server_restarted",
    ];

    #[test]
    fn every_failure_code_has_its_section_7_name() {
        let mut codes = std::collections::HashSet::new();
        for name in NAMES {
            let code: FailureCode = serde_json::from_value(serde_json::json!(name)).expect(name);
            assert_eq!(serde_json::to_value(code).unwrap(), serde_json::json!(name));
            codes.insert(code);
        }
        assert_eq!(codes.len(), NAMES.len(), "two names map to one variant");
    }

    #[test]
    fn a_failure_serializes_like_the_tool_contract() {
        let f = Failure::new(
            FailureCode::TransferRejectedNotShared,
            Stage::Transfer,
            RetryAdvice::OtherSource,
            "File not shared.",
        )
        .with_cause(Service::Slskd, None, "File not shared.");
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            serde_json::json!({
                "code": "transfer_rejected_not_shared", "stage": "transfer", "message": "File not shared.",
                "cause": {"service": "slskd", "status": null, "raw": "File not shared."},
                "retry": "other_source", "retry_after_seconds": null, "user_action": null })
        );
    }

    #[test]
    fn user_action_where_is_serialized_as_where() {
        let f = Failure::new(
            FailureCode::SlskdTooOld,
            Stage::Enqueue,
            RetryAdvice::NeedsUser,
            "x",
        )
        .with_user_action(UserAction::upgrade_backend());
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(
            v["user_action"],
            serde_json::json!({"code": "upgrade_backend",
                "what": "Upgrade slskd to 0.26 or newer", "where": "slskd"})
        );
    }

    #[test]
    fn user_actions_carry_their_texts() {
        let action = |code, what: &str, where_: &str| UserAction {
            code,
            what: what.to_string(),
            where_: where_.to_string(),
        };
        assert_eq!(
            UserAction::configure_backend(),
            action(
                UserActionCode::ConfigureBackend,
                "Set the slskd URL and API key",
                "Settings > Config"
            )
        );
        assert_eq!(
            UserAction::upgrade_backend(),
            action(
                UserActionCode::UpgradeBackend,
                "Upgrade slskd to 0.26 or newer",
                "slskd"
            )
        );
    }

    #[test]
    fn fix_importer_carries_the_message() {
        let a = UserAction::fix_importer("No metadata source plugin loaded");
        assert_eq!(a.code, UserActionCode::FixImporter);
        assert_eq!(
            a.what,
            "Fix the beets setup: No metadata source plugin loaded"
        );
        assert_eq!(a.where_, "Settings > Config");
    }

    #[test]
    fn with_cause_cuts_raw_at_2000_chars_on_a_char_boundary() {
        let f = Failure::new(
            FailureCode::ImportBeetsFailed,
            Stage::Import,
            RetryAdvice::SameSourceLater,
            "m",
        )
        .with_cause(Service::Beets, None, "é".repeat(2500));
        assert_eq!(f.cause.unwrap().raw.chars().count(), RAW_MAX_CHARS);
    }
}
