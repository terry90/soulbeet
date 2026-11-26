//! What the GD Studio backend's calls fail with, and the failures its transfers end with.

use crate::backend_error::{BackendFault, PeerAsk};
use crate::polite::RemoteError;
use shared::failure::{
    Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(thiserror::Error, Debug, Clone)]
pub enum GdError {
    #[error("{0}")]
    Remote(#[from] RemoteError),

    /// The API failed this many calls in a row, so it is left alone for a while.
    #[error("GD Studio failed {failures} calls in a row and is left alone for {rest_s} s")]
    Resting { failures: u64, rest_s: i64 },

    /// A poll for a search the backend does not run: it never started it, or forgot it 30
    /// minutes after it ended.
    #[error("Search {search_id} is not known to the GD Studio backend")]
    UnknownSearch { search_id: String },

    /// A search names its target by an album and its tracks, or by exactly one track.
    #[error("A search without an album takes exactly one track, not {tracks}")]
    NoSearchTarget { tracks: usize },

    /// A handle built before the backend was turned off.
    #[error("NetEase (GD Studio) was turned off")]
    TurnedOff,

    /// The folder of the partial files could not be opened or written.
    #[error("Cannot keep partial files under {}: {error}", download_path.display())]
    Storage {
        download_path: PathBuf,
        error: Arc<io::Error>,
    },
}

/// Where an admin turns download backends on, as the registry's own text says.
const SWITCHES_AT: &str = "Settings > Config > Connections";

/// Whether `e` says the disk is full or the user's quota is spent.
pub(crate) fn is_disk_full(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT))
}

fn free_disk_space(message: &str) -> UserAction {
    UserAction::new(
        UserActionCode::FreeDiskSpace,
        format!("Free disk space: {message}"),
        "The disk that holds the path in the message",
    )
}

/// The API refused an unsigned request: GD Studio now wants what its own site sends.
fn signing_required(e: &RemoteError) -> bool {
    matches!(e, RemoteError::Status { status: 401, .. })
}

impl GdError {
    /// This error as the failure of `stage`, shown as `message`.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        use FailureCode::*;
        use RetryAdvice::*;
        let (code, retry) = match self {
            GdError::Remote(RemoteError::Unavailable { .. }) | GdError::Resting { .. } => {
                (BackendUnreachable, SameSourceLater)
            }
            GdError::Remote(RemoteError::RateLimited { .. }) if stage == Stage::SourceSearch => {
                (SearchRateLimited, SameSourceLater)
            }
            GdError::Remote(RemoteError::RateLimited { .. }) => (RateLimited, SameSourceLater),
            GdError::Remote(e) if signing_required(e) => (BackendResponseUnreadable, NotRetryable),
            GdError::Remote(RemoteError::Unreadable { .. }) => {
                (BackendResponseUnreadable, NotRetryable)
            }
            // A browser check passes with the outage it comes with.
            GdError::Remote(RemoteError::Challenged { .. }) => {
                (BackendResponseUnreadable, SameSourceLater)
            }
            GdError::Remote(RemoteError::Status { .. }) => (BackendUnreachable, SameSourceLater),
            GdError::UnknownSearch { .. } => (SearchLost, SameSourceLater),
            GdError::NoSearchTarget { .. } => (InvalidInput, NotRetryable),
            GdError::TurnedOff => (BackendNotConfigured, NeedsUser),
            GdError::Storage { error, .. } if is_disk_full(error) => (DownloadDiskFull, NeedsUser),
            GdError::Storage { .. } => (BackendNotConfigured, NeedsUser),
        };
        let message = message.into();
        let failure = Failure::new(code, stage, retry, message.clone());
        let failure = match self {
            GdError::Remote(e) => {
                let status = match e {
                    RemoteError::Status { status, .. } => Some(i32::from(*status)),
                    _ => None,
                };
                failure.with_cause(Service::GdStudio, status, e.to_string())
            }
            GdError::Storage { error, .. } => {
                failure.with_cause(Service::Filesystem, None, error.to_string())
            }
            other => failure.with_cause(Service::Soulful, None, other.to_string()),
        };
        match self {
            GdError::Remote(e) if signing_required(e) => failure.with_user_action(UserAction::new(
                UserActionCode::UpdateSoulfulOrBackend,
                "Update Soulbeet: GD Studio's API no longer answers unsigned requests",
                "Soulbeet",
            )),
            GdError::TurnedOff => failure.with_user_action(UserAction::new(
                UserActionCode::ConfigureBackend,
                format!("Turn on {}", shared::backends::GDSTUDIO.name),
                SWITCHES_AT,
            )),
            GdError::Storage { error, .. } if is_disk_full(error) => {
                failure.with_user_action(free_disk_space(&message))
            }
            // Another server holding the folder's lock is no folder problem.
            GdError::Storage {
                download_path,
                error,
            } if error.kind() != io::ErrorKind::WouldBlock => {
                failure.with_user_action(UserAction::fix_download_path(download_path))
            }
            _ => failure,
        }
    }
}

