//! Starting a Navidrome scan after a user's imports.
//!
//! Navidrome lets only an admin start a scan (server/subsonic/library_scanning.go:43-45), and the
//! scan is the server's: startScan without targets scans every library (:83-93), and
//! getScanStatus reads the scanner's own state and the latest lastScan of all libraries,
//! whoever asks (scanner/controller.go:108-123, 149-183; persistence/library_repository.go:
//! 276-281 reads them unfiltered). So when the user's own account may not start one, Soulbeet
//! starts it with another linked account that may. Every linked account signs in to the one
//! NAVIDROME_URL (services::navidrome_client_for_user), so all are on the user's server. Whether
//! the user's own account may is read with their own getUser (SubsonicUser keeps adminRole
//! only). Another account is only ever sent startScan, and whether it may start one is learnt
//! from that answer: nothing is read with it, and nothing of it reaches the user.

use crate::auth::UserId;
use crate::core::index::NavidromeOf;
use crate::models::user::User;
use crate::services::NavidromeLookupError;
use soulbeet::navidrome::error::NOT_AUTHORIZED;
use soulbeet::navidrome::NavidromeClient;
use soulbeet::NavidromeError;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};
use tracing::debug;

/// How long what Navidrome said of an account is believed, so an account made an admin, or one
/// no longer an admin, is noticed within it. A refused startScan corrects it at once.
const ROLE_FRESH_FOR: Duration = Duration::from_secs(15 * 60);

/// What the running instance knows of the linked accounts.
pub(crate) static SCAN_ROLES: LazyLock<ScanRoles> = LazyLock::new(ScanRoles::default);

/// Whether Navidrome lets each user's account start a scan, as it last said, by user. Each answer
/// holds for the client it came from only: a new Navidrome password or sign-in, a provider reload
/// or an account deleted in Soulbeet drops the client (services::evict_navidrome_client), and
/// nothing is known of the next one.
#[derive(Default)]
pub(crate) struct ScanRoles {
    roles: Mutex<HashMap<UserId, Role>>,
}

struct Role {
    /// Weak, so an evicted client's password is not kept. It also keeps the allocation, so no
    /// later client can have its address.
    client: Weak<NavidromeClient>,
    may_scan: bool,
    at: Instant,
}

impl ScanRoles {
    /// What Navidrome said of `user`'s account signed in as `client` less than ROLE_FRESH_FOR
    /// before `now`.
    pub(crate) fn known(
        &self,
        user: &UserId,
        client: &Arc<NavidromeClient>,
        now: Instant,
    ) -> Option<bool> {
        let roles = self.roles.lock().unwrap_or_else(PoisonError::into_inner);
        roles
            .get(user)
            .filter(|role| {
                role.client.as_ptr() == Arc::as_ptr(client)
                    && now.duration_since(role.at) < ROLE_FRESH_FOR
            })
            .map(|role| role.may_scan)
    }

    /// Notes that Navidrome said at `now` whether `user`'s account signed in as `client` may start
    /// a scan. The answers of clients no longer in use go.
    pub(crate) fn learn(
        &self,
        user: &UserId,
        client: &Arc<NavidromeClient>,
        may_scan: bool,
        now: Instant,
    ) {
        let mut roles = self.roles.lock().unwrap_or_else(PoisonError::into_inner);
        roles.retain(|_, role| role.client.strong_count() > 0);
        roles.insert(
            user.clone(),
            Role {
                client: Arc::downgrade(client),
                may_scan,
                at: now,
            },
        );
    }
}

/// How a request to scan for a user ended.
#[derive(Debug, PartialEq)]
pub(crate) enum ScanStart {
    /// Navidrome took it, from the user's own account or another linked one.
    Started,
    /// No linked account may start a scan, so none was asked: the imports wait for Navidrome's
    /// own scan (its file watcher or scan schedule).
    NoAccount,
}

/// Whose account starts the scans after a user's imports, as far as Navidrome said.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScanBy {
    Own,
    Another,
    Nobody,
    /// The user's own account may not, and nothing is known yet of another linked one.
    Unknown,
}

impl ScanBy {
    /// Whether Soulbeet can start a scan after the user's imports; None while unknown.
    pub(crate) fn can_scan(self) -> Option<bool> {
        match self {
            ScanBy::Own | ScanBy::Another => Some(true),
            ScanBy::Nobody => Some(false),
            ScanBy::Unknown => None,
        }
    }

