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
/// bounded set: the fixed `"register"` key, ids of clients and grants that
/// exist, or user ids. A key a caller can choose freely would grow the map
/// without bound.
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
    /// Token requests naming a code or refresh token of the client, per
    /// client id.
    pub(crate) token_client: WindowLimiter,
    /// Refreshes, per grant id.
    pub(crate) token_grant: WindowLimiter,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            register: WindowLimiter::new(30, Duration::from_secs(3600)),
            cimd_fetch: WindowLimiter::new(10, Duration::from_secs(60)),
            token_client: WindowLimiter::new(30, Duration::from_secs(60)),
            token_grant: WindowLimiter::new(10, Duration::from_secs(60)),
        }
    }
}

/// `wait` in whole seconds, rounded up: the `Retry-After` value and the
/// number a refusal's text names.
pub(crate) fn retry_after_seconds(wait: Duration) -> u32 {
    debug_assert!(wait <= LONGEST_WINDOW, "no limit waits longer than an hour");
    u32::try_from(wait.as_millis().div_ceil(1000)).expect("a wait of at most an hour fits in u32")
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
}
