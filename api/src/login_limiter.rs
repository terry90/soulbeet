//! Slows repeated failed sign-ins: a growing wait per name and a failure
//! budget for the whole server, of which names with no account can spend only
//! part. Password proofs of signed-in sessions have a wait per account of
//! their own and no server budget, so failures at the login form never hold
//! them back. Nothing depends on the client address.
//!
//! An attempt is admitted by its name, then counted against the server's
//! budget before its credentials are checked. From each step on it counts as
//! a pending failure until it settles, so parallel requests cannot all slip
//! through before the first failure is recorded.

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
/// The part of those that names with no account may fill.
pub const NO_ACCOUNT_FAILURES_PER_MINUTE: u32 = 30;

const SERVER_WINDOW: Duration = Duration::from_secs(60);

/// The shortest wait offered while another attempt on the account is still
/// being checked: that attempt settles within a request's time.
const IN_FLIGHT_RETRY: Duration = Duration::from_secs(1);

// A forgotten account must never still be waiting out a delay.
const _: () = assert!(MAX_DELAY.as_nanos() <= FAILURE_MEMORY.as_nanos());
// Names with no account alone never refuse the names that have one.
const _: () = assert!(NO_ACCOUNT_FAILURES_PER_MINUTE < SERVER_FAILURES_PER_MINUTE);

/// The part of the server's failure budget a sign-in draws on, known once
/// its name is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// The name has an account here.
    Account,
    /// The name has no account here, yet Navidrome may know it.
    NoAccount,
}

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
    /// Failures of counted attempts in the window, of every name.
    window_failures: u32,
    /// The part of `window_failures` of names with no account.
    window_no_account_failures: u32,
    /// Counted attempts not settled yet, of every name.
    in_flight: u32,
    /// The part of `in_flight` of names with no account.
    no_account_in_flight: u32,
    server_block_logged: bool,
    no_account_block_logged: bool,
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

    /// Whether the window that started at the last failure is still open
    /// at `now`.
    fn window_open(&self, now: Instant) -> bool {
        self.window_start
            .is_some_and(|start| now < start + SERVER_WINDOW)
    }

    /// The failures of the window open at `now` and the counted attempts in
    /// flight: of every name, and of names with no account.
    fn spent(&self, now: Instant) -> (u32, u32) {
        let (all, no_account) = if self.window_open(now) {
            (self.window_failures, self.window_no_account_failures)
        } else {
            (0, 0)
        };
        (all + self.in_flight, no_account + self.no_account_in_flight)
    }

    /// Whether an attempt drawing on `budget` is refused at `now`. `None`
    /// asks before the name is looked up, so only the whole budget counts.
    fn server_block(&self, now: Instant, budget: Option<Budget>) -> Option<LoginBlock> {
        let (all, no_account) = self.spent(now);
        let full = all >= SERVER_FAILURES_PER_MINUTE
            || (budget == Some(Budget::NoAccount) && no_account >= NO_ACCOUNT_FAILURES_PER_MINUTE);
        if !full {
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

    /// Refuses with the server's `block`, logged once until an attempt of
    /// the refused kind is counted again.
    fn refuse(&mut self, block: LoginBlock, now: Instant) -> LoginBlock {
        if self.spent(now).0 >= SERVER_FAILURES_PER_MINUTE {
            if !self.server_block_logged {
                self.server_block_logged = true;
                tracing::warn!("Too many failed sign-ins on this server, refusing all sign-ins");
            }
        } else if !self.no_account_block_logged {
            self.no_account_block_logged = true;
            tracing::warn!("Too many failed sign-ins of names with no account, refusing them");
        }
        block
    }

    /// Admits an attempt for the name `key` and holds its slot. The server's
    /// budget is checked whole: the name is not looked up yet.
    fn admit(&mut self, key: &str, now: Instant) -> Result<(), LoginBlock> {
        if let Some(block) = self.server_block(now, None) {
            return Err(self.refuse(block, now));
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
        Ok(())
    }

    /// Counts an admitted attempt against `budget` until it settles.
    fn count(&mut self, budget: Budget, now: Instant) -> Result<(), LoginBlock> {
        if let Some(block) = self.server_block(now, Some(budget)) {
            return Err(self.refuse(block, now));
        }
        self.in_flight += 1;
        self.server_block_logged = false;
        if budget == Budget::NoAccount {
            self.no_account_in_flight += 1;
            self.no_account_block_logged = false;
        }
        Ok(())
    }

    /// Records a failure of the name `key`, and of the server's budget when
    /// the attempt was counted against `budget`.
    fn record_failure(&mut self, key: &str, budget: Option<Budget>, now: Instant) {
        self.accounts
            .retain(|_, a| a.in_flight > 0 || !a.forgotten(now));

        let account = self.account(key, now);
        account.failures = account.failures.saturating_add(1);
        account.last_failure = now;

        let Some(budget) = budget else {
            return;
        };
        if !self.window_open(now) {
            self.window_start = Some(now);
            self.window_failures = 0;
            self.window_no_account_failures = 0;
        }
        self.window_failures += 1;
        if budget == Budget::NoAccount {
            self.window_no_account_failures += 1;
        }
    }

    fn clear(&mut self, key: &str) {
        if let Some(account) = self.accounts.get_mut(key) {
            account.failures = 0;
        }
    }

    /// Frees the slots an attempt held. An entry with nothing left to
    /// remember is removed.
    fn release(&mut self, key: &str, budget: Option<Budget>) {
        if let Some(budget) = budget {
            self.in_flight -= 1;
            if budget == Budget::NoAccount {
                self.no_account_in_flight -= 1;
            }
        }
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
    /// Sign-ins at the login form, by name.
    form: Mutex<State>,
    /// Password proofs of signed-in sessions, by account id. None of them
    /// is counted against a server budget, so nothing fills this one's.
    proofs: Mutex<State>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
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

    /// Admits a sign-in attempt at the login form for `username` and holds
    /// its slot until it settles. It draws on the server's budget once
    /// `LoginAttempt::count` says which part.
    pub fn begin(&self, username: &str, now: Instant) -> Result<LoginAttempt<'_>, LoginBlock> {
        Self::begin_in(&self.form, key(username), now)
    }

    /// Admits a signed-in session's proof of the password of the account
    /// `account_id` and holds its slot until it settles. Failed sign-ins of
    /// the account's name do not hold it back, and its failures do not hold
    /// those back.
    pub fn begin_proof(
        &self,
        account_id: &str,
        now: Instant,
    ) -> Result<LoginAttempt<'_>, LoginBlock> {
        Self::begin_in(&self.proofs, account_id.to_owned(), now)
    }

    fn begin_in(
        state: &Mutex<State>,
        key: String,
        now: Instant,
    ) -> Result<LoginAttempt<'_>, LoginBlock> {
        lock(state).admit(&key, now)?;
        Ok(LoginAttempt {
            state,
            key: Some(key),
            budget: None,
        })
    }
}

