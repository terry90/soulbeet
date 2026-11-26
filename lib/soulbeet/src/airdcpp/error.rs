//! What the Direct Connect backend's calls fail with, and the failures its transfers end with.

use crate::backend_error::{BackendFault, PeerAsk};
use crate::BookError;
use shared::failure::{
    Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
};

/// Where an admin sets AirDC++ up in Soulbeet.
const SETTINGS_AT: &str = "Settings > Config > Connections, Direct Connect";

/// A setting Soulbeet needs to reach the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Url,
    Username,
    Password,
    /// The stored password does not decrypt under Soulful's secret key.
    PasswordUnreadable,
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum AirdcppError {
    #[error("{}", missing_text(.0))]
    NotConfigured(Missing),

    /// The address is not an http or https URL Soulful can use.
    #[error("The AirDC++ address is not usable: {0}")]
    BadUrl(String),

    /// The sidecar did not answer: refused, timed out, or too many failures in a row.
    #[error("AirDC++ did not answer: {0}")]
    Unreachable(String),

    /// The sidecar refused the user name and password; nothing more is sent until the settings
    /// change.
    #[error("AirDC++ refused the user name and password")]
    Unauthorized,

    /// The web user lacks a permission a call needs.
    #[error("AirDC++ refused: {0}")]
    Forbidden(String),

    /// The sidecar speaks another version of its Web API.
    #[error("AirDC++ speaks another version of its Web API")]
    ApiVersion,

    /// Any other answer Soulful did not ask for.
    #[error("AirDC++ answered {status}: {message}")]
    Status { status: u16, message: String },

    /// A 2xx answer Soulbeet cannot read.
    #[error("AirDC++ sent an answer Soulbeet cannot read: {0}")]
    Unreadable(String),

    /// The sidecar holds more than 20 minutes of searches already.
    #[error("AirDC++ has too many searches queued")]
    SearchOverflow,

    #[error("AirDC++ is connected to no hub")]
    NoHub,

    /// The text cannot be searched as it is.
    #[error("{0}")]
    QueryRejected(String),

    /// A poll for a search the backend does not run.
    #[error("Search {search_id} is not known to the Direct Connect backend")]
    UnknownSearch { search_id: String },

    /// AirDC++ restarted during a search, and the search's id names another one now.
    #[error("AirDC++ restarted during the search")]
    Restarted,

    #[error("A search without an album takes exactly one track, not {tracks}")]
    NoSearchTarget { tracks: usize },

    /// A handle built before the backend was turned off.
    #[error("Direct Connect was turned off")]
    TurnedOff,

    /// The settings name AirDC++'s shipped login, which can share any folder of its machine.
    #[error("Soulbeet refuses AirDC++'s default admin login; create a web user of its own")]
    DefaultLogin,

    #[error("The AirDC++ user lacks these permissions: {}", .0.join(", "))]
    MissingPermissions(Vec<String>),

    /// The sidecar runs in another language, whose messages Soulbeet does not read.
    #[error("Soulbeet reads AirDC++'s English messages; set its language back to English")]
    Language,

    /// The folder the sidecar made did not appear in Soulbeet's downloads folder.
    #[error("AirDC++ saves to {sidecar}, which is not Soulbeet's DOWNLOAD_PATH ({ours})")]
    OtherFolder { sidecar: String, ours: String },

    /// Soulbeet cannot write in folders the sidecar makes.
    #[error(
        "Soulbeet cannot write in folders AirDC++ makes in {ours}; run AirDC++ with Soulbeet's \
         PUID and PGID: {detail}"
    )]
    FolderNotWritable { ours: String, detail: String },

    #[error("{0}")]
    Book(#[from] BookError),
}

fn missing_text(missing: &Missing) -> &'static str {
    match missing {
        Missing::Url => "The AirDC++ address is not set",
        Missing::Username => "The AirDC++ user name is not set",
        Missing::Password => "The AirDC++ password is not set",
        Missing::PasswordUnreadable => {
            "The stored AirDC++ password cannot be decrypted, probably because SECRET_KEY \
             changed; enter it again"
        }
    }
}

fn configure(what: impl Into<String>) -> UserAction {
    UserAction::new(UserActionCode::ConfigureBackend, what, SETTINGS_AT)
}

/// Whether a sidecar's error text says the disk is full or out of quota.
pub(crate) fn names_space(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("space") || text.contains("quota")
}

