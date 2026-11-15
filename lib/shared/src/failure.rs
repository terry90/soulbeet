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

    /// raw is cut as `Cause::new` cuts it.
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
    /// raw is cut to RAW_MAX_CHARS characters, see `cut_raw`.
    pub fn new(service: Service, status: Option<i32>, raw: impl Into<String>) -> Self {
        Self {
            service,
            status,
            raw: cut_raw(raw.into()),
        }
    }
}

/// `raw` when it has at most RAW_MAX_CHARS characters. A longer one keeps
/// its start and its end, which for a beets tail holds the fatal lines,
/// around a line saying how many characters were left out, all within
/// RAW_MAX_CHARS characters. Cuts fall on char boundaries.
fn cut_raw(raw: String) -> String {
    let total = raw.chars().count();
    if total <= RAW_MAX_CHARS {
        return raw;
    }
    let marker = |left_out: usize| format!("\n[{left_out} characters left out]\n");
    // The marker for `total` is at least as long as the one written.
    let kept = RAW_MAX_CHARS - marker(total).chars().count();
    let head = kept / 2;
    let tail = kept - head;
    let byte_at = |chars: usize| raw.char_indices().nth(chars).map_or(raw.len(), |(i, _)| i);
    let head_end = byte_at(head);
    let tail_start = byte_at(total - tail);
    format!(
        "{}{}{}",
        &raw[..head_end],
        marker(total - kept),
        &raw[tail_start..]
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAction {
    pub code: UserActionCode,
    pub what: String,
    #[serde(rename = "where")]
    pub where_: String,
}

impl UserAction {
    /// A backend's own actions name the backend, so its provider builds them.
    pub fn new(code: UserActionCode, what: impl Into<String>, where_: impl Into<String>) -> Self {
        Self {
            code,
            what: what.into(),
            where_: where_.into(),
        }
    }

    pub fn configure_backend() -> Self {
        Self::new(
            UserActionCode::ConfigureBackend,
            "Set the slskd URL and API key",
            "Settings > Config",
        )
    }

    pub fn upgrade_backend() -> Self {
        Self::new(
            UserActionCode::UpgradeBackend,
            "Upgrade slskd to 0.26 or newer",
            "slskd",
        )
    }

    pub fn fix_importer(message: &str) -> Self {
        Self::new(
            UserActionCode::FixImporter,
            format!("Fix the beets setup: {message}"),
            "Settings > Config",
        )
    }

    /// The file a finished download was saved as is not where Soulful looks:
    /// in `download_path` or a folder in it.
    pub fn fix_download_path(download_path: &std::path::Path) -> Self {
        Self::new(
            UserActionCode::FixDownloadPath,
            format!(
                "Make the folder the download backend saves into readable by Soulful at {}",
                download_path.display()
            ),
            "DOWNLOAD_PATH and the volume mounted there",
        )
    }

    /// beets' line naming what it could not write: `message`.
    pub fn free_disk_space(message: &str) -> Self {
        Self::new(
            UserActionCode::FreeDiskSpace,
            format!("Free disk space: {message}"),
            "The disk that holds the path in the message",
        )
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
    /// Soulful's own settings database.
    Database,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserActionCode {
    ConfigureBackend,
    UpgradeBackend,
    /// Soulful and the backend no longer speak the same API.
    UpdateSoulfulOrBackend,
    FixImporter,
    FixDownloadPath,
    FreeDiskSpace,
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
    /// Every response scored below the floor. Produced once the slskd client
    /// reports the files it dropped (build step 3, design 8.1 and 10.2).
    SearchAllFiltered,
    SearchTimeout,
    /// The backend refused the search request as unusable, for example a
    /// query of exclusion terms only.
    SearchQueryRejected,
    /// The backend already runs a search under the token this one asked for.
    SearchAlreadyRunning,
    PeerOffline,
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
    /// The disk beets writes to is full.
    ImportDiskFull,
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

impl FailureCode {
    /// The wire name, as serde writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            FailureCode::InvalidInput => "invalid_input",
            FailureCode::UnknownHandle => "unknown_handle",
            FailureCode::ExpiredHandle => "expired_handle",
            FailureCode::NotOwner => "not_owner",
            FailureCode::PermissionNotGranted => "permission_not_granted",
            FailureCode::FolderRequired => "folder_required",
            FailureCode::RateLimited => "rate_limited",
            FailureCode::MetadataUnavailable => "metadata_unavailable",
            FailureCode::MetadataNotFound => "metadata_not_found",
            FailureCode::MetadataRateLimited => "metadata_rate_limited",
            FailureCode::ProviderNotConfigured => "provider_not_configured",
            FailureCode::NavidromeUnreachable => "navidrome_unreachable",
            FailureCode::NavidromeAuthFailed => "navidrome_auth_failed",
            FailureCode::NavidromeNotLinked => "navidrome_not_linked",
            FailureCode::NavidromeCircuitOpen => "navidrome_circuit_open",
            FailureCode::BackendNotConfigured => "backend_not_configured",
            FailureCode::BackendUnreachable => "backend_unreachable",
            FailureCode::SearchRateLimited => "search_rate_limited",
            FailureCode::SearchNoResults => "search_no_results",
            FailureCode::SearchAllFiltered => "search_all_filtered",
            FailureCode::SearchTimeout => "search_timeout",
            FailureCode::SearchQueryRejected => "search_query_rejected",
            FailureCode::SearchAlreadyRunning => "search_already_running",
            FailureCode::PeerOffline => "peer_offline",
            FailureCode::AlreadyRunning => "already_running",
            FailureCode::AlreadyInFlightOtherUser => "already_in_flight_other_user",
            FailureCode::AlreadyInFlightOtherFolder => "already_in_flight_other_folder",
            FailureCode::EnqueueFailed => "enqueue_failed",
            FailureCode::SlskdTooOld => "slskd_too_old",
            FailureCode::TransferConnectionFailed => "transfer_connection_failed",
            FailureCode::TransferSizeMismatch => "transfer_size_mismatch",
            FailureCode::TransferRejectedNotShared => "transfer_rejected_not_shared",
            FailureCode::TransferRejectedPeerBusy => "transfer_rejected_peer_busy",
            FailureCode::TransferRejectedFailureLimit => "transfer_rejected_failure_limit",
            FailureCode::TransferRejectedOther => "transfer_rejected_other",
            FailureCode::TransferTimedOut => "transfer_timed_out",
            FailureCode::TransferAborted => "transfer_aborted",
            FailureCode::TransferCancelled => "transfer_cancelled",
            FailureCode::TransferNeverAppeared => "transfer_never_appeared",
            FailureCode::TransferDisappeared => "transfer_disappeared",
            FailureCode::TransferStuckInRemoteQueue => "transfer_stuck_in_remote_queue",
            FailureCode::TransferListUnreadable => "transfer_list_unreadable",
            FailureCode::FileNotFoundAfterDownload => "file_not_found_after_download",
            FailureCode::ImportNoMatch => "import_no_match",
            FailureCode::ImportAlreadyInLibrary => "import_already_in_library",
            FailureCode::ImportNothingToImport => "import_nothing_to_import",
            FailureCode::ImportLookupFailed => "import_lookup_failed",
            FailureCode::ImportBeetsFailed => "import_beets_failed",
            FailureCode::ImportTargetUnwritable => "import_target_unwritable",
            FailureCode::ImportDiskFull => "import_disk_full",
            FailureCode::ImportIdleTimeout => "import_idle_timeout",
            FailureCode::ImportTotalTimeout => "import_total_timeout",
            FailureCode::ImporterBroken => "importer_broken",
            FailureCode::ImportAsIsMissingTags => "import_as_is_missing_tags",
            FailureCode::ImportChoiceInvalid => "import_choice_invalid",
            FailureCode::ImportFilesGone => "import_files_gone",
            FailureCode::ImportPartial => "import_partial",
            FailureCode::IndexNotFoundAfterScan => "index_not_found_after_scan",
            FailureCode::IndexNoScanPossible => "index_no_scan_possible",
            FailureCode::ScanFailed => "scan_failed",
            FailureCode::PlaylistNotFound => "playlist_not_found",
            FailureCode::PlaylistNotOwner => "playlist_not_owner",
            FailureCode::PlaylistNameTaken => "playlist_name_taken",
            FailureCode::PlaylistTracksNotWritten => "playlist_tracks_not_written",
            FailureCode::DeletePathUnresolved => "delete_path_unresolved",
            FailureCode::DeleteOutsideLibrary => "delete_outside_library",
            FailureCode::DeleteNotATrack => "delete_not_a_track",
            FailureCode::DeleteBlockedByOtherUser => "delete_blocked_by_other_user",
            FailureCode::DeleteVetoUnknown => "delete_veto_unknown",
            FailureCode::DeleteInUse => "delete_in_use",
            FailureCode::DeleteChangedSincePreview => "delete_changed_since_preview",
            FailureCode::DeleteDailyLimit => "delete_daily_limit",
            FailureCode::ServerRestarted => "server_restarted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Design section 7 and 13 codes, by stage, with `search_query_rejected`,
    /// `search_already_running` and `import_disk_full`, and without
    /// `peer_rejected_enqueue`, which nothing produces. The one normative list
    /// of FailureCode names.
    const NAMES: [&str; 73] = [
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
        "search_query_rejected",
        "search_already_running",
        "peer_offline",
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
        "import_disk_full",
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
            assert_eq!(code.as_str(), name);
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
        .with_user_action(UserAction::new(
            UserActionCode::UpgradeBackend,
            "Upgrade slskd to 0.26 or newer",
            "slskd",
        ));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(
            v["user_action"],
            serde_json::json!({"code": "upgrade_backend",
                "what": "Upgrade slskd to 0.26 or newer", "where": "slskd"})
        );
    }

    #[test]
    fn user_actions_name_what_to_fix() {
        let path = UserAction::fix_download_path(std::path::Path::new("/downloads"));
        assert_eq!(path.code, UserActionCode::FixDownloadPath);
        assert_eq!(
            path.what,
            "Make the folder the download backend saves into readable by Soulful at /downloads"
        );
        let full = "Error: No space left on device while creating /music/A";
        let disk = UserAction::free_disk_space(full);
        assert_eq!(disk.code, UserActionCode::FreeDiskSpace);
        assert_eq!(disk.what, format!("Free disk space: {full}"));
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

    fn raw_of(raw: String) -> String {
        Failure::new(
            FailureCode::ImportBeetsFailed,
            Stage::Import,
            RetryAdvice::SameSourceLater,
            "m",
        )
        .with_cause(Service::Beets, None, raw)
        .cause
        .unwrap()
        .raw
    }

    #[test]
    fn a_raw_text_up_to_2000_chars_is_kept_whole() {
        for raw in ["x".repeat(10), "é".repeat(RAW_MAX_CHARS), String::new()] {
            assert_eq!(raw_of(raw.clone()), raw);
        }
    }

    /// A beets tail of 2,048 bytes ends with the line that says what failed.
    #[test]
    fn a_long_raw_text_keeps_its_start_and_its_end() {
        let lines: Vec<String> = (1..=20)
            .map(|i| format!("{i:02}{}", "-".repeat(100)))
            .collect();
        let fatal = "Error: Permission denied while creating /music/A/B";
        let raw = format!("{}\n{fatal}", lines.join("\n"));
        let total = raw.chars().count();
        assert!(total > RAW_MAX_CHARS);

        let cut = raw_of(raw.clone());

        assert!(cut.chars().count() <= RAW_MAX_CHARS);
        assert!(cut.starts_with(&lines[0]), "{cut}");
        assert!(cut.ends_with(fatal), "{cut}");
        let (head, rest) = cut.split_once("\n[").unwrap();
        let (marker, tail) = rest.split_once("]\n").unwrap();
        let left_out = total - head.chars().count() - tail.chars().count();
        assert_eq!(marker, format!("{left_out} characters left out"));
        assert!(raw.starts_with(head) && raw.ends_with(tail));
    }

    #[test]
    fn a_long_multi_byte_raw_text_is_cut_on_char_boundaries() {
        let raw = format!("{}{}", "é".repeat(1500), "日本".repeat(800));
        let cut = raw_of(raw.clone());
        assert!(cut.chars().count() <= RAW_MAX_CHARS);
        assert!(cut.starts_with('é') && cut.ends_with('本'));
        let (head, rest) = cut.split_once("\n[").unwrap();
        let (_, tail) = rest.split_once("]\n").unwrap();
        assert!(raw.starts_with(head) && raw.ends_with(tail));
        let one_over = raw_of(format!("{}x", "é".repeat(RAW_MAX_CHARS)));
        assert!(one_over.chars().count() <= RAW_MAX_CHARS);
        assert!(one_over.ends_with("éx") && one_over.contains(" characters left out]"));
    }
}