    /// What it means for the user's imports.
    pub(crate) fn note(self) -> &'static str {
        match self {
            ScanBy::Own => "Your Navidrome account is an admin, so Soulbeet starts a Navidrome scan after your imports.",
            ScanBy::Another => "Your Navidrome account is not an admin, so Soulbeet starts the Navidrome scan after your imports with another linked account that is.",
            ScanBy::Nobody => "No Navidrome account linked to Soulbeet may start a scan, so new tracks appear at Navidrome's next own scan (its file watcher or scan schedule). Signing in to Soulbeet once as a Navidrome admin lets Soulbeet start one.",
            ScanBy::Unknown => "Your Navidrome account is not an admin. Soulbeet learns at your next import whether another linked account may start the Navidrome scan; if none may, new tracks appear at Navidrome's next own scan (its file watcher or scan schedule).",
        }
    }
}

/// Starts a quick Navidrome scan for `user`, whose own client is `own`: with their own account
/// when it may, else with the other linked accounts other_scanners gives, in its order, until
/// Navidrome takes one. An account Navidrome refuses is noted as one that may not.
pub(crate) async fn start_scan_for(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    roles: &ScanRoles,
    user: &UserId,
    own: &Arc<NavidromeClient>,
) -> Result<ScanStart, NavidromeLookupError> {
    start_scan_with_options_for(db, navidrome, roles, user, own, false).await
}

/// Starts a Navidrome scan for `user`, passing `fullScan=true` when `full_scan` is set: with
/// their own account when it may, else with the other linked accounts other_scanners gives, in
/// its order, until Navidrome takes one. An account Navidrome refuses is noted as one that may
/// not.
pub(crate) async fn start_scan_with_options_for(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    roles: &ScanRoles,
    user: &UserId,
    own: &Arc<NavidromeClient>,
    full_scan: bool,
) -> Result<ScanStart, NavidromeLookupError> {
    let own_may = may_scan(roles, user, own)
        .await
        .map_err(NavidromeLookupError::Client)?;
    if own_may && scan_with_options(roles, user, own, user, full_scan).await? {
        return Ok(ScanStart::Started);
    }
    for (other, client, _) in other_scanners(db, navidrome, roles, user).await? {
        if scan_with_options(roles, &other, &client, user, full_scan).await? {
            debug!("Started a Navidrome scan for {user} with the account of {other}");
            return Ok(ScanStart::Started);
        }
    }
    Ok(ScanStart::NoAccount)
}

/// Whose account starts the scans after `user`'s imports, `admin` their own adminRole, from what
/// Navidrome said of the other linked accounts alone: none is asked anything.
pub(crate) async fn scan_by(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    roles: &ScanRoles,
    user: &UserId,
    admin: bool,
) -> Result<ScanBy, NavidromeLookupError> {
    if admin {
        return Ok(ScanBy::Own);
    }
    Ok(match other_scanners(db, navidrome, roles, user).await?.first() {
        None => ScanBy::Nobody,
        Some((_, _, Some(true))) => ScanBy::Another,
        Some(_) => ScanBy::Unknown,
    })
}

/// The other linked accounts that may start a scan for `user`, with what Navidrome last said of
/// each: those it said may first, then those nothing is known of. Those it said may not, and
/// those that cannot be signed in with, are left out. No request is made.
async fn other_scanners(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    roles: &ScanRoles,
    user: &UserId,
) -> Result<Vec<(UserId, Arc<NavidromeClient>, Option<bool>)>, NavidromeLookupError> {
    let linked = User::linked_to_navidrome_in(db)
        .await
        .map_err(NavidromeLookupError::Database)?;
    let now = Instant::now();
    let mut others = Vec::new();
    for other in linked.iter().map(|u| UserId::new(&u.id)) {
        if other == *user {
            continue;
        }
        match navidrome.navidrome_of(&other).await {
            Ok(client) => {
                let known = roles.known(&other, &client, now);
                if known != Some(false) {
                    others.push((other, client, known));
                }
            }
            Err(e) => debug!("The Navidrome account of {other} cannot start a scan: {e}"),
        }
    }
    others.sort_by_key(|(_, _, known)| *known != Some(true));
    Ok(others)
}