impl AirdcppError {
    /// This error as the failure of `stage`, shown as `message`.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        use FailureCode::*;
        use RetryAdvice::*;
        let (code, retry) = match self {
            AirdcppError::NotConfigured(_)
            | AirdcppError::BadUrl(_)
            | AirdcppError::Unauthorized
            | AirdcppError::Forbidden(_)
            | AirdcppError::ApiVersion
            | AirdcppError::TurnedOff
            | AirdcppError::DefaultLogin
            | AirdcppError::MissingPermissions(_)
            | AirdcppError::Language
            | AirdcppError::OtherFolder { .. }
            | AirdcppError::FolderNotWritable { .. } => (BackendNotConfigured, NeedsUser),
            AirdcppError::Unreachable(_) | AirdcppError::NoHub => {
                (BackendUnreachable, SameSourceLater)
            }
            AirdcppError::Status { .. } | AirdcppError::Unreadable(_) => {
                (BackendResponseUnreadable, SameSourceLater)
            }
            AirdcppError::SearchOverflow => (SearchRateLimited, SameSourceLater),
            AirdcppError::QueryRejected(_) => (SearchQueryRejected, OtherQuery),
            AirdcppError::UnknownSearch { .. } | AirdcppError::Restarted => {
                (SearchLost, SameSourceLater)
            }
            AirdcppError::NoSearchTarget { .. } => (InvalidInput, NotRetryable),
            AirdcppError::Book(_) => (TransferListUnreadable, SameSourceLater),
        };
        let failure = Failure::new(code, stage, retry, message);
        let failure = match self {
            AirdcppError::Status { status, .. } => {
                failure.with_cause(Service::Airdcpp, Some(i32::from(*status)), self.to_string())
            }
            AirdcppError::Unauthorized => {
                failure.with_cause(Service::Airdcpp, Some(401), self.to_string())
            }
            AirdcppError::Forbidden(_) => {
                failure.with_cause(Service::Airdcpp, Some(403), self.to_string())
            }
            AirdcppError::ApiVersion => {
                failure.with_cause(Service::Airdcpp, Some(412), self.to_string())
            }
            AirdcppError::Unreachable(_)
            | AirdcppError::Unreadable(_)
            | AirdcppError::SearchOverflow
            | AirdcppError::Restarted
            | AirdcppError::NoHub => failure.with_cause(Service::Airdcpp, None, self.to_string()),
            AirdcppError::Book(e) => failure.with_cause(Service::Database, None, e.0.clone()),
            AirdcppError::OtherFolder { .. } | AirdcppError::FolderNotWritable { .. } => {
                failure.with_cause(Service::Filesystem, None, self.to_string())
            }
            other => failure.with_cause(Service::Soulful, None, other.to_string()),
        };
        match self {
            AirdcppError::NotConfigured(_) | AirdcppError::BadUrl(_) => failure
                .with_user_action(configure("Set the AirDC++ address, user name and password")),
            AirdcppError::Unauthorized => {
                failure.with_user_action(configure("Check the AirDC++ user name and password"))
            }
            AirdcppError::Forbidden(text) => failure.with_user_action(configure(format!(
                "Give the AirDC++ web user the permission it lacks: {text}"
            ))),
            AirdcppError::MissingPermissions(missing) => failure.with_user_action(configure(
                format!("Give the AirDC++ web user {}", missing.join(", ")),
            )),
            AirdcppError::DefaultLogin => failure.with_user_action(configure(
                "Create an AirDC++ web user for Soulbeet and change the admin password",
            )),
            AirdcppError::Language => failure.with_user_action(UserAction::new(
                UserActionCode::ConfigureBackend,
                "Set AirDC++'s language to English",
                "AirDC++ Settings > Appearance",
            )),
            AirdcppError::ApiVersion => failure.with_user_action(UserAction::new(
                UserActionCode::UpdateSoulfulOrBackend,
                "Run the AirDC++ version Soulbeet supports (2.14), or update Soulbeet",
                "The AirDC++ image tag",
            )),
            AirdcppError::TurnedOff => failure.with_user_action(UserAction::new(
                UserActionCode::ConfigureBackend,
                format!("Turn on {}", shared::backends::AIRDCPP.name),
                "Settings > Config > Connections",
            )),
            AirdcppError::OtherFolder { .. } | AirdcppError::FolderNotWritable { .. } => failure
                .with_user_action(UserAction::new(
                    UserActionCode::FixDownloadPath,
                    "Mount Soulbeet's downloads folder as AirDC++'s download folder, and run \
                     AirDC++ with Soulbeet's PUID and PGID",
                    "The AirDC++ container's volumes and environment",
                )),
            _ => failure,
        }
    }
}

