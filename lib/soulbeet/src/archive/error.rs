//! What the Internet Archive backend's calls fail with, and the failures its transfers end with.

use crate::backend_error::{BackendFault, PeerAsk};
use crate::polite::RemoteError;
use crate::BookError;
use shared::failure::{
    Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
};
use std::io;
use std::path::{Path, PathBuf};

#[derive(thiserror::Error, Debug)]
pub enum ArchiveError {
    #[error("{0}")]
    Remote(#[from] RemoteError),

    /// A poll for a search the backend does not run: it never started it, or forgot it 10
    /// minutes after it ended.
    #[error("Search {search_id} is not known to the Internet Archive backend")]
    UnknownSearch { search_id: String },

    /// A search without an album names its target by exactly one track.
    #[error("A search without an album takes exactly one track, not {tracks}")]
    NoSearchTarget { tracks: usize },

    /// A handle built before the backend was turned off.
    #[error("Internet Archive was turned off")]
    TurnedOff,

    /// The folder of the partial files could not be opened or written.
    #[error("Cannot keep partial files under {}: {error}", download_path.display())]
    Storage {
        download_path: PathBuf,
        error: io::Error,
    },

    /// The transfer book could not be read or written.
    #[error("{0}")]
    Book(#[from] BookError),
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

impl ArchiveError {
    /// This error as the failure of `stage`, shown as `message`.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        use FailureCode::*;
        use RetryAdvice::*;
        let (code, retry) = match self {
            ArchiveError::Remote(RemoteError::Unavailable { .. }) => {
                (BackendUnreachable, SameSourceLater)
            }
            ArchiveError::Remote(RemoteError::RateLimited { .. })
                if stage == Stage::SourceSearch =>
            {
                (SearchRateLimited, SameSourceLater)
            }
            ArchiveError::Remote(RemoteError::RateLimited { .. }) => {
                (EnqueueFailed, SameSourceLater)
            }
            ArchiveError::Remote(RemoteError::Status { status, .. })
                if *status == 408 || *status >= 500 =>
            {
                (BackendUnreachable, SameSourceLater)
            }
            // archive.org refused the query as written.
            ArchiveError::Remote(RemoteError::Status { .. }) if stage == Stage::SourceSearch => {
                (SearchQueryRejected, OtherQuery)
            }
            ArchiveError::Remote(RemoteError::Status { .. }) => {
                (BackendUnreachable, SameSourceLater)
            }
            // A browser check passes with the outage it comes with.
            ArchiveError::Remote(
                RemoteError::Challenged { .. } | RemoteError::Unreadable { .. },
            ) => (BackendResponseUnreadable, SameSourceLater),
            ArchiveError::UnknownSearch { .. } => (SearchLost, SameSourceLater),
            ArchiveError::NoSearchTarget { .. } => (InvalidInput, NotRetryable),
            ArchiveError::TurnedOff => (BackendNotConfigured, NeedsUser),
            ArchiveError::Storage { error, .. } if is_disk_full(error) => {
                (DownloadDiskFull, NeedsUser)
            }
            ArchiveError::Storage { .. } => (BackendNotConfigured, NeedsUser),
            ArchiveError::Book(_) => (TransferListUnreadable, SameSourceLater),
        };
        let message = message.into();
        let failure = Failure::new(code, stage, retry, message.clone());
        let failure = match self {
            ArchiveError::Remote(e) => {
                let status = match e {
                    RemoteError::Status { status, .. } => Some(i32::from(*status)),
                    _ => None,
                };
                failure.with_cause(Service::InternetArchive, status, e.to_string())
            }
            ArchiveError::Storage { error, .. } => {
                failure.with_cause(Service::Filesystem, None, error.to_string())
            }
            ArchiveError::Book(e) => failure.with_cause(Service::Database, None, e.0.clone()),
            other => failure.with_cause(Service::Soulful, None, other.to_string()),
        };
        match self {
            ArchiveError::TurnedOff => failure.with_user_action(UserAction::new(
                UserActionCode::ConfigureBackend,
                format!("Turn on {}", shared::backends::ARCHIVE.name),
                SWITCHES_AT,
            )),
            ArchiveError::Storage { error, .. } if is_disk_full(error) => {
                failure.with_user_action(free_disk_space(&message))
            }
            // Another server holding the folder's lock is no folder problem.
            ArchiveError::Storage {
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
            "The Internet Archive backend could not say this transfer's place in its queue"
        }
        PeerAsk::Presence => "archive.org could not say whether the item is still there",
    }
}

impl BackendFault for ArchiveError {
    fn failure(&self, stage: Stage, message: String) -> Failure {
        ArchiveError::failure(self, stage, message)
    }

    fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        ArchiveError::failure(self, stage, ask_message(ask))
    }

    fn answer_unreadable(&self) -> bool {
        matches!(self, ArchiveError::Remote(RemoteError::Unreadable { .. }))
    }
}

/// How a transfer ends when it cannot be finished.
pub(crate) mod ended {
    use super::*;

    fn at_transfer(code: FailureCode, retry: RetryAdvice, message: &str) -> Failure {
        Failure::new(code, Stage::Transfer, retry, message)
    }

    /// archive.org answered the file's download with `status`.
    pub(crate) fn refused(status: u16) -> Failure {
        let (code, retry, message) = match status {
            403 => (
                FailureCode::TransferRejectedNotShared,
                RetryAdvice::OtherSource,
                "archive.org refused this file (restricted)",
            ),
            404 | 410 => (
                FailureCode::TransferRejectedNotShared,
                RetryAdvice::OtherSource,
                "archive.org no longer has this file",
            ),
            _ => (
                FailureCode::TransferRejectedOther,
                RetryAdvice::OtherSource,
                "archive.org refused to send this file",
            ),
        };
        at_transfer(code, retry, message).with_cause(
            Service::InternetArchive,
            Some(i32::from(status)),
            format!("HTTP {status}"),
        )
    }

    /// archive.org answered `status` (429 or 503) too many times, or asked for a wait longer
    /// than a transfer holds its slot.
    pub(crate) fn busy(status: u16, retry_after: Option<u64>) -> Failure {
        let failure = at_transfer(
            FailureCode::TransferRejectedPeerBusy,
            RetryAdvice::SameSourceLater,
            "archive.org is too busy to send this file now",
        )
        .with_cause(
            Service::InternetArchive,
            Some(i32::from(status)),
            format!("HTTP {status}"),
        );
        match retry_after.and_then(|s| u32::try_from(s).ok()) {
            Some(seconds) => failure.with_retry_after(seconds),
            None => failure,
        }
    }

    /// The connection kept failing: `last` is the last error, without its URL.
    pub(crate) fn connection(last: &str) -> Failure {
        at_transfer(
            FailureCode::TransferConnectionFailed,
            RetryAdvice::SameSourceLater,
            "The connection to archive.org kept failing",
        )
        .with_cause(Service::InternetArchive, None, last)
    }

    /// The bytes sent twice did not match the md5 archive.org lists.
    pub(crate) fn checksum(expected: &str, got: &str) -> Failure {
        at_transfer(
            FailureCode::TransferChecksumMismatch,
            RetryAdvice::OtherSource,
            "The file archive.org sent does not match its md5, twice",
        )
        .with_cause(
            Service::InternetArchive,
            None,
            format!("md5 {got}, archive.org lists {expected}"),
        )
    }

    /// archive.org sent more bytes than the file's size, or says it has another size now.
    pub(crate) fn size(expected: u64, got: u64) -> Failure {
        at_transfer(
            FailureCode::TransferSizeMismatch,
            RetryAdvice::OtherSource,
            "The file archive.org sent is not the size it lists, twice",
        )
        .with_cause(
            Service::InternetArchive,
            None,
            format!("{got} bytes, archive.org lists {expected}"),
        )
    }

    /// A restart found the transfer landed, and its file no longer in the batch folder: the
    /// import took it, or someone removed it. It is not fetched again.
    pub(crate) fn landed_gone() -> Failure {
        Failure::new(
            FailureCode::FileNotFoundAfterDownload,
            Stage::FileLookup,
            RetryAdvice::NotRetryable,
            "The file landed before Soulbeet restarted and is no longer in its folder",
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
            RetryAdvice::NeedsUser,
            "The downloaded file could not be written to the downloads folder",
        )
        .with_cause(Service::Filesystem, None, e.to_string())
        .with_user_action(UserAction::fix_download_path(download_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(e: RemoteError) -> ArchiveError {
        ArchiveError::Remote(e)
    }

    #[test]
    fn archive_errors_map_to_failure_codes_by_stage() {
        use FailureCode::*;
        use RetryAdvice::*;
        let service = "archive.org";
        let status = |status| {
            remote(RemoteError::Status {
                service,
                status,
                body: "<html>".into(),
            })
        };
        let cases = [
            (
                remote(RemoteError::Unavailable {
                    service,
                    detail: "HTTP 502".into(),
                }),
                Stage::SourceSearch,
                BackendUnreachable,
                SameSourceLater,
            ),
            (
                remote(RemoteError::RateLimited { service }),
                Stage::SourceSearch,
                SearchRateLimited,
                SameSourceLater,
            ),
            (
                remote(RemoteError::RateLimited { service }),
                Stage::Transfer,
                EnqueueFailed,
                SameSourceLater,
            ),
            (
                status(408),
                Stage::Transfer,
                BackendUnreachable,
                SameSourceLater,
            ),
            (
                status(400),
                Stage::SourceSearch,
                SearchQueryRejected,
                OtherQuery,
            ),
            (
                status(400),
                Stage::Transfer,
                BackendUnreachable,
                SameSourceLater,
            ),
            (
                remote(RemoteError::Challenged { service }),
                Stage::SourceSearch,
                BackendResponseUnreadable,
                SameSourceLater,
            ),
            (
                remote(RemoteError::Unreadable {
                    service,
                    detail: "expected value".into(),
                }),
                Stage::SourceSearch,
                BackendResponseUnreadable,
                SameSourceLater,
            ),
            (
                ArchiveError::UnknownSearch {
                    search_id: "s1".into(),
                },
                Stage::SourceSearch,
                SearchLost,
                SameSourceLater,
            ),
            (
                ArchiveError::NoSearchTarget { tracks: 2 },
                Stage::SourceSearch,
                InvalidInput,
                NotRetryable,
            ),
            (
                ArchiveError::TurnedOff,
                Stage::Enqueue,
                BackendNotConfigured,
                NeedsUser,
            ),
            (
                ArchiveError::Storage {
                    download_path: "/downloads".into(),
                    error: io::Error::from_raw_os_error(libc::ENOSPC),
                },
                Stage::SourceSearch,
                DownloadDiskFull,
                NeedsUser,
            ),
            (
                ArchiveError::Storage {
                    download_path: "/downloads".into(),
                    error: io::Error::from_raw_os_error(libc::EACCES),
                },
                Stage::SourceSearch,
                BackendNotConfigured,
                NeedsUser,
            ),
            (
                ArchiveError::Book(BookError("database is locked".into())),
                Stage::Transfer,
                TransferListUnreadable,
                SameSourceLater,
            ),
        ];
        for (e, stage, code, retry) in cases {
            let f = e.failure(stage, e.to_string());
            assert_eq!((f.code, f.retry, f.stage), (code, retry, stage), "{e:?}");
            assert_eq!(f.message, e.to_string());
            assert!(f.cause.is_some(), "{e:?}");
        }
        assert!(remote(RemoteError::Unreadable {
            service,
            detail: "x".into()
        })
        .answer_unreadable());
        assert!(!remote(RemoteError::Challenged { service }).answer_unreadable());
    }

    #[test]
    fn a_refused_query_keeps_archive_orgs_status_and_a_turned_off_handle_says_where_to_turn_it_on()
    {
        let e = remote(RemoteError::Status {
            service: "archive.org",
            status: 400,
            body: "bad query".into(),
        });
        let cause = e.failure(Stage::SourceSearch, "m").cause.unwrap();
        assert_eq!(
            (cause.service, cause.status),
            (Service::InternetArchive, Some(400))
        );
        let action = ArchiveError::TurnedOff
            .failure(Stage::Enqueue, "m")
            .user_action
            .unwrap();
        assert_eq!(
            (action.code, action.what.as_str(), action.where_.as_str()),
            (
                UserActionCode::ConfigureBackend,
                "Turn on Internet Archive",
                "Settings > Config > Connections"
            )
        );
    }

    #[test]
    fn a_folder_another_server_holds_asks_for_no_path_fix() {
        let held = ArchiveError::Storage {
            download_path: "/downloads".into(),
            error: io::Error::new(
                io::ErrorKind::WouldBlock,
                "Another Soulful server on this database and downloads folder holds /downloads/.soulful-partial/i/.lock",
            ),
        };
        let f = held.failure(Stage::SourceSearch, held.to_string());
        assert_eq!(f.user_action, None);
        assert!(
            f.message.contains("Another Soulful server"),
            "{}",
            f.message
        );
        let unwritable = ArchiveError::Storage {
            download_path: "/downloads".into(),
            error: io::Error::from_raw_os_error(libc::EACCES),
        };
        assert_eq!(
            unwritable
                .failure(Stage::SourceSearch, "m")
                .user_action
                .map(|a| a.code),
            Some(UserActionCode::FixDownloadPath)
        );
    }

    #[test]
    fn a_transfer_ends_with_the_code_its_cause_calls_for() {
        use FailureCode::*;
        let download_path = Path::new("/downloads");
        let cases = [
            (
                ended::refused(403),
                TransferRejectedNotShared,
                RetryAdvice::OtherSource,
            ),
            (
                ended::refused(404),
                TransferRejectedNotShared,
                RetryAdvice::OtherSource,
            ),
            (
                ended::refused(410),
                TransferRejectedNotShared,
                RetryAdvice::OtherSource,
            ),
            (
                ended::refused(451),
                TransferRejectedOther,
                RetryAdvice::OtherSource,
            ),
            (
                ended::busy(429, None),
                TransferRejectedPeerBusy,
                RetryAdvice::SameSourceLater,
            ),
            (
                ended::connection("connection closed before message completed"),
                TransferConnectionFailed,
                RetryAdvice::SameSourceLater,
            ),
            (
                ended::checksum("aa", "bb"),
                TransferChecksumMismatch,
                RetryAdvice::OtherSource,
            ),
            (
                ended::size(10, 12),
                TransferSizeMismatch,
                RetryAdvice::OtherSource,
            ),
            (
                ended::storage(&io::Error::from_raw_os_error(libc::ENOSPC), download_path),
                DownloadDiskFull,
                RetryAdvice::NeedsUser,
            ),
            (
                ended::storage(&io::Error::from_raw_os_error(libc::EDQUOT), download_path),
                DownloadDiskFull,
                RetryAdvice::NeedsUser,
            ),
            (
                ended::storage(&io::Error::from_raw_os_error(libc::EACCES), download_path),
                TransferAborted,
                RetryAdvice::NeedsUser,
            ),
        ];
        for (f, code, retry) in cases {
            assert_eq!(
                (f.code, f.retry, f.stage),
                (code, retry, Stage::Transfer),
                "{f:?}"
            );
        }
        assert_eq!(
            ended::refused(403).message,
            "archive.org refused this file (restricted)"
        );
        assert_eq!(ended::busy(503, Some(7200)).retry_after_seconds, Some(7200));
        let full = ended::storage(&io::Error::from_raw_os_error(libc::ENOSPC), download_path);
        assert_eq!(
            full.user_action.map(|a| (a.code, a.what)),
            Some((
                UserActionCode::FreeDiskSpace,
                "Free disk space: The disk of the downloads folder /downloads is full".into()
            ))
        );
    }
}
