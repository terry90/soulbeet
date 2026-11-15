//! Registered OAuth clients and the rules their redirect URIs follow.

use sqlx::SqlitePool;
use url::Url;

use crate::base_url::is_loopback_host;

/// Most clients registered through `POST /oauth/register` (D4).
pub(crate) const DCR_CAP: i64 = 100;

const REDIRECT_REFUSED: &str =
    "redirect URIs must be https, or http on a loopback address, with no fragment";

/// A redirect URI is `https` with a host, or `http` on a loopback host, with
/// no fragment. It must also be written as an RFC 3986 URI with a host,
/// `scheme://host...`: the WHATWG parser browsers follow reads `https:///cb`
/// or `https:\\cb` as the host `cb` and drops tabs and newlines, where
/// RFC 3986 reads no host or no URI at all.
pub(crate) fn redirect_uri_allowed(uri: &str) -> Result<(), &'static str> {
    let url = Url::parse(uri).map_err(|_| REDIRECT_REFUSED)?;
    let written_with_host = uri
        .get(url.scheme().len()..)
        .and_then(|rest| rest.strip_prefix("://"))
        .is_some_and(|authority| !authority.starts_with('/'))
        && !uri.contains(|c: char| c == '\\' || c.is_ascii_whitespace() || c.is_ascii_control());
    if !written_with_host || url.fragment().is_some() {
        return Err(REDIRECT_REFUSED);
    }
    match (url.scheme(), url.host()) {
        ("https", Some(_)) => Ok(()),
        ("http", Some(host)) if is_loopback_host(&host) => Ok(()),
        _ => Err(REDIRECT_REFUSED),
    }
}

/// Exact match, or both loopback http URIs equal but for the port (RFC 8252 section 7.3; Claude Code's
/// http://localhost/callback, claude.com connectors/building/authentication.md:220).
pub(crate) fn redirect_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }
    match (
        loopback_without_port(registered),
        loopback_without_port(requested),
    ) {
        (Some(registered), Some(requested)) => registered == requested,
        _ => false,
    }
}

/// `uri` as written with its port taken out, when it is an `http://` URI on
/// a loopback host; the rest of the string is compared as sent.
fn loopback_without_port(uri: &str) -> Option<String> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "http" || !url.host().is_some_and(|host| is_loopback_host(&host)) {
        return None;
    }
    let rest = uri.strip_prefix("http://")?;
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    // The last ':' outside an IPv6 literal's brackets starts the port.
    let host = match authority.rfind(':') {
        Some(at)
            if !authority[at..].contains(']')
                && authority[at + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            &authority[..at]
        }
        _ => authority,
    };
    Some(format!("{host}{tail}"))
}

/// Whether `uri` returns to this computer: its host is loopback.
pub(crate) fn is_loopback_uri(uri: &str) -> bool {
    Url::parse(uri)
        .ok()
        .is_some_and(|url| url.host().is_some_and(|host| is_loopback_host(&host)))
}

/// How a client is known: registered through `POST /oauth/register`, or by
/// its Client ID Metadata Document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientKind {
    Dcr,
    Cimd,
}

impl ClientKind {
    /// The `kind` column's value.
    pub(crate) fn as_db(self) -> &'static str {
        match self {
            ClientKind::Dcr => "dcr",
            ClientKind::Cimd => "cimd",
        }
    }

    pub(crate) fn from_db(kind: &str) -> Result<ClientKind, String> {
        match kind {
            "dcr" => Ok(ClientKind::Dcr),
            "cimd" => Ok(ClientKind::Cimd),
            other => Err(format!("unknown client kind {other:?}")),
        }
    }
}

/// A client an authorize request may name.
pub(crate) struct KnownClient {
    pub client_id: String,
    pub kind: ClientKind,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

/// The stored client `client_id`, or None.
pub(crate) async fn find_in(
    db: &SqlitePool,
    client_id: &str,
) -> Result<Option<KnownClient>, String> {
    let row: Option<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT client_id, kind, client_name, redirect_uris FROM oauth_clients \
         WHERE client_id = ?",
    )
    .bind(client_id)
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?;
    row.map(|(client_id, kind, client_name, redirect_uris)| {
        Ok(KnownClient {
            client_id,
            kind: ClientKind::from_db(&kind)?,
            client_name,
            redirect_uris: redirect_uris_from_db(&redirect_uris)?,
        })
    })
    .transpose()
}