/// An admitted sign-in attempt. It counts as a failure for its name until it
/// settles, and for the server's budget once counted; dropping it unsettled
/// frees its slots and records nothing.
#[must_use = "an attempt dropped unsettled records nothing"]
pub struct LoginAttempt<'a> {
    state: &'a Mutex<State>,
    /// `None` once settled.
    key: Option<String>,
    /// The part of the server's budget it draws on, once counted.
    budget: Option<Budget>,
}

impl LoginAttempt<'_> {
    /// Counts the attempt against `budget` before its credentials are
    /// checked. Refused while that budget is spent; the refused attempt
    /// records nothing.
    pub fn count(mut self, budget: Budget, now: Instant) -> Result<Self, LoginBlock> {
        // The guard goes first: a refused attempt is dropped, which locks.
        let counted = lock(self.state).count(budget, now);
        counted?;
        self.budget = Some(budget);
        Ok(self)
    }

    /// The credentials did not match.
    pub fn failed(mut self, now: Instant) {
        self.settle(|state, key, budget| state.record_failure(key, budget, now));
    }

    /// The credentials matched: the name's failures are cleared.
    pub fn succeeded(mut self) {
        self.settle(|state, key, _| state.clear(key));
    }

    fn settle(&mut self, record: impl FnOnce(&mut State, &str, Option<Budget>)) {
        if let Some(key) = self.key.take() {
            let mut state = lock(self.state);
            record(&mut state, &key, self.budget);
            state.release(&key, self.budget);
        }
    }
}

impl Drop for LoginAttempt<'_> {
    fn drop(&mut self) {
        self.settle(|_, _, _| {});
    }
}

pub static LOGIN_LIMITER: LazyLock<LoginLimiter> = LazyLock::new(LoginLimiter::new);

#[cfg(test)]
mod tests {
    use super::*;

    /// Records settled failures of a name with an account at the login
    /// form directly, as if each attempt had been admitted and counted
    /// earlier, so a test can start from any history.
    fn fail(limiter: &LoginLimiter, name: &str, times: u32, at: Instant) {
        for _ in 0..times {
            lock(&limiter.form).record_failure(&key(name), Some(Budget::Account), at);
        }
    }

