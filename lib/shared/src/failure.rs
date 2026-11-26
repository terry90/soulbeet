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

/// How many characters `c` takes in JSON text as serde_json writes it: `"`,
/// `\` and the control characters with a short escape (`\n`, `\t`, `\r`,
/// `\b`, `\f`) take 2, the other control characters 6 (`\u0001`), any other
/// character 1.
fn json_width(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\t' | '\r' | '\u{8}' | '\u{c}' => 2,
        '\0'..='\u{1f}' => 6,
        _ => 1,
    }
}

/// How many characters `text` takes inside a JSON string, quotes not counted.
pub fn json_chars(text: &str) -> usize {
    text.chars().map(json_width).sum()
}

/// `text` when it takes at most `max_chars` characters inside a JSON string
/// (`json_chars`). A longer one keeps its start and its end around a line
/// saying how many characters were left out, at most `max_chars` characters
/// of JSON in all, so a bound on a field holds as the field is sent whatever
/// the field holds. Cuts fall on char boundaries. `max_chars` must hold that
/// line; every caller passes a constant of hundreds.
pub fn cut_json(text: String, max_chars: usize) -> String {
    if json_chars(&text) <= max_chars {
        return text;
    }
    let count = text.chars().count();
    let marker = |left_out: usize| format!("\n[{left_out} characters left out]\n");
    // No more characters are left out than the text has, so the line takes
    // at most this much.
    let marker_room = json_chars(&marker(count));
    debug_assert!(
        max_chars >= marker_room,
        "cut_json: {max_chars} characters cannot hold the line saying what was left out"
    );
    let room = max_chars - marker_room;
    let (mut head_end, mut head_chars, mut used) = (0, 0, 0);
    for (at, c) in text.char_indices() {
        if used + json_width(c) > room / 2 {
            break;
        }
        used += json_width(c);
        head_end = at + c.len_utf8();
        head_chars += 1;
    }
    let (mut tail_start, mut tail_chars) = (text.len(), 0);
    for (at, c) in text.char_indices().rev() {
        if used + json_width(c) > room {
            break;
        }
        used += json_width(c);
        tail_start = at;
        tail_chars += 1;
    }
    format!(
        "{}{}{}",
        &text[..head_end],
        marker(count - head_chars - tail_chars),
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
                "Make the folder the download backend saves into readable by Soulbeet at {}",
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
    Move,
    Retag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Slskd,
    Beets,
    Navidrome,
    Musicbrainz,
    Filesystem,
    /// Soulful's own settings database.
    Database,
    /// Soulful itself: lines it could not read from a service, or what it found wrong, in its
    /// own words.
    Soulful,
    /// archive.org, the Internet Archive download backend's service.
    InternetArchive,
    /// YouTube, the YouTube Music download backend's service.
    Youtube,
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
    EnableDeletion,
    EnableIndexing,
    AddFolder,
    /// A change Soulful does not make for a connected app, which the user can make in Navidrome.
    RateInNavidrome,
    /// A setting only the user changes, in Soulful's own Settings.
    EnableAutoDelete,
    /// The discovery folder or promote threshold, which only the user changes, in Soulful's own
    /// Settings.
    ChangeDiscoverySettings,
    /// Discovery has nothing to recommend from until the user plays or stars music in Navidrome.
    ListenInNavidrome,
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
    PermissionNotGranted => "permission_not_granted",
    FolderRequired => "folder_required",
    /// Nothing played or starred in Navidrome and no ListenBrainz history to recommend from,
    /// every history having answered.
    NoListeningData => "no_listening_data",
    RateLimited => "rate_limited",
    MetadataUnavailable => "metadata_unavailable",
    MetadataNotFound => "metadata_not_found",
    MetadataRateLimited => "metadata_rate_limited",
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
    /// The file's bytes do not match the checksum its source gave for it.
    TransferChecksumMismatch => "transfer_checksum_mismatch",
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
    /// The disk a backend writes downloads to is full.
    DownloadDiskFull => "download_disk_full",
    ImportNoMatch => "import_no_match",
    ImportAlreadyInLibrary => "import_already_in_library",
    ImportNothingToImport => "import_nothing_to_import",
    ImportLookupFailed => "import_lookup_failed",
    ImportBeetsFailed => "import_beets_failed",
    ImportTargetUnwritable => "import_target_unwritable",
    /// The disk beets writes to is full.
    ImportDiskFull => "import_disk_full",
    /// The beets library database stayed locked past beets' own wait.
    ImportLibraryLocked => "import_library_locked",
    ImportIdleTimeout => "import_idle_timeout",
    ImportTotalTimeout => "import_total_timeout",
    ImporterBroken => "importer_broken",
    ImportAsIsMissingTags => "import_as_is_missing_tags",
    ImportChoiceInvalid => "import_choice_invalid",
    ImportFilesGone => "import_files_gone",
    IndexNotFoundAfterScan => "index_not_found_after_scan",
    IndexNoScanPossible => "index_no_scan_possible",
    ScanFailed => "scan_failed",
    PlaylistNotFound => "playlist_not_found",
    PlaylistNotOwner => "playlist_not_owner",
    PlaylistNameTaken => "playlist_name_taken",
    PlaylistTracksNotWritten => "playlist_tracks_not_written",
    /// Navidrome answered a playlist change it does not show: a name, comment, public flag or delete.
    PlaylistNotWritten => "playlist_not_written",
    /// A position given for a removal no longer holds the song given.
    PlaylistChanged => "playlist_changed",
    /// A smart or file-synced playlist, whose tracks Navidrome sets itself.
    PlaylistNotEditable => "playlist_not_editable",
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
    /// The job's state does not allow the change asked for.
    WrongJobState => "wrong_job_state",
    /// The job's search has no source left that it has not tried.
    SourcesExhausted => "sources_exhausted",
    /// The user rejected the result of a done job.
    ResultRejected => "result_rejected",
    /// A search Soulful stopped before it ran, because the job it served was cancelled.
    SearchStopped => "search_stopped",
    /// The track is already inside the target folder.
    MoveSameFolder => "move_same_folder",
    /// A file already exists at the destination path.
    MoveTargetExists => "move_target_exists",
    /// Another copy of this track already lives in the target folder.
    MoveDuplicateInTarget => "move_duplicate_in_target",
    /// The OS refused to move the file or finish cleaning up after it.
    MoveFailed => "move_failed",
    /// beets failed to modify tags or re-match the track.
    RetagFailed => "retag_failed",
    /// Relocating after re-tagging requires a linked Navidrome admin to scan between tag write and move.
    RetagRelocateNeedsScan => "retag_relocate_needs_scan",
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Design section 7 and 13 codes, by stage, with `settings_unreadable`,
    /// `backend_response_unreadable`, `search_query_rejected`,
    /// `search_already_running`, `search_lost`, `import_disk_full`,
    /// `import_library_locked`, `delete_failed`, `partial` (design 10.1), `wrong_job_state`,
    /// `sources_exhausted`, `result_rejected`, `search_stopped`,
    /// `no_listening_data`, `transfer_checksum_mismatch` and `download_disk_full`, and
    /// without `peer_rejected_enqueue`, which nothing produces. The one
    /// normative list of FailureCode names.
    const NAMES: [&str; 93] = [
        "invalid_input",
        "unknown_handle",
        "expired_handle",
        "permission_not_granted",
        "folder_required",
        "no_listening_data",
        "rate_limited",
        "metadata_unavailable",
        "metadata_not_found",
        "metadata_rate_limited",
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
        "transfer_checksum_mismatch",
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
        "download_disk_full",
        "import_no_match",
        "import_already_in_library",
        "import_nothing_to_import",
        "import_lookup_failed",
        "import_beets_failed",
        "import_target_unwritable",
        "import_disk_full",
        "import_library_locked",
        "import_idle_timeout",
        "import_total_timeout",
        "importer_broken",
        "import_as_is_missing_tags",
        "import_choice_invalid",
        "import_files_gone",
        "index_not_found_after_scan",
        "index_no_scan_possible",
        "scan_failed",
        "playlist_not_found",
        "playlist_not_owner",
        "playlist_name_taken",
        "playlist_tracks_not_written",
        "playlist_not_written",
        "playlist_changed",
        "playlist_not_editable",
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
        "wrong_job_state",
        "sources_exhausted",
        "result_rejected",
        "search_stopped",
        "move_same_folder",
        "move_target_exists",
        "move_duplicate_in_target",
        "move_failed",
        "retag_failed",
        "retag_relocate_needs_scan",
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
    fn the_user_docs_list_every_failure_code() {
        let docs =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/mcp.md"))
                .unwrap();
        let section = docs
            .split("\n## Error codes\n")
            .nth(1)
            .expect("docs/mcp.md has an Error codes section");
        let section = section.split("\n## ").next().unwrap();
        assert_eq!(
            section.lines().find(|l| l.starts_with('|')),
            Some("| Code | Stage | What it means | What to do |"),
            "the table starts with its header, whose first cell is not a code"
        );
        let listed: Vec<&str> = section
            .lines()
            .filter_map(|l| l.strip_prefix("| `"))
            .filter_map(|l| l.split('`').next())
            .collect();
        assert!(
            !listed.contains(&"code") && !listed.contains(&"Code"),
            "the header row is not read as a code"
        );
        let unique: std::collections::BTreeSet<&str> = listed.iter().copied().collect();
        assert_eq!(listed.len(), unique.len(), "a code is listed twice");
        let codes: std::collections::BTreeSet<&str> =
            FailureCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(unique, codes);
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
    fn the_internet_archive_is_written_internet_archive() {
        let f = Failure::new(
            FailureCode::TransferChecksumMismatch,
            Stage::Transfer,
            RetryAdvice::OtherSource,
            "The file does not match its md5",
        )
        .with_cause(Service::InternetArchive, Some(200), "md5 differs");
        assert_eq!(
            serde_json::to_value(&f).unwrap()["cause"],
            serde_json::json!({"service": "internet_archive", "status": 200, "raw": "md5 differs"})
        );
    }

    #[test]
    fn youtube_is_written_youtube() {
        let f = Failure::new(
            FailureCode::RateLimited,
            Stage::Transfer,
            RetryAdvice::SameSourceLater,
            "YouTube rate-limited this server",
        )
        .with_cause(Service::Youtube, None, "try again later");
        assert_eq!(
            serde_json::to_value(&f).unwrap()["cause"],
            serde_json::json!({"service": "youtube", "status": null, "raw": "try again later"})
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
            "Make the folder the download backend saves into readable by Soulbeet at /downloads"
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
    fn a_character_takes_in_json_what_serde_json_writes() {
        for c in (0..0x80u8).map(char::from).chain(['é', '日', '\u{2028}']) {
            let written = serde_json::to_string(&c.to_string()).unwrap();
            assert_eq!(
                json_chars(&c.to_string()),
                written.chars().count() - 2,
                "{c:?}"
            );
        }
    }

    #[test]
    fn a_text_cut_for_json_holds_its_bound_once_escaped() {
        for text in [
            "x".repeat(10_000),
            "\u{1}".repeat(10_000),
            format!("Silent Night{}", "\u{1}".repeat(488)),
            "\"\\".repeat(3_000),
            "é".repeat(700),
        ] {
            let cut = cut_json(text.clone(), TEXT_MAX_CHARS);
            let sent = serde_json::to_string(&cut).unwrap().chars().count() - 2;
            assert!(sent <= TEXT_MAX_CHARS, "{sent} for {:?}", &text[..12]);
            let (head, rest) = cut.split_once("\n[").unwrap();
            let (marker, tail) = rest.split_once("]\n").unwrap();
            let left_out = text.chars().count() - head.chars().count() - tail.chars().count();
            assert_eq!(marker, format!("{left_out} characters left out"));
            assert!(text.starts_with(head) && text.ends_with(tail));
        }
        let plain = "x".repeat(TEXT_MAX_CHARS);
        assert_eq!(cut_json(plain.clone(), TEXT_MAX_CHARS), plain);
        // 250 quotes take 500 characters once escaped, 251 take 502.
        assert_eq!(cut_json("\"".repeat(250), TEXT_MAX_CHARS), "\"".repeat(250));
        assert_ne!(cut_json("\"".repeat(251), TEXT_MAX_CHARS), "\"".repeat(251));
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
