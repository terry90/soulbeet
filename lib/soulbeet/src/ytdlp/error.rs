//! What the YouTube Music backend's calls fail with, and the failures its transfers end with.

use crate::backend_error::{BackendFault, PeerAsk};
use regex::Regex;
use serde::Deserialize;
use shared::failure::{
    Failure, FailureCode, RetryAdvice, Service, Stage, UserAction, UserActionCode,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

/// Where an admin turns download backends on, as the registry's own text says.
const SWITCHES_AT: &str = "Settings > Config > Connections";

/// The longest helper message kept, in characters.
const MESSAGE_MAX_CHARS: usize = 500;

static RE_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://\S+").unwrap());

/// `text` with every URL replaced by `<url>`, trimmed and cut to `MESSAGE_MAX_CHARS`: a signed
/// media URL carries the server's IP address, which no log or failure may hold.
pub(crate) fn clean(text: &str) -> String {
    let cleaned = RE_URL.replace_all(text, "<url>");
    let trimmed = cleaned.trim();
    match trimmed.char_indices().nth(MESSAGE_MAX_CHARS) {
        Some((at, _)) => trimmed[..at].to_string(),
        None => trimmed.to_string(),
    }
}

/// What went wrong in a helper run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// "This content isn't available, try again later", HTTP 429.
    RateLimited,
    /// YouTube asks the server to sign in to prove it is no bot, or for a captcha.
    BotCheck,
    /// HTTP 403 on the media.
    Forbidden,
    /// The video is gone, private, not in this country, or asks for an age check.
    Unavailable,
    /// No Opus format of the bitrate asked for.
    FormatMissing,
    /// A timeout, a reset or cut connection.
    Network,
    /// yt-dlp, its JS runtime or ffmpeg is missing or broken.
    Tooling,
    /// Anything else the helper caught.
    Internal,
    /// The file's length is not the track's: found by Soulbeet, never sent by the helper.
    #[serde(skip_deserializing)]
    Truncated,
    /// The helper printed a line Soulbeet cannot read.
    #[serde(skip_deserializing)]
    Unreadable,
    /// The helper sent nothing for too long, or ran too long.
    #[serde(skip_deserializing)]
    TimedOut,
}

/// A helper run that failed: its kind and its message, cleaned of URLs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Fault {
    pub kind: Kind,
    pub message: String,
}

impl Fault {
    pub(crate) fn new(kind: Kind, message: impl AsRef<str>) -> Fault {
        Fault {
            kind,
            message: clean(message.as_ref()),
        }
    }
}

/// Why the backend takes no work from YouTube now, and until when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    RateLimited,
    BotCheck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pause {
    pub why: Hold,
    pub until: SystemTime,
}

impl Pause {
    /// Whole seconds from now until the pause ends, at least 1.
    pub(crate) fn seconds_left(&self) -> u32 {
        let left = self.until.duration_since(SystemTime::now());
        let left = left.unwrap_or(Duration::ZERO).as_secs().max(1);
        u32::try_from(left).unwrap_or(u32::MAX)
    }

    /// The fault a call this pause holds back fails with.
    pub(crate) fn fault(&self) -> Fault {
        let kind = match self.why {
            Hold::RateLimited => Kind::RateLimited,
            Hold::BotCheck => Kind::BotCheck,
        };
        Fault::new(kind, self.to_string())
    }

    fn until_text(&self) -> String {
        chrono::DateTime::<chrono::Utc>::from(self.until)
            .format("%H:%M UTC")
            .to_string()
    }
}

impl std::fmt::Display for Pause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.why {
            Hold::RateLimited => write!(
                f,
                "YouTube rate-limited this server until {}",
                self.until_text()
            ),
            Hold::BotCheck => write!(
                f,
                "YouTube asks this server to sign in to confirm it is not a bot; Soulbeet tries again at {}",
                self.until_text()
            ),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum YtDlpError {
    #[error("{}", .0.message)]
    Helper(Fault),

    /// A poll for a search the backend does not run: it never started it, or forgot it 30
    /// minutes after it ended.
    #[error("Search {search_id} is not known to the YouTube Music backend")]
    UnknownSearch { search_id: String },

    #[error("A search names no track")]
    NoSearchTarget,

    /// A handle built before the backend was turned off.
    #[error("YouTube Music was turned off")]
    TurnedOff,

    #[error("{0}")]
    Paused(Pause),

    /// The folder of the partial files could not be opened or written.
    #[error("Cannot keep partial files under {}: {error}", download_path.display())]
    Storage {
        download_path: PathBuf,
        error: io::Error,
    },
}

/// Whether `e` says the disk is full or the user's quota is spent.
fn is_disk_full(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT))
}