/// Soulbeet's words for the ask that failed.
fn ask_message(ask: PeerAsk) -> &'static str {
    match ask {
        PeerAsk::QueuePosition => "AirDC++ could not say this transfer's place in the queue",
        PeerAsk::Presence => "AirDC++ could not say whether the peer is online",
    }
}

impl BackendFault for AirdcppError {
    fn failure(&self, stage: Stage, message: String) -> Failure {
        AirdcppError::failure(self, stage, message)
    }

    fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        AirdcppError::failure(self, stage, ask_message(ask))
    }

    fn answer_unreadable(&self) -> bool {
        matches!(self, AirdcppError::Unreadable(_))
    }
}

/// How AirDC++ answered a file it was asked to queue, as the failure of that item.
pub(crate) fn enqueue_refused(status: u16, text: &str) -> Failure {
    let lower = text.to_lowercase();
    let (code, retry, message) = if lower.contains("user not found")
        || lower.contains("hub was not found")
        || lower.contains("offline")
    {
        (
            FailureCode::PeerOffline,
            RetryAdvice::OtherSource,
            "The peer is not online on its hub",
        )
    } else if lower.contains("skiplist")
        || lower.contains("shared")
        || lower.contains("already queued")
        || lower.contains("in the queue already")
    {
        (
            FailureCode::EnqueueFailed,
            RetryAdvice::NeedsUser,
            "AirDC++'s own settings refused this file",
        )
    } else {
        (
            FailureCode::EnqueueFailed,
            RetryAdvice::OtherSource,
            "AirDC++ refused to queue this file",
        )
    };
    let failure = Failure::new(code, Stage::Enqueue, retry, message).with_cause(
        Service::Airdcpp,
        Some(i32::from(status)),
        text,
    );
    match retry {
        RetryAdvice::NeedsUser => failure.with_user_action(UserAction::new(
            UserActionCode::ConfigureBackend,
            format!("Change the AirDC++ setting that refused it: {text}"),
            "AirDC++ Settings > Downloads",
        )),
        _ => failure,
    }
}

/// How a transfer ends when AirDC++ cannot finish it.
pub(crate) mod ended {
    use super::*;

    fn at_transfer(code: FailureCode, retry: RetryAdvice, message: &str, raw: &str) -> Failure {
        Failure::new(code, Stage::Transfer, retry, message).with_cause(Service::Airdcpp, None, raw)
    }

    /// The bundle stopped on a file error, `text` in AirDC++'s words: bytes that did not match
    /// the TTH, a full disk, or a folder it cannot write.
    pub(crate) fn download_error(text: &str) -> Failure {
        if text.to_lowercase().contains("tth") {
            return at_transfer(
                FailureCode::TransferChecksumMismatch,
                RetryAdvice::OtherSource,
                "AirDC++ threw away bytes that did not match the file's TTH",
                text,
            );
        }
        if names_space(text) {
            return at_transfer(
                FailureCode::DownloadDiskFull,
                RetryAdvice::NeedsUser,
                "The disk AirDC++ saves to is full",
                text,
            )
            .with_user_action(UserAction::new(
                UserActionCode::FreeDiskSpace,
                format!("Free disk space: {text}"),
                "The disk that holds the downloads folder",
            ));
        }
        at_transfer(
            FailureCode::TransferAborted,
            RetryAdvice::NeedsUser,
            "AirDC++ could not write the file",
            text,
        )
        .with_user_action(UserAction::new(
            UserActionCode::FixDownloadPath,
            format!("Let AirDC++ write in the downloads folder: {text}"),
            "The AirDC++ container's volumes and its PUID and PGID",
        ))
    }

    /// A completion hook of the sidecar refused the finished file.
    pub(crate) fn validation(text: &str) -> Failure {
        at_transfer(
            FailureCode::TransferRejectedOther,
            RetryAdvice::OtherSource,
            "AirDC++ refused the finished file",
            text,
        )
    }