/// Asks the account of `by`, signed in as `client`, to start a scan (full when `full_scan`) for
/// `user`, and notes what Navidrome's answer says of it. Ok(false) when Navidrome refuses it.
async fn scan_with_options(
    roles: &ScanRoles,
    by: &UserId,
    client: &Arc<NavidromeClient>,
    user: &UserId,
    full_scan: bool,
) -> Result<bool, NavidromeLookupError> {
    match client.start_scan_options(full_scan).await {
        Ok(()) => {
            roles.learn(by, client, true, Instant::now());
            Ok(true)
        }
        Err(e) if refused(&e, by, user) => {
            debug!("Navidrome does not let the account of {by} start a scan: {e}");
            roles.learn(by, client, false, Instant::now());
            Ok(false)
        }
        Err(e) => Err(NavidromeLookupError::Client(e)),
    }
}

/// Whether Navidrome lets `user`'s own account signed in as `client` start a scan: what it said
/// last, else its getUser adminRole.
async fn may_scan(
    roles: &ScanRoles,
    user: &UserId,
    client: &Arc<NavidromeClient>,
) -> Result<bool, NavidromeError> {
    if let Some(may_scan) = roles.known(user, client, Instant::now()) {
        return Ok(may_scan);
    }
    let admin = client.get_own_user().await?.admin_role;
    roles.learn(user, client, admin, Instant::now());
    Ok(admin)
}