fn free_disk_space(message: &str) -> UserAction {
    UserAction::new(
        UserActionCode::FreeDiskSpace,
        format!("Free disk space: {message}"),
        "The disk that holds the path in the message",
    )
}

/// What a missing or broken tool asks of the admin.
pub(crate) fn use_the_full_image() -> UserAction {
    UserAction::new(
        UserActionCode::ConfigureBackend,
        "Run the :full image, which has yt-dlp, deno and ffmpeg",
        "The image tag Soulbeet runs from",
    )
}

/// The code and advice of a helper fault at `stage`.
fn code_of(kind: Kind, stage: Stage) -> (FailureCode, RetryAdvice) {
    use FailureCode::*;
    use RetryAdvice::*;
    let search = stage == Stage::SourceSearch;
    match kind {
        Kind::RateLimited if search => (SearchRateLimited, SameSourceLater),
        Kind::RateLimited => (RateLimited, SameSourceLater),
        Kind::BotCheck | Kind::Forbidden | Kind::Network if search => {
            (BackendUnreachable, SameSourceLater)
        }
        Kind::BotCheck => (TransferRejectedOther, OtherSource),
        Kind::Forbidden => (TransferRejectedOther, SameSourceLater),
        Kind::Unavailable => (TransferRejectedNotShared, OtherSource),
        Kind::FormatMissing => (TransferRejectedOther, OtherSource),
        Kind::Network => (TransferConnectionFailed, SameSourceLater),
        Kind::Truncated => (TransferSizeMismatch, SameSourceLater),
        Kind::Tooling => (BackendNotConfigured, NeedsUser),
        Kind::Internal | Kind::Unreadable => (BackendResponseUnreadable, SameSourceLater),
        Kind::TimedOut if search => (SearchTimeout, SameSourceLater),
        Kind::TimedOut => (TransferTimedOut, SameSourceLater),
    }
}

impl YtDlpError {
    /// This error as the failure of `stage`, shown as `message`.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        use FailureCode::*;
        use RetryAdvice::*;
        let message = message.into();
        match self {
            YtDlpError::Helper(fault) => {
                let (code, retry) = code_of(fault.kind, stage);
                let failure = Failure::new(code, stage, retry, message).with_cause(
                    Service::Youtube,
                    None,
                    fault.message.clone(),
                );
                match fault.kind {
                    Kind::Tooling => failure.with_user_action(use_the_full_image()),
                    _ => failure,
                }
            }
            YtDlpError::Paused(pause) => {
                let (code, retry) = code_of(pause.fault().kind, stage);
                let failure = Failure::new(code, stage, retry, message).with_cause(
                    Service::Youtube,
                    None,
                    pause.to_string(),
                );
                match pause.why {
                    Hold::RateLimited => failure.with_retry_after(pause.seconds_left()),
                    Hold::BotCheck => failure,
                }
            }
            YtDlpError::UnknownSearch { .. } => {
                Failure::new(SearchLost, stage, SameSourceLater, message).with_cause(
                    Service::Soulful,
                    None,
                    self.to_string(),
                )
            }
            YtDlpError::NoSearchTarget => Failure::new(InvalidInput, stage, NotRetryable, message)
                .with_cause(Service::Soulful, None, self.to_string()),
            YtDlpError::TurnedOff => Failure::new(BackendNotConfigured, stage, NeedsUser, message)
                .with_cause(Service::Soulful, None, self.to_string())
                .with_user_action(UserAction::new(
                    UserActionCode::ConfigureBackend,
                    format!("Turn on {}", shared::backends::YTDLP.name),
                    SWITCHES_AT,
                )),
            YtDlpError::Storage {
                download_path,
                error,
            } => {
                let full = is_disk_full(error);
                let code = if full {
                    DownloadDiskFull
                } else {
                    BackendNotConfigured
                };
                let failure = Failure::new(code, stage, NeedsUser, message.clone()).with_cause(
                    Service::Filesystem,
                    None,
                    error.to_string(),
                );
                if full {
                    failure.with_user_action(free_disk_space(&message))
                } else if error.kind() != io::ErrorKind::WouldBlock {
                    // Another server holding the folder's lock is no folder problem.
                    failure.with_user_action(UserAction::fix_download_path(download_path))
                } else {
                    failure
                }
            }
        }
    }
}