    /// No byte came for `minutes` while AirDC++ only failed to connect, or every source was
    /// offline the whole time.
    pub(crate) fn gave_up(minutes: u64, offline: bool) -> Failure {
        match offline {
            true => at_transfer(
                FailureCode::PeerOffline,
                RetryAdvice::OtherSource,
                &format!("The peer was offline for {minutes} minutes"),
                "every source of the file offline",
            ),
            false => at_transfer(
                FailureCode::TransferConnectionFailed,
                RetryAdvice::OtherSource,
                &format!("AirDC++ could not connect to the peer for {minutes} minutes"),
                "no byte, only failed connections",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: AirdcppError) -> (FailureCode, RetryAdvice, Option<UserActionCode>) {
        let f = e.failure(Stage::SourceSearch, "m");
        (f.code, f.retry, f.user_action.map(|a| a.code))
    }

    #[test]
    fn each_answer_of_the_sidecar_has_its_code_and_action() {
        use FailureCode::*;
        use RetryAdvice::*;
        use UserActionCode::*;
        let cases = [
            (
                AirdcppError::Unauthorized,
                (BackendNotConfigured, NeedsUser, Some(ConfigureBackend)),
            ),
            (
                AirdcppError::Forbidden(
                    "The permission queue_edit is required for accessing this method".into(),
                ),
                (BackendNotConfigured, NeedsUser, Some(ConfigureBackend)),
            ),
            (
                AirdcppError::ApiVersion,
                (
                    BackendNotConfigured,
                    NeedsUser,
                    Some(UpdateSoulfulOrBackend),
                ),
            ),
            (
                AirdcppError::Unreachable("Connection refused".into()),
                (BackendUnreachable, SameSourceLater, None),
            ),
            (
                AirdcppError::DefaultLogin,
                (BackendNotConfigured, NeedsUser, Some(ConfigureBackend)),
            ),
            (
                AirdcppError::Language,
                (BackendNotConfigured, NeedsUser, Some(ConfigureBackend)),
            ),
            (
                AirdcppError::NoHub,
                (BackendUnreachable, SameSourceLater, None),
            ),
            (
                AirdcppError::OtherFolder {
                    sidecar: "/Downloads/".into(),
                    ours: "/downloads".into(),
                },
                (BackendNotConfigured, NeedsUser, Some(FixDownloadPath)),
            ),
            (
                AirdcppError::FolderNotWritable {
                    ours: "/downloads".into(),
                    detail: "Permission denied".into(),
                },
                (BackendNotConfigured, NeedsUser, Some(FixDownloadPath)),
            ),
            (
                AirdcppError::SearchOverflow,
                (SearchRateLimited, SameSourceLater, None),
            ),
            (
                AirdcppError::Status {
                    status: 500,
                    message: "x".into(),
                },
                (BackendResponseUnreadable, SameSourceLater, None),
            ),
            (
                AirdcppError::NotConfigured(Missing::Url),
                (BackendNotConfigured, NeedsUser, Some(ConfigureBackend)),
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(code(error.clone()), expected, "{error:?}");
        }
        assert!(AirdcppError::Unreadable("x".into()).answer_unreadable());
        assert!(!AirdcppError::Status {
            status: 500,
            message: "x".into()
        }
        .answer_unreadable());
    }

    #[test]
    fn a_file_error_says_whether_bytes_the_disk_or_the_folder_failed() {
        assert_eq!(
            ended::download_error("TTH inconsistency").code,
            FailureCode::TransferChecksumMismatch
        );
        let full = ended::download_error("No space left on device");
        assert_eq!(full.code, FailureCode::DownloadDiskFull);
        assert_eq!(
            full.user_action.map(|a| a.code),
            Some(UserActionCode::FreeDiskSpace)
        );
        let denied = ended::download_error("Permission denied");
        assert_eq!(denied.code, FailureCode::TransferAborted);
        assert_eq!(
            denied.user_action.map(|a| a.code),
            Some(UserActionCode::FixDownloadPath)
        );
    }

    #[test]
    fn a_refused_enqueue_names_why() {
        assert_eq!(
            enqueue_refused(400, "User not found").code,
            FailureCode::PeerOffline
        );
        let collision = enqueue_refused(400, "File exists on the disk already");
        assert_eq!(
            (collision.code, collision.retry),
            (FailureCode::EnqueueFailed, RetryAdvice::OtherSource)
        );
        let queued = enqueue_refused(
            400,
            "A file with a different size already exists in the queue",
        );
        assert_eq!(queued.retry, RetryAdvice::OtherSource);
        let skipped = enqueue_refused(400, "The file matches the download skiplist");
        assert_eq!(skipped.retry, RetryAdvice::NeedsUser);
    }
}
