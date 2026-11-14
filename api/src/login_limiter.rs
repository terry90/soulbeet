//! Slows repeated failed sign-ins: a growing wait per account and a failure
//! budget for the whole server. Nothing depends on the client address.

use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Consecutive failures an account may make before waits start.
pub const FREE_FAILURES: u32 = 3;
/// Longest wait after a failure.
pub const MAX_DELAY: Duration = Duration::from_secs(900);
/// An account with no failure for this long is forgotten.
pub const FAILURE_MEMORY: Duration = Duration::from_secs(900);
/// Failures the server accepts per fixed 60 second window.
pub const SERVER_FAILURES_PER_MINUTE: u32 = 60;

const SERVER_WINDOW: Duration = Duration::from_secs(60);

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
        let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
        write!(
            f,
            "Too many failed sign-ins {what}. Try again in {seconds} seconds."
        )
    }
}

struct AccountFailures {
    count: u32,
    last: Instant,
}

#[derive(Default)]
struct State {
    accounts: HashMap<String, AccountFailures>,
    window_start: Option<Instant>,
    window_failures: u32,
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

    pub fn check(&self, username: &str, now: Instant) -> Result<(), LoginBlock> {
        let state = self.lock();
        if let Some(start) = state.window_start {
            let end = start + SERVER_WINDOW;
            if now < end && state.window_failures >= SERVER_FAILURES_PER_MINUTE {
                return Err(LoginBlock::Server {
                    retry_after: end - now,
                });
            }
        }
        if let Some(account) = state.accounts.get(&key(username)) {
            let ready = account.last + delay_after(account.count);
            if now < ready {
                return Err(LoginBlock::Account {
                    retry_after: ready - now,
                });
            }
        }
        Ok(())
    }

    pub fn record_failure(&self, username: &str, now: Instant) {
        let mut state = self.lock();
        state
            .accounts
            .retain(|_, a| now.saturating_duration_since(a.last) < FAILURE_MEMORY);

        let account = state
            .accounts
            .entry(key(username))
            .or_insert(AccountFailures {
                count: 0,
                last: now,
            });
        account.count = account.count.saturating_add(1);
        account.last = now;

        match state.window_start {
            Some(start) if now < start + SERVER_WINDOW => state.window_failures += 1,
            _ => {
                state.window_start = Some(now);
                state.window_failures = 1;
            }
        }
    }

    pub fn record_success(&self, username: &str) {
        self.lock().accounts.remove(&key(username));
    }
}

pub static LOGIN_LIMITER: LazyLock<LoginLimiter> = LazyLock::new(LoginLimiter::new);

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(limiter: &LoginLimiter, name: &str, times: u32, at: Instant) {
        for _ in 0..times {
            limiter.record_failure(name, at);
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
        assert_eq!(l.check("bob", t0), Ok(()));
    }

    #[test]
    fn the_fourth_failure_waits_two_seconds() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", 4, t0);
        assert_eq!(
            l.check("bob", t0 + secs(1)),
            Err(LoginBlock::Account {
                retry_after: secs(1)
            })
        );
        assert_eq!(l.check("bob", t0 + secs(2)), Ok(()));
    }

    #[test]
    fn the_delay_doubles_and_stops_at_fifteen_minutes() {
        let t0 = Instant::now();
        for (failures, wait) in [(5, 4), (13, 900), (20, 900)] {
            let l = LoginLimiter::new();
            fail(&l, "bob", failures, t0);
            assert_eq!(
                l.check("bob", t0),
                Err(LoginBlock::Account {
                    retry_after: secs(wait)
                }),
                "{failures} failures"
            );
            assert_eq!(l.check("bob", t0 + secs(wait)), Ok(()));
        }
    }

    #[test]
    fn a_success_resets_the_count() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", 6, t0);
        l.record_success("bob");
        fail(&l, "bob", 1, t0);
        assert_eq!(l.check("bob", t0), Ok(()));
    }

    #[test]
    fn usernames_are_compared_without_case() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, " Bob", 4, t0);
        assert!(l.check("bob", t0).is_err());
    }

    #[test]
    fn a_quiet_account_is_forgotten() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        fail(&l, "bob", 10, t0);
        let later = t0 + secs(901);
        assert_eq!(l.check("bob", later), Ok(()));
        fail(&l, "bob", 1, later);
        assert_eq!(l.check("bob", later), Ok(()));
    }

    #[test]
    fn the_server_budget_blocks_everyone_for_the_rest_of_the_minute() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        for i in 0..SERVER_FAILURES_PER_MINUTE {
            l.record_failure(
                &format!("user{i}"),
                t0 + Duration::from_millis(u64::from(i) * 500),
            );
        }
        assert!(matches!(
            l.check("fresh", t0 + secs(30)),
            Err(LoginBlock::Server { .. })
        ));
        assert_eq!(l.check("fresh", t0 + secs(61)), Ok(()));
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
    }
}