impl BackendFault for YtDlpError {
    fn failure(&self, stage: Stage, message: String) -> Failure {
        YtDlpError::failure(self, stage, message)
    }

    // Both are answered in this process, never by YouTube.
    fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        let message = match ask {
            PeerAsk::QueuePosition => {
                "The YouTube Music backend could not say this transfer's place in its queue"
            }
            PeerAsk::Presence => "The YouTube Music backend could not say whether it can fetch now",
        };
        Failure::new(
            FailureCode::BackendUnreachable,
            stage,
            RetryAdvice::SameSourceLater,
            message,
        )
        .with_cause(Service::Youtube, None, self.to_string())
    }

    fn answer_unreadable(&self) -> bool {
        matches!(self, YtDlpError::Helper(f) if f.kind == Kind::Unreadable)
    }
}

/// How a transfer ends when it cannot be finished.
pub(crate) mod ended {
    use super::*;

    /// The words a transfer that failed with `kind` is shown with.
    fn words(kind: Kind) -> &'static str {
        match kind {
            Kind::RateLimited => "YouTube rate-limited this server",
            Kind::BotCheck => "YouTube asks this server to sign in to confirm it is not a bot",
            Kind::Forbidden => "YouTube refused to send this file",
            Kind::Unavailable => "YouTube Music no longer has this track, or not in this country",
            Kind::FormatMissing => "YouTube offers this track in no Opus format of 96 kbps or more",
            Kind::Network => "The connection to YouTube kept failing",
            Kind::Truncated => "The file YouTube sent is not as long as the track",
            Kind::Tooling => "yt-dlp, deno or ffmpeg is missing or broken in this image",
            Kind::Internal => "yt-dlp could not download this track",
            Kind::Unreadable => "The YouTube helper answered with something Soulbeet cannot read",
            Kind::TimedOut => "The YouTube helper stopped answering",
        }
    }

    /// A transfer the helper failed with `fault`.
    pub(crate) fn helper(fault: &Fault) -> Failure {
        YtDlpError::Helper(fault.clone()).failure(Stage::Transfer, words(fault.kind))
    }

    /// A transfer of `pause`'s time, failed without asking YouTube.
    pub(crate) fn paused(pause: Pause) -> Failure {
        YtDlpError::Paused(pause).failure(Stage::Transfer, words(pause.fault().kind))
    }

    /// Tagging or landing the file under `download_path` failed with `e`.
    pub(crate) fn storage(e: &io::Error, download_path: &Path) -> Failure {
        if is_disk_full(e) {
            let message = format!(
                "The disk of the downloads folder {} is full",
                download_path.display()
            );
            return Failure::new(
                FailureCode::DownloadDiskFull,
                Stage::Transfer,
                RetryAdvice::NeedsUser,
                &message,
            )
            .with_cause(Service::Filesystem, None, e.to_string())
            .with_user_action(free_disk_space(&message));
        }
        Failure::new(
            FailureCode::TransferAborted,
            Stage::Transfer,
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

    #[test]
    fn a_message_keeps_no_url() {
        let message = "ERROR: unable to download video data: HTTP Error 403: Forbidden \
            (https://rr3---sn-4g5e6nsz.googlevideo.com/videoplayback?expire=1&ip=203.0.113.7&id=o-A)";
        let cleaned = clean(message);
        assert!(!cleaned.contains("googlevideo"), "{cleaned}");
        assert!(!cleaned.contains("203.0.113.7"), "{cleaned}");
        assert!(cleaned.ends_with("Forbidden (<url>"), "{cleaned}");
        assert_eq!(clean(&"é".repeat(600)).chars().count(), MESSAGE_MAX_CHARS);
    }

    #[test]
    fn each_kind_fails_a_transfer_as_the_design_table_says() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (kind, code, retry) in [
            (Kind::RateLimited, RateLimited, SameSourceLater),
            (Kind::BotCheck, TransferRejectedOther, OtherSource),
            (Kind::Forbidden, TransferRejectedOther, SameSourceLater),
            (Kind::Unavailable, TransferRejectedNotShared, OtherSource),
            (Kind::FormatMissing, TransferRejectedOther, OtherSource),
            (Kind::Network, TransferConnectionFailed, SameSourceLater),
            (Kind::Truncated, TransferSizeMismatch, SameSourceLater),
            (Kind::Tooling, BackendNotConfigured, NeedsUser),
            (Kind::Internal, BackendResponseUnreadable, SameSourceLater),
            (Kind::Unreadable, BackendResponseUnreadable, SameSourceLater),
            (Kind::TimedOut, TransferTimedOut, SameSourceLater),
        ] {
            let f = ended::helper(&Fault::new(kind, "raw"));
            assert_eq!(
                (f.code, f.retry, f.stage),
                (code, retry, Stage::Transfer),
                "{kind:?}"
            );
            let cause = f.cause.unwrap();
            assert_eq!(
                (cause.service, cause.raw.as_str()),
                (Service::Youtube, "raw")
            );
            assert_eq!(f.user_action.is_some(), kind == Kind::Tooling, "{kind:?}");
        }
    }

    #[test]
    fn a_search_names_a_rate_limit_a_timeout_and_an_unreachable_youtube_as_searches() {
        let at_search = |kind| {
            YtDlpError::Helper(Fault::new(kind, "x"))
                .failure(Stage::SourceSearch, "m")
                .code
        };
        assert_eq!(at_search(Kind::RateLimited), FailureCode::SearchRateLimited);
        assert_eq!(at_search(Kind::TimedOut), FailureCode::SearchTimeout);
        assert_eq!(at_search(Kind::BotCheck), FailureCode::BackendUnreachable);
        assert_eq!(at_search(Kind::Network), FailureCode::BackendUnreachable);
    }

    #[test]
    fn only_a_line_that_cannot_be_read_is_unreadable_for_good() {
        let unreadable = |kind| YtDlpError::Helper(Fault::new(kind, "x")).answer_unreadable();
        assert!(unreadable(Kind::Unreadable));
        assert!(!unreadable(Kind::Internal));
        assert!(!unreadable(Kind::Network));
    }

    #[test]
    fn a_rate_limit_pause_says_when_it_ends_and_how_long_to_wait() {
        let pause = Pause {
            why: Hold::RateLimited,
            until: SystemTime::now() + Duration::from_secs(3600),
        };
        let f = ended::paused(pause);
        assert_eq!(f.code, FailureCode::RateLimited);
        let wait = f.retry_after_seconds.unwrap();
        assert!((3590..=3600).contains(&wait), "{wait}");
        assert!(f
            .cause
            .unwrap()
            .raw
            .starts_with("YouTube rate-limited this server until "));
    }

    #[test]
    fn a_turned_off_handle_asks_to_turn_youtube_music_on() {
        let f = YtDlpError::TurnedOff.failure(Stage::Enqueue, "m");
        assert_eq!(f.code, FailureCode::BackendNotConfigured);
        assert_eq!(f.user_action.unwrap().what, "Turn on YouTube Music");
    }

    #[test]
    fn the_helper_cannot_claim_a_kind_only_soulbeet_finds() {
        for kind in ["truncated", "unreadable", "timed_out"] {
            let line = format!(r#"{{"kind": "{kind}", "message": "m"}}"#);
            assert!(serde_json::from_str::<Fault>(&line).is_err(), "{kind}");
        }
        let fault: Fault =
            serde_json::from_str(r#"{"kind": "bot_check", "message": "m"}"#).unwrap();
        assert_eq!(fault.kind, Kind::BotCheck);
    }
}
