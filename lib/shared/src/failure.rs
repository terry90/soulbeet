use serde::{Deserialize, Serialize};

/// Longest `Cause.raw` kept, in characters.
pub const RAW_MAX_CHARS: usize = 2000;

/// Longest text field taken from another service into a tool result, in
/// characters, see `cut_chars`.
pub const TEXT_MAX_CHARS: usize = 500;

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

    pub fn with_retry_after(mut self, seconds: u32) -> Self {
        self.retry_after_seconds = Some(seconds);
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

/// `raw` cut by `cut_chars`, whose kept end holds a beets tail's fatal lines.
fn cut_raw(raw: String) -> String {
    cut_chars(raw, RAW_MAX_CHARS)
}

/// `text` when it has at most `max_chars` characters. A longer one keeps
/// its start and its end around a line saying how many characters were
/// left out, `max_chars` characters in all. Cuts fall on char boundaries.
/// `max_chars` must hold that line for the whole text left out (25
/// characters and one per digit of the count past the first); every caller
/// passes a constant of hundreds.
pub fn cut_chars(text: String, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let marker = |left_out: usize| format!("\n[{left_out} characters left out]\n");
    debug_assert!(
        max_chars >= marker(total).chars().count(),
        "cut_chars: {max_chars} characters cannot hold the line saying what was left out"
    );
    let mut kept = max_chars - marker(total).chars().count();
    // Each character kept is one fewer left out, whose count can be a
    // digit shorter: keep characters until the marker leaves no room.
    loop {
        let room = max_chars - marker(total - kept).chars().count();
        if room == kept {
            break;
        }
        kept = room;
    }
    let head = kept / 2;
    let tail = kept - head;
    let byte_at = |chars: usize| {
        text.char_indices()
            .nth(chars)
            .map_or(text.len(), |(i, _)| i)
    };
    let head_end = byte_at(head);
    let tail_start = byte_at(total - tail);
    format!(
        "{}{}{}",
        &text[..head_end],
        marker(total - kept),
        &text[tail_start..]
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
    /// A provider's own actions name the provider (slskd, beets), so the
    /// provider builds them.
    pub fn new(code: UserActionCode, what: impl Into<String>, where_: impl Into<String>) -> Self {
        Self {
            code,
            what: what.into(),
            where_: where_.into(),
        }
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
    /// Some items of a call failed: each failed item's own error says
    /// whether and how to retry it.
    PerItem,
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
    /// Soulful itself: lines it could not read from a service, or what it found wrong, in its
    /// own words.
    Soulful,
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
    ConfigureNavidrome,
    /// A metadata provider refused Soulful's key for it.
    ConfigureProvider,
    EnableDeletion,
    EnableIndexing,
    AddFolder,
}

/// Defines `FailureCode` from one list of variants and wire names: `as_str`
/// gives each name, and the tests check every variant against serde through
/// `ALL`, so a new variant cannot be left out of either.
macro_rules! failure_codes {
    ($($(#[$doc:meta])* $variant:ident => $name:literal,)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum FailureCode {
            $($(#[$doc])* $variant,)*
        }

        impl FailureCode {
            /// Every variant, in declaration order.
            #[cfg(test)]
            const ALL: &'static [FailureCode] = &[$(FailureCode::$variant,)*];

            /// The wire name, as serde writes it.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(FailureCode::$variant => $name,)*
                }
            }
        }
    };
}

failure_codes! {
    InvalidInput => "invalid_input",
    UnknownHandle => "unknown_handle",
    ExpiredHandle => "expired_handle",
    NotOwner => "not_owner",
    PermissionNotGranted => "permission_not_granted",
    FolderRequired => "folder_required",
    RateLimited => "rate_limited",
    MetadataUnavailable => "metadata_unavailable",
    MetadataNotFound => "metadata_not_found",
    MetadataRateLimited => "metadata_rate_limited",
    ProviderNotConfigured => "provider_not_configured",
    NavidromeUnreachable => "navidrome_unreachable",
    NavidromeAuthFailed => "navidrome_auth_failed",
    NavidromeNotLinked => "navidrome_not_linked",
    NavidromeCircuitOpen => "navidrome_circuit_open",
    /// Navidrome answered with a Subsonic error other than a refused login.
    NavidromeRefused => "navidrome_refused",
    BackendNotConfigured => "backend_not_configured",
    BackendUnreachable => "backend_unreachable",
    /// Soulful could not read its own settings database to find the backend.
    SettingsUnreadable => "settings_unreadable",
    /// The backend answered with something Soulful cannot parse. The
    /// downloads list has its own code, `transfer_list_unreadable`.
    BackendResponseUnreadable => "backend_response_unreadable",
    SearchRateLimited => "search_rate_limited",
    SearchNoResults => "search_no_results",
    /// Every response scored below the floor. Produced once the slskd client
    /// reports the files it dropped (build step 3, design 8.1 and 10.2).
    SearchAllFiltered => "search_all_filtered",
    SearchTimeout => "search_timeout",
    /// The backend refused the search request as unusable, for example a
    /// query of exclusion terms only.
    SearchQueryRejected => "search_query_rejected",
    /// The backend already runs a search under the token this one asked for.
    SearchAlreadyRunning => "search_already_running",
    /// The backend no longer has a search Soulful was polling, or this client
    /// no longer runs it: running the same search again is the remedy.
    SearchLost => "search_lost",
    PeerOffline => "peer_offline",
    AlreadyRunning => "already_running",
    AlreadyInFlightOtherUser => "already_in_flight_other_user",
    AlreadyInFlightOtherFolder => "already_in_flight_other_folder",
    EnqueueFailed => "enqueue_failed",
    SlskdTooOld => "slskd_too_old",
    TransferConnectionFailed => "transfer_connection_failed",
    TransferSizeMismatch => "transfer_size_mismatch",
    TransferRejectedNotShared => "transfer_rejected_not_shared",
    TransferRejectedPeerBusy => "transfer_rejected_peer_busy",
    TransferRejectedFailureLimit => "transfer_rejected_failure_limit",
    TransferRejectedOther => "transfer_rejected_other",
    TransferTimedOut => "transfer_timed_out",
    TransferAborted => "transfer_aborted",
    TransferCancelled => "transfer_cancelled",
    TransferNeverAppeared => "transfer_never_appeared",
    TransferDisappeared => "transfer_disappeared",
    TransferStuckInRemoteQueue => "transfer_stuck_in_remote_queue",
    TransferListUnreadable => "transfer_list_unreadable",
    FileNotFoundAfterDownload => "file_not_found_after_download",
    ImportNoMatch => "import_no_match",
    ImportAlreadyInLibrary => "import_already_in_library",
    ImportNothingToImport => "import_nothing_to_import",
    ImportLookupFailed => "import_lookup_failed",
    ImportBeetsFailed => "import_beets_failed",
    ImportTargetUnwritable => "import_target_unwritable",
    /// The disk beets writes to is full.
    ImportDiskFull => "import_disk_full",
    ImportIdleTimeout => "import_idle_timeout",
    ImportTotalTimeout => "import_total_timeout",
    ImporterBroken => "importer_broken",
    ImportAsIsMissingTags => "import_as_is_missing_tags",
    ImportChoiceInvalid => "import_choice_invalid",
    ImportFilesGone => "import_files_gone",
    ImportPartial => "import_partial",
    IndexNotFoundAfterScan => "index_not_found_after_scan",
    IndexNoScanPossible => "index_no_scan_possible",
    ScanFailed => "scan_failed",
    PlaylistNotFound => "playlist_not_found",
    PlaylistNotOwner => "playlist_not_owner",
    PlaylistNameTaken => "playlist_name_taken",
    PlaylistTracksNotWritten => "playlist_tracks_not_written",
    DeletePathUnresolved => "delete_path_unresolved",
    DeleteOutsideLibrary => "delete_outside_library",
    DeleteNotATrack => "delete_not_a_track",
    DeleteBlockedByOtherUser => "delete_blocked_by_other_user",
    DeleteVetoUnknown => "delete_veto_unknown",
    DeleteInUse => "delete_in_use",
    DeleteChangedSincePreview => "delete_changed_since_preview",
    /// The OS refused to remove a file every rule allowed removing.
    DeleteFailed => "delete_failed",
    DeleteDailyLimit => "delete_daily_limit",
    ServerRestarted => "server_restarted",
    /// Some items of a call failed and the others were done (design 10.1).
    Partial => "partial",
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Design section 7 and 13 codes, by stage, with `settings_unreadable`,
    /// `backend_response_unreadable`, `search_query_rejected`,
    /// `search_already_running`, `search_lost`, `import_disk_full`,
    /// `delete_failed` and `partial` (design 10.1), and
    /// without `peer_rejected_enqueue`, which nothing produces. The one
    /// normative list of FailureCode names.
    const NAMES: [&str; 79] = [
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
        "navidrome_refused",
        "backend_not_configured",
        "backend_unreachable",
        "settings_unreadable",
        "backend_response_unreadable",
        "search_rate_limited",
        "search_no_results",
        "search_all_filtered",
        "search_timeout",
        "search_query_rejected",
        "search_already_running",
        "search_lost",
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
        "delete_failed",
        "delete_daily_limit",
        "server_restarted",
        "partial",
    ];

    #[test]
    fn every_failure_code_has_its_section_7_name() {
        for code in FailureCode::ALL {
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::json!(code.as_str()),
                "{code:?}"
            );
        }
        let wire: std::collections::BTreeSet<&str> =
            FailureCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            wire.len(),
            FailureCode::ALL.len(),
            "two variants share a name"
        );
        assert_eq!(wire, NAMES.into_iter().collect());
        for name in NAMES {
            let code: FailureCode = serde_json::from_value(serde_json::json!(name)).expect(name);
            assert_eq!(code.as_str(), name);
        }
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
    fn fix_download_path_names_the_folder() {
        let path = UserAction::fix_download_path(std::path::Path::new("/downloads"));
        assert_eq!(path.code, UserActionCode::FixDownloadPath);
        assert_eq!(
            path.what,
            "Make the folder the download backend saves into readable by Soulful at /downloads"
        );
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

    #[test]
    fn per_item_advice_is_written_per_item() {
        assert_eq!(
            serde_json::to_value(RetryAdvice::PerItem).unwrap(),
            "per_item"
        );
    }

    #[test]
    fn a_text_cut_at_500_characters_has_500_and_says_what_was_left_out() {
        let cut = cut_chars("x".repeat(10_000), TEXT_MAX_CHARS);
        assert_eq!(cut.chars().count(), TEXT_MAX_CHARS);
        assert!(cut.contains(" characters left out]"), "{cut}");
    }

    #[test]
    fn retry_after_is_kept() {
        let f = Failure::new(
            FailureCode::RateLimited,
            Stage::Input,
            RetryAdvice::SameSourceLater,
            "x",
        )
        .with_retry_after(7);
        assert_eq!(f.retry_after_seconds, Some(7));
    }
}
