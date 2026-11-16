//! Slows repeated failed sign-ins: a growing wait per account and a failure
//! budget for the whole server. Nothing depends on the client address.
//!
//! An attempt is admitted and counted in one step, and counts as a pending
//! failure until it settles, so parallel requests cannot all slip through
//! before the first failure is recorded.

use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::oauth::limits::{retry_after_seconds, try_again_in};

/// Consecutive failures an account may make before waits start.
pub const FREE_FAILURES: u32 = 3;
/// Longest wait after a failure.
pub const MAX_DELAY: Duration = Duration::from_secs(900);
/// An account with no failure for this long is forgotten.
pub const FAILURE_MEMORY: Duration = Duration::from_secs(900);
/// Failures the server accepts per fixed 60 second window.
pub const SERVER_FAILURES_PER_MINUTE: u32 = 60;

const SERVER_WINDOW: Duration = Duration::from_secs(60);

/// The shortest wait offered while another attempt on the account is still
/// being checked: that attempt settles within a request's time.
const IN_FLIGHT_RETRY: Duration = Duration::from_secs(1);

// A forgotten account must never still be waiting out a delay.
const _: () = assert!(MAX_DELAY.as_nanos() <= FAILURE_MEMORY.as_nanos());

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoginBlock {
    Account { retry_after: Duration },
    Server { retry_after: Duration },
}

impl fmt::Display for LoginBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (what, retry_after) = match self {
            LoginBlock::Account { retry_after } => ("for this account", retry_after),
            LoginBlock::Server { retry_after } => ("on this server", retry_after),
        };
        write!(
            f,
            "Too many failed sign-ins {what}. {}",
            try_again_in(retry_after_seconds(*retry_after))
        )
    }
}

struct Account {
    failures: u32,
    last_failure: Instant,
    in_flight: u32,
}

impl Account {
    fn forgotten(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_failure) >= FAILURE_MEMORY
    }
}

#[derive(Default)]
struct State {
    accounts: HashMap<String, Account>,
    window_start: Option<Instant>,
    window_failures: u32,
    in_flight: u32,
    server_block_logged: bool,
}

impl State {
    /// The account's entry, with failures older than `FAILURE_MEMORY` dropped.
    fn account(&mut self, key: &str, now: Instant) -> &mut Account {
        let account = self.accounts.entry(key.to_owned()).or_insert(Account {
            failures: 0,
            last_failure: now,
            in_flight: 0,
        });
        if account.forgotten(now) {
            account.failures = 0;
        }
        account
    }

    /// Failures recorded in the window that is open at `now`.
    fn window_failures_at(&self, now: Instant) -> u32 {
        match self.window_start {
            Some(start) if now < start + SERVER_WINDOW => self.window_failures,
            _ => 0,
        }
    }

    fn server_block(&self, now: Instant) -> Option<LoginBlock> {
        if self.window_failures_at(now) + self.in_flight < SERVER_FAILURES_PER_MINUTE {
            return None;
        }
        // Inside an open window the budget frees up when it ends. Otherwise
        // attempts in flight fill it, and their failures open a new window.
        let retry_after = match self.window_start {
            Some(start) if now < start + SERVER_WINDOW => start + SERVER_WINDOW - now,
            _ => SERVER_WINDOW,
        };
        Some(LoginBlock::Server { retry_after })
    }

    fn admit(&mut self, key: &str, now: Instant) -> Result<(), LoginBlock> {
        if let Some(block) = self.server_block(now) {
            if !self.server_block_logged {
                self.server_block_logged = true;
                tracing::warn!("Too many failed sign-ins on this server, refusing all sign-ins");
            }
            return Err(block);
        }
        let account = self.account(key, now);
        let pending = account.failures.saturating_add(account.in_flight);
        if pending >= FREE_FAILURES {
            if account.in_flight > 0 {
                return Err(LoginBlock::Account {
                    retry_after: delay_after(pending).max(IN_FLIGHT_RETRY),
                });
            }
            let ready = account.last_failure + delay_after(account.failures);
            if now < ready {
                return Err(LoginBlock::Account {
                    retry_after: ready - now,
                });
            }
        }
        account.in_flight += 1;
        self.in_flight += 1;
        self.server_block_logged = false;
        Ok(())
    }