/// Soulful's words for the ask that failed.
fn ask_message(ask: PeerAsk) -> &'static str {
    match ask {
        PeerAsk::QueuePosition => {
            "The GD Studio backend could not say this transfer's place in its queue"
        }
        PeerAsk::Presence => "GD Studio could not say whether NetEase can be reached",
    }
}

impl BackendFault for GdError {
    fn failure(&self, stage: Stage, message: String) -> Failure {
        GdError::failure(self, stage, message)
    }

    fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        GdError::failure(self, stage, ask_message(ask))
    }

    /// An answer that does not parse, or a refusal of every unsigned request: neither reads
    /// later either.
    fn answer_unreadable(&self) -> bool {
        match self {
            GdError::Remote(e) => {
                signing_required(e) || matches!(e, RemoteError::Unreadable { .. })
            }
            _ => false,
        }
    }
}

/// How a transfer ends when it cannot be finished.
pub(crate) mod ended {
    use super::*;

    fn at_transfer(code: FailureCode, retry: RetryAdvice, message: &str) -> Failure {
        Failure::new(code, Stage::Transfer, retry, message)
    }

    fn cause(failure: Failure, raw: impl Into<String>) -> Failure {
        failure.with_cause(Service::GdStudio, None, raw)
    }

    /// GD Studio answered an empty URL for the track, or NetEase's server no longer has it.
    pub(crate) fn not_served(raw: &str) -> Failure {
        cause(
            at_transfer(
                FailureCode::TransferRejectedNotShared,
                RetryAdvice::OtherSource,
                "NetEase no longer serves this track",
            ),
            raw,
        )
    }

    /// The track now resolves to another file than the one the search found.
    pub(crate) fn changed(raw: String) -> Failure {
        cause(
            at_transfer(
                FailureCode::SourceFileChanged,
                RetryAdvice::OtherQuery,
                "NetEase now serves another file for this track; search again to see what it serves",
            ),
            raw,
        )
    }

    /// GD Studio named a file on a server that is not NetEase's.
    pub(crate) fn foreign_host(host: &str) -> Failure {
        cause(
            at_transfer(
                FailureCode::BackendResponseUnreadable,
                RetryAdvice::NotRetryable,
                "GD Studio named a file outside NetEase's servers, so it was not fetched",
            ),
            format!("host {host}"),
        )
    }

    /// NetEase's server refused the file at the start of a request twice, a fresh URL between.
    pub(crate) fn refused(status: u16) -> Failure {
        at_transfer(
            FailureCode::TransferConnectionFailed,
            RetryAdvice::SameSourceLater,
            "NetEase's server refused the file twice",
        )
        .with_cause(
            Service::GdStudio,
            Some(i32::from(status)),
            format!("HTTP {status}"),
        )
    }

    /// The bytes were not `expected` long.
    pub(crate) fn size(expected: u64, got: u64) -> Failure {
        cause(
            at_transfer(
                FailureCode::TransferSizeMismatch,
                RetryAdvice::SameSourceLater,
                "The file NetEase sent is not the size GD Studio gave",
            ),
            format!("{got} bytes, GD Studio gave {expected}"),
        )
    }

    /// The bytes' MD5 is not the one the file is named by.
    pub(crate) fn checksum(expected: &str, got: &str) -> Failure {
        cause(
            at_transfer(
                FailureCode::TransferChecksumMismatch,
                RetryAdvice::SameSourceLater,
                "The file NetEase sent does not match the MD5 it is named by",
            ),
            format!("md5 {got}, named {expected}"),
        )
    }

    /// The bytes are not the audio the file's name says.
    pub(crate) fn not_audio(ext: &str, why: &str) -> Failure {
        cause(
            at_transfer(
                FailureCode::TransferRejectedOther,
                RetryAdvice::OtherSource,
                &format!("The file NetEase sent is not the {ext} its name says"),
            ),
            why,
        )
    }

