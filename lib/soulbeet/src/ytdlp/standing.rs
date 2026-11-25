//! How YouTube treats this server now: a pause after a rate limit or a bot check, and what it
//! answered last. One for the process: YouTube limits the server's address, not a user, so every
//! user's searches and downloads share it.

use super::error::{Fault, Hold, Kind, Pause};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;
use tokio::time::Instant;

/// How long a rate limit holds every call: YouTube's guest limit lasts up to an hour.
pub(crate) const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(60 * 60);
/// How long a bot check holds every call, unless a probe passes first.
pub(crate) const BOT_CHECK_PAUSE: Duration = Duration::from_secs(6 * 60 * 60);
/// This many refused downloads in a row within an hour count as a bot check.
const FORBIDDEN_RUN: usize = 3;
const FORBIDDEN_WINDOW: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
struct State {
    /// The pause, and when it ends on the runtime's clock.
    pause: Option<(Pause, Instant)>,
    /// When YouTube last answered, and the failure it answered with.
    heard: Option<(Instant, Option<Fault>)>,
    /// The refused downloads since the last answer that was not one.
    forbidden: VecDeque<Instant>,
}

#[derive(Default)]
pub(crate) struct Standing {
    state: Mutex<State>,
    /// Notified when a pause starts.
    paused_now: Notify,
}

impl Standing {
    /// The pause in force, if one is.
    pub(crate) fn paused(&self) -> Option<Pause> {
        let mut state = self.state.lock().unwrap();
        match state.pause {
            Some((_, ends)) if ends <= Instant::now() => {
                state.pause = None;
                None
            }
            pause => pause.map(|(p, _)| p),
        }
    }

    /// Waits until a pause starts.
    pub(crate) async fn pause_started(&self) {
        self.paused_now.notified().await;
    }

    /// Records what a download or a probe came to: Ok, or the fault it failed with. A rate limit
    /// or a bot check pauses every call; three refused downloads within an hour, with no
    /// download answered in between, count as a bot check. Returns the pause this answer
    /// started, if it started one.
    pub(crate) fn heard_download(&self, outcome: Result<(), &Fault>) -> Option<Pause> {
        self.heard(outcome, true)
    }

    /// Records what a search came to. YouTube Music's API answers searches while its media
    /// servers refuse this server's downloads, so a search leaves the run of refused downloads
    /// as it is.
    pub(crate) fn heard_search(&self, outcome: Result<(), &Fault>) -> Option<Pause> {
        self.heard(outcome, false)
    }