    fn record_failure(&mut self, key: &str, now: Instant) {
        self.accounts
            .retain(|_, a| a.in_flight > 0 || !a.forgotten(now));

        let account = self.account(key, now);
        account.failures = account.failures.saturating_add(1);
        account.last_failure = now;

        match self.window_start {
            Some(start) if now < start + SERVER_WINDOW => self.window_failures += 1,
            _ => {
                self.window_start = Some(now);
                self.window_failures = 1;
            }
        }
    }

    fn clear(&mut self, key: &str) {
        if let Some(account) = self.accounts.get_mut(key) {
            account.failures = 0;
        }
    }

    /// Frees the slot an attempt held. An entry with nothing left to remember
    /// is removed.
    fn release(&mut self, key: &str) {
        self.in_flight -= 1;
        if let Some(account) = self.accounts.get_mut(key) {
            account.in_flight -= 1;
            if account.in_flight == 0 && account.failures == 0 {
                self.accounts.remove(key);
            }
        }
    }
}

#[derive(Default)]
pub struct LoginLimiter {
    state: Mutex<State>,
}

fn key(username: &str) -> String {
    username.trim().to_lowercase()
}

fn delay_after(failures: u32) -> Duration {
    if failures <= FREE_FAILURES {
        return Duration::ZERO;
    }
    // 2^10 s already exceeds the cap, so larger exponents are not computed.
    let exponent = (failures - FREE_FAILURES).min(10);
    Duration::from_secs(1u64 << exponent).min(MAX_DELAY)
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admits a sign-in attempt and holds its slot until it settles.
    pub fn begin(&self, username: &str, now: Instant) -> Result<LoginAttempt<'_>, LoginBlock> {
        let key = key(username);
        self.lock().admit(&key, now)?;
        Ok(LoginAttempt {
            limiter: self,
            key: Some(key),
        })
    }
}

/// An admitted sign-in attempt. It counts as a failure for the account and
/// the server budget until it settles; dropping it unsettled frees the slot
/// and records nothing.
#[must_use = "an attempt dropped unsettled records nothing"]
pub struct LoginAttempt<'a> {
    limiter: &'a LoginLimiter,
    /// `None` once settled.
    key: Option<String>,
}

impl LoginAttempt<'_> {
    /// The credentials did not match.
    pub fn failed(mut self, now: Instant) {
        self.settle(|state, key| state.record_failure(key, now));
    }

    /// The credentials matched: the account's failures are cleared.
    pub fn succeeded(mut self) {
        self.settle(State::clear);
    }

    fn settle(&mut self, record: impl FnOnce(&mut State, &str)) {
        if let Some(key) = self.key.take() {
            let mut state = self.limiter.lock();
            record(&mut state, &key);
            state.release(&key);
        }
    }
}

impl Drop for LoginAttempt<'_> {
    fn drop(&mut self) {
        self.settle(|_, _| {});
    }
}

pub static LOGIN_LIMITER: LazyLock<LoginLimiter> = LazyLock::new(LoginLimiter::new);

#[cfg(test)]
mod tests {
    use super::*;

    /// Records settled failures directly, as if each attempt had been admitted
    /// earlier, so a test can start from any history.
    fn fail(limiter: &LoginLimiter, name: &str, times: u32, at: Instant) {
        for _ in 0..times {
            limiter.lock().record_failure(&key(name), at);
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn three_failures_cost_nothing() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", 3, t0);
        assert!(l.begin("bob", t0).is_ok());
    }

    #[test]
    fn the_fourth_failure_waits_two_seconds() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", 4, t0);
        assert_eq!(
            l.begin("bob", t0 + secs(1)).err(),
            Some(LoginBlock::Account {
                retry_after: secs(1)
            })
        );
        assert!(l.begin("bob", t0 + secs(2)).is_ok());
    }

    #[test]
    fn the_delay_doubles_and_stops_at_fifteen_minutes() {
        let t0 = Instant::now();
        for (failures, wait) in [(5, 4), (13, 900), (20, 900)] {
            let l = LoginLimiter::new();
            fail(&l, "bob", failures, t0);
            assert_eq!(
                l.begin("bob", t0).err(),
                Some(LoginBlock::Account {
                    retry_after: secs(wait)
                }),
                "{failures} failures"
            );
            assert!(l.begin("bob", t0 + secs(wait)).is_ok());
        }
    }

