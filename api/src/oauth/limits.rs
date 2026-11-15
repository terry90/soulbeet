//! Fixed-window request budgets. None is keyed by a client address
//! (design 9.2, 14).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Past this many keys, `admit` drops the windows that have ended.
const PRUNE_ABOVE: usize = 1_000;

/// No limit in `Limits` has a longer window.
const LONGEST_WINDOW: Duration = Duration::from_secs(3600);

/// Admits up to `limit` hits per key in a fixed window, which opens at the
/// key's first hit after its last window ended.
///
/// A window still open is never dropped, so callers pass only keys from a
/// bounded set: the fixed `"register"` key, ids of grants that exist, or
/// user ids. A key a caller can choose freely would grow the map without
/// bound.
pub(crate) struct WindowLimiter {
    limit: u32,
    window: Duration,
    windows: Mutex<HashMap<String, Window>>,
}

struct Window {
    start: Instant,
    hits: u32,
}

impl WindowLimiter {
    pub(crate) fn new(limit: u32, window: Duration) -> WindowLimiter {
        WindowLimiter {
            limit,
            window,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// The hits a window admits.
    pub(crate) fn limit(&self) -> u32 {
        self.limit
    }

    /// The time left in `key`'s current window when it admits no more hits;
    /// counts nothing.
    pub(crate) fn wait(&self, key: &str, now: Instant) -> Option<Duration> {
        let windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        windows
            .get(key)
            .filter(|w| now < w.start + self.window && w.hits >= self.limit)
            .map(|w| w.start + self.window - now)
    }

    /// Counts one hit for `key` in the current fixed window; Err is the time
    /// left in it.
    pub(crate) fn admit(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        if windows.len() > PRUNE_ABOVE {
            windows.retain(|_, w| now < w.start + self.window);
        }
        match windows.get_mut(key) {
            Some(w) if now < w.start + self.window => {
                if w.hits >= self.limit {
                    return Err(w.start + self.window - now);
                }
                w.hits += 1;
            }
            _ => {
                windows.insert(
                    key.to_owned(),
                    Window {
                        start: now,
                        hits: 1,
                    },
                );
            }
        }
        Ok(())
    }
}

/// The server's request budgets (D4).
pub(crate) struct Limits {
    /// App registrations, one budget for the whole server.
    pub(crate) register: WindowLimiter,
    /// Client metadata document fetches, per signed-in user id.
    pub(crate) cimd_fetch: WindowLimiter,
    /// Token requests that issue tokens, per user id: the user of the code
    /// or refresh token. Not per client id, since every user of an install
    /// shares Claude's.
    pub(crate) token_user: WindowLimiter,
    /// Refreshes, per grant id.
    pub(crate) token_grant: WindowLimiter,
    /// MCP tool calls, per grant id.
    pub(crate) tool_calls: WindowLimiter,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            register: WindowLimiter::new(30, Duration::from_secs(3600)),
            cimd_fetch: WindowLimiter::new(10, Duration::from_secs(60)),
            token_user: WindowLimiter::new(30, Duration::from_secs(60)),
            token_grant: WindowLimiter::new(10, Duration::from_secs(60)),
            tool_calls: WindowLimiter::new(120, Duration::from_secs(60)),
        }
    }
}

/// `wait` in whole seconds, rounded up: the `Retry-After` value and the
/// number a refusal's text names.
pub(crate) fn retry_after_seconds(wait: Duration) -> u32 {
    debug_assert!(wait <= LONGEST_WINDOW, "no limit waits longer than an hour");
    u32::try_from(wait.as_millis().div_ceil(1000)).expect("a wait of at most an hour fits in u32")
}

/// "Try again in {n} seconds.", or "in 1 second." for one.
pub(crate) fn try_again_in(seconds: u32) -> String {
    match seconds {
        1 => "Try again in 1 second.".to_string(),
        n => format!("Try again in {n} seconds."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_admits_its_limit_then_says_how_long_to_wait() {
        let l = WindowLimiter::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(l.admit("a", t0), Ok(()));
        assert_eq!(l.admit("a", t0 + Duration::from_secs(1)), Ok(()));
        assert_eq!(
            l.admit("a", t0 + Duration::from_secs(20)),
            Err(Duration::from_secs(40))
        );
        assert_eq!(l.admit("b", t0 + Duration::from_secs(20)), Ok(()));
        assert_eq!(l.admit("a", t0 + Duration::from_secs(60)), Ok(()));
    }

    #[test]
    fn waiting_says_how_long_a_full_window_lasts_and_counts_nothing() {
        let l = WindowLimiter::new(1, Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(l.wait("a", t0), None);
        assert_eq!(l.admit("a", t0), Ok(()));
        for _ in 0..3 {
            assert_eq!(
                l.wait("a", t0 + Duration::from_secs(15)),
                Some(Duration::from_secs(45))
            );
        }
        assert_eq!(l.wait("a", t0 + Duration::from_secs(60)), None);
        assert_eq!(l.admit("a", t0 + Duration::from_secs(60)), Ok(()));
    }

    #[test]
    fn past_its_bound_the_map_drops_ended_windows_and_keeps_open_ones() {
        let l = WindowLimiter::new(1, Duration::from_secs(60));
        let t0 = Instant::now();
        for i in 0..=PRUNE_ABOVE {
            l.admit(&format!("old-{i}"), t0).unwrap();
        }
        let later = t0 + Duration::from_secs(30);
        l.admit("open", later).unwrap();
        // The next hit finds more than PRUNE_ABOVE keys, all old-* ended.
        let after = t0 + Duration::from_secs(61);
        l.admit("new", after).unwrap();
        let windows = l.windows.lock().unwrap();
        assert_eq!(
            {
                let mut keys: Vec<&str> = windows.keys().map(String::as_str).collect();
                keys.sort_unstable();
                keys
            },
            ["new", "open"]
        );
        drop(windows);
        assert_eq!(
            l.admit("open", after),
            Err(Duration::from_secs(29)),
            "an open window survives the prune with its hits"
        );
    }

    #[test]
    fn a_wait_is_rounded_up_to_whole_seconds() {
        for (wait, seconds) in [
            (Duration::ZERO, 0),
            (Duration::from_millis(1), 1),
            (Duration::from_millis(999), 1),
            (Duration::from_secs(1), 1),
            (Duration::from_millis(1001), 2),
            (Duration::from_millis(59_500), 60),
            (LONGEST_WINDOW, 3600),
        ] {
            assert_eq!(retry_after_seconds(wait), seconds, "{wait:?}");
        }
    }

    #[test]
    fn one_second_is_singular() {
        assert_eq!(try_again_in(1), "Try again in 1 second.");
        assert_eq!(try_again_in(2), "Try again in 2 seconds.");
        assert_eq!(try_again_in(0), "Try again in 0 seconds.");
    }
}