/// A stored JSON array of redirect URIs.
pub(crate) fn redirect_uris_from_db(json: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(json).map_err(|e| format!("stored redirect URIs are unreadable: {e}"))
}

/// A client as `POST /oauth/register` validated it.
pub(crate) struct NewClient {
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum InsertRefusal {
    /// Every registered client has a grant.
    Full,
    Database(String),
}

/// Inserts a DCR client; at `cap` DCR clients, first deletes the one with no
/// grant whose COALESCE(last_used_at, created_at) is oldest (D4), in one
/// BEGIN IMMEDIATE transaction. CIMD clients neither count nor make room.
pub(crate) async fn insert_dcr_in(
    db: &SqlitePool,
    client_id: &str,
    client: &NewClient,
    now: i64,
    cap: i64,
) -> Result<(), InsertRefusal> {
    let database = |e: sqlx::Error| InsertRefusal::Database(e.to_string());
    let redirect_uris =
        serde_json::to_string(&client.redirect_uris).expect("a list of strings serializes");
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await.map_err(database)?;
    let registered: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients WHERE kind = 'dcr'")
            .fetch_one(&mut *tx)
            .await
            .map_err(database)?;
    if registered >= cap {
        let evicted = sqlx::query(
            "DELETE FROM oauth_clients WHERE client_id = ( \
                 SELECT c.client_id FROM oauth_clients c \
                 WHERE c.kind = 'dcr' \
                   AND NOT EXISTS (SELECT 1 FROM oauth_grants g WHERE g.client_id = c.client_id) \
                 ORDER BY COALESCE(c.last_used_at, c.created_at) LIMIT 1)",
        )
        .execute(&mut *tx)
        .await
        .map_err(database)?;
        if evicted.rows_affected() == 0 {
            return Err(InsertRefusal::Full);
        }
    }
    sqlx::query(
        "INSERT INTO oauth_clients (client_id, kind, client_name, redirect_uris, created_at) \
         VALUES (?, 'dcr', ?, ?, ?)",
    )
    .bind(client_id)
    .bind(&client.client_name)
    .bind(redirect_uris)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    tx.commit().await.map_err(database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::test_util::CLAUDE_CALLBACK;
    use crate::db::test_db;

    #[test]
    fn redirect_uris_must_be_https_or_loopback() {
        // https://claude.ai/api/mcp/auth_callback: claude.com connectors/building/authentication.md:211;
        // http://localhost:3118/callback: the same page, :217.
        for ok in [
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost:3118/callback",
            "http://127.0.0.1/callback",
            "http://[::1]:8080/cb",
        ] {
            assert_eq!(redirect_uri_allowed(ok), Ok(()), "{ok}");
        }
        for bad in [
            "http://claude.ai/api/mcp/auth_callback",
            "com.example.app:/callback",
            "https://claude.ai/api/mcp/auth_callback#x",
            "https:///cb",
            "not a uri",
        ] {
            assert!(redirect_uri_allowed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_redirect_uri_not_written_as_scheme_and_host_is_refused() {
        // Each parses, by the WHATWG rules, as https://claude.ai/cb.
        for bad in [
            "https:claude.ai/cb",
            "https:/claude.ai/cb",
            "https:\\\\claude.ai/cb",
            "https://claude.ai\\cb",
            " https://claude.ai/cb",
            "https://claude.ai/\ncb",
            "https://claude.\tai/cb",
        ] {
            assert_eq!(
                Url::parse(bad).unwrap().as_str(),
                "https://claude.ai/cb",
                "{bad:?}"
            );
            assert!(redirect_uri_allowed(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn loopback_redirects_match_on_any_port_and_nothing_else_does() {
        assert!(redirect_matches(
            "http://localhost/callback",
            "http://localhost:3118/callback"
        ));
        assert!(redirect_matches(
            "http://127.0.0.1/callback",
            "http://127.0.0.1:50000/callback"
        ));
        assert!(redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(!redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai:8443/api/mcp/auth_callback"
        ));
        assert!(!redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback?x=1"
        ));
        assert!(!redirect_matches(
            "http://localhost/callback",
            "http://localhost:3118/other"
        ));
        assert!(!redirect_matches(
            "http://localhost/callback",
            "http://127.0.0.1:3118/callback"
        ));
    }

    #[test]
    fn only_the_port_of_a_loopback_redirect_may_differ() {
        assert!(redirect_matches("http://[::1]/cb", "http://[::1]:8080/cb"));
        assert!(redirect_matches(
            "http://localhost:1/cb",
            "http://localhost/cb"
        ));
        for requested in [
            "http://evil.example@localhost:3118/callback",
            "http://LOCALHOST:3118/callback",
            "http://localhost:3118/callback/",
            "http://localhost:99999/callback",
            "https://localhost:3118/callback",
        ] {
            assert!(
                !redirect_matches("http://localhost/callback", requested),
                "{requested}"
            );
        }
    }

    async fn grant(db: &SqlitePool, client_id: &str) {
        // Not INSERT OR IGNORE: the username trigger aborts before OR IGNORE applies.
        sqlx::query(
            "INSERT INTO users (id, username, password_hash) SELECT 'u1', 'bob', 'x' \
             WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 'u1')",
        )
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauth_grants (id, user_id, client_id, resource, scope, created_at) \
             VALUES (?, 'u1', ?, 'https://music.example.com/mcp', 'soulful', 0)",
        )
        .bind(format!("g-{client_id}"))
        .bind(client_id)
        .execute(db)
        .await
        .unwrap();
    }

    async fn client_ids(db: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT client_id FROM oauth_clients ORDER BY client_id")
            .fetch_all(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn at_the_cap_the_client_without_a_grant_used_longest_ago_makes_room() {
        let db = test_db::migrated().await;
        let client = NewClient {
            client_name: None,
            redirect_uris: vec![CLAUDE_CALLBACK.into()],
        };
        for (id, at) in [("dcr_a", 1), ("dcr_b", 2), ("dcr_c", 3)] {
            insert_dcr_in(&db, id, &client, at, 3).await.unwrap();
        }
        // dcr_a registered first but was used after dcr_b was registered; dcr_c is connected.
        sqlx::query("UPDATE oauth_clients SET last_used_at = 10 WHERE client_id = 'dcr_a'")
            .execute(&db)
            .await
            .unwrap();
        grant(&db, "dcr_c").await;
        insert_dcr_in(&db, "dcr_d", &client, 20, 3).await.unwrap();
        assert_eq!(client_ids(&db).await, ["dcr_a", "dcr_c", "dcr_d"]);
        grant(&db, "dcr_a").await;
        grant(&db, "dcr_d").await;
        assert!(matches!(
            insert_dcr_in(&db, "dcr_e", &client, 21, 3).await,
            Err(InsertRefusal::Full)
        ));
        assert_eq!(client_ids(&db).await, ["dcr_a", "dcr_c", "dcr_d"]);
    }

    #[tokio::test]
    async fn the_cap_counts_and_evicts_registered_clients_only() {
        let db = test_db::migrated().await;
        let client = NewClient {
            client_name: None,
            redirect_uris: vec![CLAUDE_CALLBACK.into()],
        };
        // A CIMD client unused since time 0, the oldest row and without a grant.
        sqlx::query(
            "INSERT INTO oauth_clients (client_id, kind, redirect_uris, created_at) \
             VALUES ('https://app.example.com/client.json', 'cimd', '[]', 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        insert_dcr_in(&db, "dcr_a", &client, 1, 2).await.unwrap();
        insert_dcr_in(&db, "dcr_b", &client, 2, 2).await.unwrap();
        insert_dcr_in(&db, "dcr_c", &client, 3, 2).await.unwrap();
        assert_eq!(
            client_ids(&db).await,
            ["dcr_b", "dcr_c", "https://app.example.com/client.json"]
        );
    }
}