/// Whether Navidrome's error `e` to the account of `by`, asked for `user`, says that account may
/// not start a scan: it is no admin (startScan's ErrorAuthorizationFail), or it is another user's
/// and Navidrome refuses its credentials, which is not `user`'s failure.
fn refused(e: &NavidromeError, by: &UserId, user: &UserId) -> bool {
    matches!(
        e,
        NavidromeError::Subsonic {
            code: NOT_AUTHORIZED,
            ..
        }
    ) || (by != user && e.is_wrong_credentials())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_navidrome::{subsonic_failed, user_body, NavidromeEach, NavidromeStub};

    /// A user `id` with a Navidrome password stored, so linked_to_navidrome_in lists them.
    async fn linked(db: &SqlitePool, id: &str) -> UserId {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, navidrome_token) VALUES (?, ?, 'x', 'cipher')",
        )
        .bind(id)
        .bind(id)
        .execute(db)
        .await
        .unwrap();
        UserId::new(id)
    }

    /// A Navidrome stub whose getUser says the account is an admin when `admin`.
    fn account(admin: bool) -> NavidromeStub {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getUser", &[&user_body(admin)]);
        navi
    }

    const REFUSED: (u32, &str) = (50, "User is not authorized for the given operation");

    #[tokio::test]
    async fn an_admin_account_starts_its_own_scan_and_no_other_is_asked() {
        let db = crate::db::test_db::migrated().await;
        let (alice, bob) = (linked(&db, "alice").await, linked(&db, "bob").await);
        let (own, other) = (account(true), NavidromeStub::start());
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &other)]);
        let roles = ScanRoles::default();
        let started =
            start_scan_for(&db, &navidrome, &roles, &alice, &navidrome.client(&alice)).await;
        assert_eq!(started.unwrap(), ScanStart::Started);
        assert_eq!(own.asked(), vec!["/rest/getUser", "/rest/startScan"]);
        assert!(other.asked().is_empty());
    }

    #[tokio::test]
    async fn a_user_who_may_not_scan_has_it_started_by_a_linked_admin_who_is_asked_only_to_scan() {
        let db = crate::db::test_db::migrated().await;
        let (alice, bob, carol) = (
            linked(&db, "alice").await,
            linked(&db, "bob").await,
            linked(&db, "carol").await,
        );
        let (own, plain, admin) = (account(false), NavidromeStub::start(), NavidromeStub::start());
        plain.answer("/rest/startScan", &[&subsonic_failed(REFUSED.0, REFUSED.1)]);
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &plain), (&carol, &admin)]);
        let roles = ScanRoles::default();
        let own_client = navidrome.client(&alice);
        assert_eq!(
            scan_by(&db, &navidrome, &roles, &alice, false)
                .await
                .unwrap(),
            ScanBy::Unknown
        );
        let started = start_scan_for(&db, &navidrome, &roles, &alice, &own_client).await;
        assert_eq!(started.unwrap(), ScanStart::Started);
        assert_eq!(own.asked(), vec!["/rest/getUser"]);
        // bob is asked first when the database lists him before carol, and refuses.
        let plain_asked = plain.asked();
        assert!(plain_asked.iter().all(|path| path == "/rest/startScan"));
        assert!(plain_asked.len() <= 1, "{plain_asked:?}");
        assert_eq!(admin.asked(), vec!["/rest/startScan"]);
        // Known now: the next import asks only the admin, and only to scan.
        let again = start_scan_for(&db, &navidrome, &roles, &alice, &own_client).await;
        assert_eq!(again.unwrap(), ScanStart::Started);
        assert_eq!((own.asked().len(), plain.asked()), (1, plain_asked));
        assert_eq!(admin.asked(), vec!["/rest/startScan", "/rest/startScan"]);
        assert_eq!(
            scan_by(&db, &navidrome, &roles, &alice, false)
                .await
                .unwrap(),
            ScanBy::Another
        );
        assert_eq!(admin.asked().len(), 2);
    }

    #[tokio::test]
    async fn with_no_admin_no_scan_is_asked_for_and_navidrome_is_not_asked_again() {
        let db = crate::db::test_db::migrated().await;
        let (alice, bob) = (linked(&db, "alice").await, linked(&db, "bob").await);
        let (own, other) = (account(false), NavidromeStub::start());
        other.answer("/rest/startScan", &[&subsonic_failed(REFUSED.0, REFUSED.1)]);
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &other)]);
        let roles = ScanRoles::default();
        let own_client = navidrome.client(&alice);
        for _ in 0..2 {
            let started = start_scan_for(&db, &navidrome, &roles, &alice, &own_client).await;
            assert_eq!(started.unwrap(), ScanStart::NoAccount);
        }
        assert_eq!(own.asked(), vec!["/rest/getUser"]);
        assert_eq!(other.asked(), vec!["/rest/startScan"]);
        assert_eq!(
            scan_by(&db, &navidrome, &roles, &alice, false)
                .await
                .unwrap(),
            ScanBy::Nobody
        );
        assert_eq!(other.asked().len(), 1);
    }

    #[tokio::test]
    async fn status_says_from_what_is_known_alone_who_starts_the_scans() {
        let db = crate::db::test_db::migrated().await;
        let alice = linked(&db, "alice").await;
        let (own, other) = (NavidromeStub::start(), NavidromeStub::start());
        let roles = ScanRoles::default();
        let alone = NavidromeEach::of(&[(&alice, &own)]);
        let by = scan_by(&db, &alone, &roles, &alice, true).await;
        assert_eq!(by.unwrap(), ScanBy::Own);
        let by = scan_by(&db, &alone, &roles, &alice, false).await;
        assert_eq!(by.unwrap(), ScanBy::Nobody);
        // Nothing is known of bob's account until an import sends it startScan.
        let bob = linked(&db, "bob").await;
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &other)]);
        let by = scan_by(&db, &navidrome, &roles, &alice, false).await;
        assert_eq!(by.unwrap(), ScanBy::Unknown);
        assert_eq!(ScanBy::Unknown.can_scan(), None);
        let bob_client = navidrome.client(&bob);
        roles.learn(&bob, &bob_client, false, Instant::now());
        let by = scan_by(&db, &navidrome, &roles, &alice, false).await;
        assert_eq!(by.unwrap(), ScanBy::Nobody);
        roles.learn(&bob, &bob_client, true, Instant::now());
        let by = scan_by(&db, &navidrome, &roles, &alice, false).await;
        assert_eq!(by.unwrap(), ScanBy::Another);
        assert!(own.asked().is_empty() && other.asked().is_empty());
    }

    #[tokio::test]
    async fn an_admin_no_longer_one_is_passed_over_for_the_next() {
        let db = crate::db::test_db::migrated().await;
        let (alice, bob) = (linked(&db, "alice").await, linked(&db, "bob").await);
        // alice's getUser still says admin, as Navidrome said before she lost the role.
        let (own, other) = (account(true), NavidromeStub::start());
        own.answer("/rest/startScan", &[&subsonic_failed(REFUSED.0, REFUSED.1)]);
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &other)]);
        let roles = ScanRoles::default();
        let own_client = navidrome.client(&alice);
        let started = start_scan_for(&db, &navidrome, &roles, &alice, &own_client).await;
        assert_eq!(started.unwrap(), ScanStart::Started);
        assert_eq!(own.asked(), vec!["/rest/getUser", "/rest/startScan"]);
        assert_eq!(other.asked(), vec!["/rest/startScan"]);
        assert_eq!(
            roles.known(&alice, &own_client, Instant::now()),
            Some(false)
        );
    }

    #[tokio::test]
    async fn another_accounts_refused_password_is_passed_over_and_its_server_failure_returned() {
        let db = crate::db::test_db::migrated().await;
        let (alice, bob) = (linked(&db, "alice").await, linked(&db, "bob").await);
        let (own, other) = (account(false), NavidromeStub::start());
        other.answer(
            "/rest/startScan",
            &[&subsonic_failed(40, "Wrong username or password")],
        );
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &other)]);
        let roles = ScanRoles::default();
        let own_client = navidrome.client(&alice);
        let started = start_scan_for(&db, &navidrome, &roles, &alice, &own_client).await;
        assert_eq!(started.unwrap(), ScanStart::NoAccount);
        assert_eq!(other.asked(), vec!["/rest/startScan"]);
        assert_eq!(
            roles.known(&bob, &navidrome.client(&bob), Instant::now()),
            Some(false)
        );
        // Navidrome's internal error says nothing of bob's account: the next import asks again.
        let (own, other) = (account(false), NavidromeStub::start());
        other.answer("/rest/startScan", &[&subsonic_failed(0, "Internal error")]);
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &other)]);
        let own_client = navidrome.client(&alice);
        let failed = start_scan_for(&db, &navidrome, &roles, &alice, &own_client).await;
        assert!(
            matches!(
                failed,
                Err(NavidromeLookupError::Client(NavidromeError::Subsonic {
                    code: 0,
                    ..
                }))
            ),
            "{failed:?}"
        );
        assert_eq!(
            roles.known(&bob, &navidrome.client(&bob), Instant::now()),
            None
        );
    }

    #[tokio::test]
    async fn an_account_without_a_stored_password_is_never_asked() {
        let db = crate::db::test_db::migrated().await;
        let alice = linked(&db, "alice").await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('bob', 'bob', 'x')")
            .execute(&db)
            .await
            .unwrap();
        let bob = UserId::new("bob");
        let (own, other) = (account(false), account(true));
        let navidrome = NavidromeEach::of(&[(&alice, &own), (&bob, &other)]);
        let started = start_scan_for(
            &db,
            &navidrome,
            &ScanRoles::default(),
            &alice,
            &navidrome.client(&alice),
        )
        .await;
        assert_eq!(started.unwrap(), ScanStart::NoAccount);
        assert!(other.asked().is_empty());
    }

    #[tokio::test]
    async fn what_navidrome_said_holds_for_its_client_and_for_a_while() {
        let navi = NavidromeStub::start();
        let (client, alice, roles, at) = (
            navi.client(),
            UserId::new("alice"),
            ScanRoles::default(),
            Instant::now(),
        );
        roles.learn(&alice, &client, true, at);
        assert_eq!(roles.known(&alice, &client, at), Some(true));
        assert_eq!(
            roles.known(
                &alice,
                &client,
                at + ROLE_FRESH_FOR - Duration::from_secs(1)
            ),
            Some(true)
        );
        assert_eq!(roles.known(&alice, &client, at + ROLE_FRESH_FOR), None);
        // Another client of the same user: other credentials, nothing known of them.
        assert_eq!(roles.known(&alice, &navi.client(), at), None);
        assert_eq!(roles.known(&UserId::new("bob"), &client, at), None);
    }

    #[tokio::test]
    async fn start_scan_with_options_forwards_full_scan_flag() {
        let db = crate::db::test_db::migrated().await;
        let alice = linked(&db, "alice").await;
        let own = account(true);
        let navidrome = NavidromeEach::of(&[(&alice, &own)]);
        let roles = ScanRoles::default();
        let client = navidrome.client(&alice);

        assert_eq!(
            start_scan_with_options_for(&db, &navidrome, &roles, &alice, &client, false)
                .await
                .unwrap(),
            ScanStart::Started
        );
        assert_eq!(
            start_scan_with_options_for(&db, &navidrome, &roles, &alice, &client, true)
                .await
                .unwrap(),
            ScanStart::Started
        );
        assert_eq!(own.posted("/rest/startScan"), vec!["", "fullScan=true"]);
    }
}