    #[test]
    fn a_success_resets_the_count() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", 6, t0);
        let later = t0 + secs(8);
        l.begin("bob", later).unwrap().succeeded();
        fail(&l, "bob", 1, later);
        assert!(l.begin("bob", later).is_ok());
    }

    #[test]
    fn usernames_are_compared_without_case() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, " Bob", 4, t0);
        assert!(l.begin("bob", t0).is_err());
    }

    #[test]
    fn a_quiet_account_is_forgotten() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", 10, t0);
        let later = t0 + secs(901);
        l.begin("bob", later).unwrap().failed(later);
        assert!(l.begin("bob", later).is_ok());
    }

    #[test]
    fn the_server_budget_blocks_everyone_for_the_rest_of_the_minute() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        for i in 0..SERVER_FAILURES_PER_MINUTE {
            fail(
                &l,
                &format!("user{i}"),
                1,
                t0 + Duration::from_millis(u64::from(i) * 500),
            );
        }
        assert_eq!(
            l.begin("fresh", t0 + secs(30)).err(),
            Some(LoginBlock::Server {
                retry_after: secs(30)
            })
        );
        assert!(l.begin("fresh", t0 + secs(61)).is_ok());
    }

    #[test]
    fn parallel_attempts_cannot_all_be_admitted() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", FREE_FAILURES, t0);
        let first = l.begin("bob", t0);
        assert!(first.is_ok());
        assert!(matches!(
            l.begin("bob", t0).err(),
            Some(LoginBlock::Account { .. })
        ));
    }

    #[test]
    fn in_flight_attempts_count_against_the_free_failures() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        let admitted: Vec<_> = (0..FREE_FAILURES)
            .map(|_| l.begin("bob", t0).unwrap())
            .collect();
        assert!(matches!(
            l.begin("bob", t0).err(),
            Some(LoginBlock::Account { .. })
        ));
        drop(admitted);
    }

    #[test]
    fn in_flight_attempts_count_against_the_server_budget() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        let admitted: Vec<_> = (0..SERVER_FAILURES_PER_MINUTE)
            .map(|i| l.begin(&format!("user{i}"), t0).unwrap())
            .collect();
        assert_eq!(
            l.begin("fresh", t0).err(),
            Some(LoginBlock::Server {
                retry_after: SERVER_WINDOW
            })
        );
        drop(admitted);
    }

    #[test]
    fn a_dropped_attempt_releases_its_slot_and_records_nothing() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", FREE_FAILURES, t0);
        drop(l.begin("bob", t0).unwrap());
        // The slot is free again, and the count is still 3: no wait yet.
        let attempt = l.begin("bob", t0).unwrap();
        attempt.failed(t0);
        assert_eq!(
            l.begin("bob", t0).err(),
            Some(LoginBlock::Account {
                retry_after: secs(2)
            })
        );
        // Nothing of the dropped attempt reached the server budget either.
        let state = l.lock();
        assert_eq!(state.window_failures, FREE_FAILURES + 1);
        assert_eq!(state.in_flight, 0);
    }

    #[test]
    fn a_failure_after_the_window_starts_a_new_window_at_one() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        for i in 0..SERVER_FAILURES_PER_MINUTE {
            fail(&l, &format!("user{i}"), 1, t0);
        }
        let next = t0 + SERVER_WINDOW;
        l.begin("late", next).unwrap().failed(next);
        let soon = next + secs(1);
        let admitted: Vec<_> = (1..SERVER_FAILURES_PER_MINUTE)
            .map(|i| l.begin(&format!("again{i}"), soon).unwrap())
            .collect();
        assert_eq!(
            l.begin("fresh", soon).err(),
            Some(LoginBlock::Server {
                retry_after: secs(59)
            })
        );
        drop(admitted);
    }

    #[test]
    fn the_block_messages_round_seconds_up() {
        let account = LoginBlock::Account {
            retry_after: Duration::from_millis(1500),
        };
        assert_eq!(
            account.to_string(),
            "Too many failed sign-ins for this account. Try again in 2 seconds."
        );
        let server = LoginBlock::Server {
            retry_after: secs(30),
        };
        assert_eq!(
            server.to_string(),
            "Too many failed sign-ins on this server. Try again in 30 seconds."
        );
        let last = LoginBlock::Account {
            retry_after: Duration::from_millis(400),
        };
        assert_eq!(
            last.to_string(),
            "Too many failed sign-ins for this account. Try again in 1 second."
        );
    }
}