    /// The answer stalled for the read timeout on every attempt.
    pub(crate) fn timed_out(last: &str) -> Failure {
        cause(
            at_transfer(
                FailureCode::TransferTimedOut,
                RetryAdvice::SameSourceLater,
                "NetEase's server stopped sending the file, on every attempt",
            ),
            last,
        )
    }

    /// The connection kept failing: `last` is the last error, without its URL.
    pub(crate) fn connection(last: &str) -> Failure {
        cause(
            at_transfer(
                FailureCode::TransferConnectionFailed,
                RetryAdvice::SameSourceLater,
                "The connection to NetEase's server kept failing",
            ),
            last,
        )
    }

    /// The tags could not be read from the bytes, which are then no audio file lofty knows.
    pub(crate) fn untaggable(raw: String) -> Failure {
        cause(
            at_transfer(
                FailureCode::TransferRejectedOther,
                RetryAdvice::OtherSource,
                "The file NetEase sent could not be tagged",
            ),
            raw,
        )
    }

    /// Writing or landing the file under `download_path` failed with `e`.
    pub(crate) fn storage(e: &io::Error, download_path: &Path) -> Failure {
        if is_disk_full(e) {
            let message = format!(
                "The disk of the downloads folder {} is full",
                download_path.display()
            );
            return at_transfer(
                FailureCode::DownloadDiskFull,
                RetryAdvice::NeedsUser,
                &message,
            )
            .with_cause(Service::Filesystem, None, e.to_string())
            .with_user_action(free_disk_space(&message));
        }
        at_transfer(
            FailureCode::TransferAborted,
            RetryAdvice::NotRetryable,
            "The downloaded file could not be written to the downloads folder",
        )
        .with_cause(Service::Filesystem, None, e.to_string())
        .with_user_action(UserAction::fix_download_path(download_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE: &str = "GD Studio";

    fn remote(e: RemoteError) -> GdError {
        GdError::Remote(e)
    }

    fn status(status: u16, body: &str) -> GdError {
        remote(RemoteError::Status {
            service: SERVICE,
            status,
            body: body.into(),
        })
    }

    #[test]
    fn gd_errors_map_to_failure_codes_by_stage() {
        use FailureCode::*;
        use RetryAdvice::*;
        let cases = [
            (
                remote(RemoteError::Unavailable {
                    service: SERVICE,
                    detail: "HTTP 522".into(),
                }),
                Stage::SourceSearch,
                BackendUnreachable,
                SameSourceLater,
            ),
            (
                GdError::Resting {
                    failures: 5,
                    rest_s: 60,
                },
                Stage::Transfer,
                BackendUnreachable,
                SameSourceLater,
            ),
            (
                remote(RemoteError::RateLimited { service: SERVICE }),
                Stage::SourceSearch,
                SearchRateLimited,
                SameSourceLater,
            ),
            (
                remote(RemoteError::RateLimited { service: SERVICE }),
                Stage::Transfer,
                RateLimited,
                SameSourceLater,
            ),
            (
                status(401, r#"{"detail":"Invalid request."}"#),
                Stage::SourceSearch,
                BackendResponseUnreadable,
                NotRetryable,
            ),
            (
                remote(RemoteError::Unreadable {
                    service: SERVICE,
                    detail: "missing field `url`".into(),
                }),
                Stage::SourceSearch,
                BackendResponseUnreadable,
                NotRetryable,
            ),
            (
                remote(RemoteError::Challenged { service: SERVICE }),
                Stage::SourceSearch,
                BackendResponseUnreadable,
                SameSourceLater,
            ),
            (
                status(403, "blocked"),
                Stage::SourceSearch,
                BackendUnreachable,
                SameSourceLater,
            ),
            (
                GdError::UnknownSearch {
                    search_id: "s1".into(),
                },
                Stage::SourceSearch,
                SearchLost,
                SameSourceLater,
            ),
            (
                GdError::NoSearchTarget { tracks: 2 },
                Stage::SourceSearch,
                InvalidInput,
                NotRetryable,
            ),
            (
                GdError::TurnedOff,
                Stage::Enqueue,
                BackendNotConfigured,
                NeedsUser,
            ),
            (
                GdError::Storage {
                    download_path: "/downloads".into(),
                    error: Arc::new(io::Error::from_raw_os_error(libc::ENOSPC)),
                },
                Stage::SourceSearch,
                DownloadDiskFull,
                NeedsUser,
            ),
            (
                GdError::Storage {
                    download_path: "/downloads".into(),
                    error: Arc::new(io::Error::from_raw_os_error(libc::EACCES)),
                },
                Stage::SourceSearch,
                BackendNotConfigured,
                NeedsUser,
            ),
        ];
        for (e, stage, code, retry) in cases {
            let f = e.failure(stage, e.to_string());
            assert_eq!((f.code, f.retry, f.stage), (code, retry, stage), "{e:?}");
            assert!(f.cause.is_some(), "{e:?}");
        }
    }

    #[test]
    fn only_an_unreadable_answer_or_a_refused_unsigned_request_reads_as_unreadable() {
        let unreadable = remote(RemoteError::Unreadable {
            service: SERVICE,
            detail: "x".into(),
        });
        let unsigned = status(401, r#"{"detail":"Invalid request."}"#);
        assert!(unreadable.answer_unreadable());
        assert!(unsigned.answer_unreadable());
        for other in [
            remote(RemoteError::Challenged { service: SERVICE }),
            remote(RemoteError::RateLimited { service: SERVICE }),
            status(500, "x"),
            GdError::TurnedOff,
        ] {
            assert!(!other.answer_unreadable(), "{other:?}");
        }
        let f = unsigned.failure(Stage::SourceSearch, "m");
        let cause = f.cause.unwrap();
        assert_eq!(
            (cause.service, cause.status),
            (Service::GdStudio, Some(401))
        );
        assert_eq!(
            f.user_action.map(|a| a.code),
            Some(UserActionCode::UpdateSoulfulOrBackend)
        );
    }

    #[test]
    fn a_turned_off_handle_says_where_to_turn_it_on() {
        let action = GdError::TurnedOff
            .failure(Stage::Enqueue, "m")
            .user_action
            .unwrap();
        assert_eq!(
            (action.code, action.what.as_str(), action.where_.as_str()),
            (
                UserActionCode::ConfigureBackend,
                "Turn on NetEase (GD Studio)",
                "Settings > Config > Connections"
            )
        );
        let held = GdError::Storage {
            download_path: "/downloads".into(),
            error: Arc::new(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Another Soulful server",
            )),
        };
        assert_eq!(held.failure(Stage::SourceSearch, "m").user_action, None);
    }

    #[test]
    fn a_transfer_ends_with_the_code_its_cause_calls_for() {
        use FailureCode::*;
        use RetryAdvice::*;
        let download_path = Path::new("/downloads");
        let cases = [
            (
                ended::not_served("empty url"),
                TransferRejectedNotShared,
                OtherSource,
            ),
            (ended::changed("md5".into()), SourceFileChanged, OtherQuery),
            (
                ended::foreign_host("evil.example"),
                BackendResponseUnreadable,
                NotRetryable,
            ),
            (
                ended::refused(403),
                TransferConnectionFailed,
                SameSourceLater,
            ),
            (ended::size(10, 12), TransferSizeMismatch, SameSourceLater),
            (
                ended::checksum("aa", "bb"),
                TransferChecksumMismatch,
                SameSourceLater,
            ),
            (
                ended::not_audio("flac", "ID3"),
                TransferRejectedOther,
                OtherSource,
            ),
            (
                ended::timed_out("stalled"),
                TransferTimedOut,
                SameSourceLater,
            ),
            (
                ended::connection("reset"),
                TransferConnectionFailed,
                SameSourceLater,
            ),
            (
                ended::storage(&io::Error::from_raw_os_error(libc::ENOSPC), download_path),
                DownloadDiskFull,
                NeedsUser,
            ),
            (
                ended::storage(&io::Error::from_raw_os_error(libc::EXDEV), download_path),
                TransferAborted,
                NotRetryable,
            ),
        ];
        for (f, code, retry) in cases {
            assert_eq!(
                (f.code, f.retry, f.stage),
                (code, retry, Stage::Transfer),
                "{f:?}"
            );
            assert!(f.cause.is_some(), "{f:?}");
        }
        let exdev = ended::storage(&io::Error::from_raw_os_error(libc::EXDEV), download_path);
        assert_eq!(
            exdev.user_action.map(|a| a.code),
            Some(UserActionCode::FixDownloadPath)
        );
    }
}