    /// An attempt at the login form, admitted and counted against `budget`.
    fn counted<'a>(
        l: &'a LoginLimiter,
        name: &str,
        budget: Budget,
        at: Instant,
    ) -> LoginAttempt<'a> {
        l.begin(name, at).unwrap().count(budget, at).unwrap()
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
            .map(|i| counted(&l, &format!("user{i}"), Budget::Account, t0))
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
        counted(&l, "bob", Budget::Account, t0).failed(t0);
        assert_eq!(
            l.begin("bob", t0).err(),
            Some(LoginBlock::Account {
                retry_after: secs(2)
            })
        );
        // Nothing of the dropped attempt reached the server budget either.
        let state = lock(&l.form);
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
        counted(&l, "late", Budget::Account, next).failed(next);
        let soon = next + secs(1);
        let admitted: Vec<_> = (1..SERVER_FAILURES_PER_MINUTE)
            .map(|i| counted(&l, &format!("again{i}"), Budget::Account, soon))
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
    fn names_with_no_account_fill_only_their_part_of_the_server_budget() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        for i in 0..NO_ACCOUNT_FAILURES_PER_MINUTE {
            counted(&l, &format!("ghost{i}"), Budget::NoAccount, t0).failed(t0);
        }
        // Admitted by name, then refused once looked up.
        let ghost = l.begin("ghost", t0).unwrap();
        assert_eq!(
            ghost.count(Budget::NoAccount, t0).err(),
            Some(LoginBlock::Server {
                retry_after: SERVER_WINDOW
            })
        );
        // The names that have an account keep the rest.
        let admitted: Vec<_> = (NO_ACCOUNT_FAILURES_PER_MINUTE..SERVER_FAILURES_PER_MINUTE)
            .map(|i| counted(&l, &format!("user{i}"), Budget::Account, t0))
            .collect();
        assert!(matches!(
            l.begin("fresh", t0).err(),
            Some(LoginBlock::Server { .. })
        ));
        drop(admitted);
        assert!(l.begin("fresh", t0).is_ok());
        // The refused attempt recorded nothing and holds nothing.
        let state = lock(&l.form);
        assert_eq!(
            state.window_no_account_failures,
            NO_ACCOUNT_FAILURES_PER_MINUTE
        );
        assert_eq!((state.in_flight, state.no_account_in_flight), (0, 0));
        assert!(!state.accounts.contains_key("ghost"));
    }

    #[test]
    fn names_with_no_account_in_flight_fill_only_their_part_too() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        let ghosts: Vec<_> = (0..NO_ACCOUNT_FAILURES_PER_MINUTE)
            .map(|i| counted(&l, &format!("ghost{i}"), Budget::NoAccount, t0))
            .collect();
        assert!(l
            .begin("ghost", t0)
            .unwrap()
            .count(Budget::NoAccount, t0)
            .is_err());
        assert!(l
            .begin("ann", t0)
            .unwrap()
            .count(Budget::Account, t0)
            .is_ok());
        drop(ghosts);
        assert!(l
            .begin("ghost", t0)
            .unwrap()
            .count(Budget::NoAccount, t0)
            .is_ok());
    }

    #[test]
    fn proofs_and_the_login_form_keep_separate_counts() {
        let t0 = Instant::now();
        let l = LoginLimiter::new();
        // The form is closed to "bob", and to everyone.
        fail(&l, "bob", 10, t0);
        for i in 0..SERVER_FAILURES_PER_MINUTE {
            fail(&l, &format!("user{i}"), 1, t0);
        }
        assert!(l.begin("bob", t0).is_err());
        // Bob's session still proves his password.
        l.begin_proof("id-bob", t0).unwrap().succeeded();

        // Its failures wait on their own and never close the server.
        let l = LoginLimiter::new();
        for _ in 0..=FREE_FAILURES {
            l.begin_proof("id-bob", t0).unwrap().failed(t0);
        }
        assert!(matches!(
            l.begin_proof("id-bob", t0).err(),
            Some(LoginBlock::Account { .. })
        ));
        assert!(l.begin("bob", t0).is_ok());
        for i in 0..SERVER_FAILURES_PER_MINUTE {
            l.begin_proof(&format!("id-{i}"), t0).unwrap().failed(t0);
        }
        assert!(l.begin_proof("id-ann", t0).is_ok());
        assert!(l.begin("ann", t0).is_ok());
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