    fn heard(&self, outcome: Result<(), &Fault>, download: bool) -> Option<Pause> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        // A track YouTube does not have, or has in no format asked for, is YouTube answering.
        let fault = outcome
            .err()
            .filter(|f| !matches!(f.kind, Kind::Unavailable | Kind::FormatMissing));
        state.heard = Some((now, fault.cloned()));
        let hold = match fault.map(|f| f.kind) {
            Some(Kind::RateLimited) => Some((Hold::RateLimited, RATE_LIMIT_PAUSE)),
            Some(Kind::BotCheck) => Some((Hold::BotCheck, BOT_CHECK_PAUSE)),
            _ if !download => None,
            Some(Kind::Forbidden) => {
                state.forbidden.push_back(now);
                while state
                    .forbidden
                    .front()
                    .is_some_and(|at| now.duration_since(*at) > FORBIDDEN_WINDOW)
                {
                    state.forbidden.pop_front();
                }
                (state.forbidden.len() >= FORBIDDEN_RUN)
                    .then_some((Hold::BotCheck, BOT_CHECK_PAUSE))
            }
            _ => {
                state.forbidden.clear();
                None
            }
        };
        let (why, length) = hold?;
        state.forbidden.clear();
        // A longer pause already in force stays.
        if state.pause.is_some_and(|(_, ends)| ends >= now + length) {
            return None;
        }
        let pause = Pause {
            why,
            until: SystemTime::now() + length,
        };
        state.pause = Some((pause, now + length));
        drop(state);
        self.paused_now.notify_waiters();
        Some(pause)
    }

    /// What YouTube answered within `recent`: Ok, or the failure it answered with.
    pub(crate) fn heard_within(&self, recent: Duration) -> Option<Result<(), Fault>> {
        let state = self.state.lock().unwrap();
        let (at, fault) = state.heard.as_ref()?;
        (at.elapsed() < recent).then(|| fault.clone().map_or(Ok(()), Err))
    }

    /// A probe passed: a bot-check pause is over.
    pub(crate) fn passed_probe(&self) {
        let mut state = self.state.lock().unwrap();
        if state.pause.is_some_and(|(p, _)| p.why == Hold::BotCheck) {
            state.pause = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fault(kind: Kind) -> Fault {
        Fault::new(kind, "x")
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_pauses_for_an_hour() {
        let standing = Standing::default();
        let pause = standing.heard_download(Err(&fault(Kind::RateLimited))).unwrap();
        assert_eq!(pause.why, Hold::RateLimited);
        assert_eq!(standing.paused(), Some(pause));
        tokio::time::advance(RATE_LIMIT_PAUSE - Duration::from_secs(1)).await;
        assert!(standing.paused().is_some());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(standing.paused(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn three_refusals_within_an_hour_are_a_bot_check() {
        let standing = Standing::default();
        assert!(standing.heard_download(Err(&fault(Kind::Forbidden))).is_none());
        tokio::time::advance(Duration::from_secs(61 * 60)).await;
        // The first is over an hour old.
        assert!(standing.heard_download(Err(&fault(Kind::Forbidden))).is_none());
        assert!(standing.heard_download(Err(&fault(Kind::Forbidden))).is_none());
        let pause = standing.heard_download(Err(&fault(Kind::Forbidden))).unwrap();
        assert_eq!(pause.why, Hold::BotCheck);

        // A download answered in between breaks the run.
        let standing = Standing::default();
        standing.heard_download(Err(&fault(Kind::Forbidden)));
        standing.heard_download(Err(&fault(Kind::Forbidden)));
        standing.heard_download(Ok(()));
        assert!(standing.heard_download(Err(&fault(Kind::Forbidden))).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_answered_between_refused_downloads_leaves_the_run() {
        let standing = Standing::default();
        standing.heard_download(Err(&fault(Kind::Forbidden)));
        assert!(standing.heard_search(Ok(())).is_none());
        standing.heard_download(Err(&fault(Kind::Forbidden)));
        assert!(standing.heard_search(Err(&fault(Kind::Forbidden))).is_none());
        let pause = standing.heard_download(Err(&fault(Kind::Forbidden))).unwrap();
        assert_eq!(pause.why, Hold::BotCheck);
        // A search still pauses everything on a rate limit.
        let standing = Standing::default();
        let pause = standing.heard_search(Err(&fault(Kind::RateLimited))).unwrap();
        assert_eq!(pause.why, Hold::RateLimited);
    }

    #[tokio::test(start_paused = true)]
    async fn a_passed_probe_lifts_a_bot_check_and_not_a_rate_limit() {
        let standing = Standing::default();
        standing.heard_download(Err(&fault(Kind::BotCheck)));
        standing.passed_probe();
        assert_eq!(standing.paused(), None);
        standing.heard_download(Err(&fault(Kind::RateLimited)));
        standing.passed_probe();
        assert!(standing.paused().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_track_is_youtube_answering() {
        let standing = Standing::default();
        standing.heard_download(Err(&fault(Kind::Unavailable)));
        assert_eq!(standing.heard_within(Duration::from_secs(60)), Some(Ok(())));
        standing.heard_download(Err(&fault(Kind::Network)));
        assert_eq!(
            standing.heard_within(Duration::from_secs(60)),
            Some(Err(fault(Kind::Network)))
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(standing.heard_within(Duration::from_secs(60)), None);
    }
}
