//! Registered OAuth clients and the rules their redirect URIs follow.

use serde_json::{Map, Value};
use sqlx::{SqliteExecutor, SqlitePool};
use url::Url;

use super::cimd::ClientDocument;
use crate::base_url::is_loopback_host;

/// Most clients registered through `POST /oauth/register` (D4).
pub(crate) const DCR_CAP: i64 = 100;

const REDIRECT_REFUSED: &str =
    "redirect URIs must be https, or http on a loopback address, with no fragment";

/// The most redirect URIs a client may name, and their longest length.
const MAX_REDIRECT_URIS: usize = 10;
const MAX_REDIRECT_URI_CHARS: usize = 2_000;
/// The longest `client_name`, which the consent page and Settings show.
const MAX_CLIENT_NAME_CHARS: usize = 200;

/// A client metadata member that breaks a rule, and the rule.
#[derive(Debug, PartialEq)]
pub(crate) enum MetadataRefusal {
    ClientName(&'static str),
    RedirectUris(&'static str),
}

/// The name and redirect URIs of a client's metadata, from a registration
/// or a Client ID Metadata Document alike: `client_name`, when present and
/// not empty, a string of at most 200 characters; `redirect_uris`, 1 to 10
/// strings of at most 2,000 characters, each one `redirect_uri_allowed`
/// accepts.
pub(crate) fn client_metadata(doc: &Map<String, Value>) -> Result<NewClient, MetadataRefusal> {
    let uris: Option<Vec<&str>> = doc
        .get("redirect_uris")
        .and_then(Value::as_array)
        .filter(|uris| (1..=MAX_REDIRECT_URIS).contains(&uris.len()))
        .and_then(|uris| uris.iter().map(Value::as_str).collect());
    let Some(uris) = uris.filter(|uris| {
        uris.iter()
            .all(|uri| uri.chars().count() <= MAX_REDIRECT_URI_CHARS)
    }) else {
        return Err(MetadataRefusal::RedirectUris(
            "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
        ));
    };
    for uri in &uris {
        redirect_uri_allowed(uri).map_err(MetadataRefusal::RedirectUris)?;
    }
    let client_name = match doc.get("client_name") {
        None => None,
        Some(Value::String(name)) if name.is_empty() => None,
        Some(Value::String(name)) if name.chars().count() <= MAX_CLIENT_NAME_CHARS => {
            Some(name.clone())
        }
        Some(Value::String(_)) => {
            return Err(MetadataRefusal::ClientName(
                "client_name must be at most 200 characters",
            ))
        }
        Some(_) => return Err(MetadataRefusal::ClientName("client_name must be a string")),
    };
    Ok(NewClient {
        client_name,
        redirect_uris: uris.into_iter().map(str::to_owned).collect(),
    })
}

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
/// grant, code or consent request whose COALESCE(last_used_at, created_at)
/// is oldest (D4), as the prune keeps them, in one BEGIN IMMEDIATE
/// transaction. CIMD clients neither count nor make room.
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
                   AND NOT EXISTS (SELECT 1 FROM oauth_codes o WHERE o.client_id = c.client_id) \
                   AND NOT EXISTS (SELECT 1 FROM oauth_requests r WHERE r.client_id = c.client_id) \
                 ORDER BY COALESCE(c.last_used_at, c.created_at), c.client_id LIMIT 1)",
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

/// Stores a CIMD client as its document reads now: inserted at its first
/// allowed authorization, its name and redirect URIs replaced at each later
/// one.
pub(crate) async fn upsert_cimd_in<'e>(
    db: impl SqliteExecutor<'e>,
    doc: &ClientDocument,
    now: i64,
) -> Result<(), String> {
    let redirect_uris =
        serde_json::to_string(&doc.redirect_uris).expect("a list of strings serializes");
    sqlx::query(
        "INSERT INTO oauth_clients (client_id, kind, client_name, redirect_uris, created_at) \
         VALUES (?, 'cimd', ?, ?, ?) \
         ON CONFLICT (client_id) DO UPDATE SET client_name = excluded.client_name, \
         redirect_uris = excluded.redirect_uris",
    )
    .bind(&doc.client_id)
    .bind(&doc.client_name)
    .bind(redirect_uris)
    .bind(now)
    .execute(db)
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::connection::test_util::{behind_a_writer, CLAUDE_CALLBACK};
    use crate::db::test_db;

    fn metadata(doc: Value) -> Result<NewClient, MetadataRefusal> {
        client_metadata(doc.as_object().unwrap())
    }

    #[test]
    fn client_metadata_names_the_rule_it_breaks() {
        let long = format!("https://claude.ai/{}", "a".repeat(2000));
        let eleven: Vec<String> = (0..11).map(|i| format!("https://a.example/{i}")).collect();
        for (doc, refused) in [
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK], "client_name": "c".repeat(201)}),
                MetadataRefusal::ClientName("client_name must be at most 200 characters"),
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK], "client_name": 7}),
                MetadataRefusal::ClientName("client_name must be a string"),
            ),
            (
                json!({"redirect_uris": []}),
                MetadataRefusal::RedirectUris(
                    "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
                ),
            ),
            (
                json!({}),
                MetadataRefusal::RedirectUris(
                    "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
                ),
            ),
            (
                json!({"redirect_uris": eleven}),
                MetadataRefusal::RedirectUris(
                    "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
                ),
            ),
            (
                json!({"redirect_uris": [long]}),
                MetadataRefusal::RedirectUris(
                    "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
                ),
            ),
            (
                json!({"redirect_uris": [CLAUDE_CALLBACK, 7]}),
                MetadataRefusal::RedirectUris(
                    "redirect_uris must list 1 to 10 URIs of at most 2,000 characters",
                ),
            ),
            (
                json!({"redirect_uris": ["http://claude.ai/api/mcp/auth_callback"]}),
                MetadataRefusal::RedirectUris(REDIRECT_REFUSED),
            ),
            (
                json!({"redirect_uris": ["https://claude.ai/cb#x"]}),
                MetadataRefusal::RedirectUris(REDIRECT_REFUSED),
            ),
        ] {
            assert_eq!(metadata(doc.clone()).err(), Some(refused), "{doc}");
        }
        let ok = metadata(json!({"redirect_uris": [CLAUDE_CALLBACK], "client_name": ""})).unwrap();
        assert_eq!(
            (ok.client_name, ok.redirect_uris),
            (None, vec![CLAUDE_CALLBACK.to_string()])
        );
    }

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
            "INSERT INTO oauth_grants (id, user_id, client_id, redirect_uri, resource, scope, created_at) \
             VALUES (?, 'u1', ?, 'https://claude.ai/api/mcp/auth_callback', \
             'https://music.example.com/mcp', 'soulful', 0)",
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
    async fn at_the_cap_a_client_signing_in_is_kept_and_ties_go_by_client_id() {
        let db = test_db::migrated().await;
        let client = NewClient {
            client_name: None,
            redirect_uris: vec![CLAUDE_CALLBACK.into()],
        };
        // Three clients registered in the same second.
        for id in ["dcr_c", "dcr_a", "dcr_b"] {
            insert_dcr_in(&db, id, &client, 5, 3).await.unwrap();
        }
        // dcr_a has a code out and dcr_b a consent request waiting.
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1', 'bob', 'x')")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO oauth_codes (code_hash, user_id, client_id, redirect_uri, code_challenge, \
             scope, resource, expires_at) VALUES ('h', 'u1', 'dcr_a', ?, 'x', 'soulful', 'r', 99)",
        )
        .bind(CLAUDE_CALLBACK)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO oauth_requests (id_hash, user_id, client_id, client_kind, \
             client_redirect_uris, redirect_uri, code_challenge, scope, resource, expires_at) \
             VALUES ('q', 'u1', 'dcr_b', 'dcr', '[]', ?, 'x', 'soulful', 'r', 99)",
        )
        .bind(CLAUDE_CALLBACK)
        .execute(&db)
        .await
        .unwrap();
        insert_dcr_in(&db, "dcr_d", &client, 6, 3).await.unwrap();
        assert_eq!(client_ids(&db).await, ["dcr_a", "dcr_b", "dcr_d"]);
        // With none signing in, the tie goes to the smallest client id.
        sqlx::query("DELETE FROM oauth_codes")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM oauth_requests")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE oauth_clients SET created_at = 5")
            .execute(&db)
            .await
            .unwrap();
        insert_dcr_in(&db, "dcr_e", &client, 7, 3).await.unwrap();
        assert_eq!(client_ids(&db).await, ["dcr_b", "dcr_d", "dcr_e"]);
    }

    // The count is read before the insert: begun with BEGIN IMMEDIATE the
    // registration waits for another writer, a plain BEGIN fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_registration_waits_for_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db::migrated_file(dir.path()).await;
        let registered = behind_a_writer(&db, {
            let db = db.clone();
            async move {
                let client = NewClient {
                    client_name: None,
                    redirect_uris: vec![CLAUDE_CALLBACK.into()],
                };
                insert_dcr_in(&db, "dcr_a", &client, 0, DCR_CAP)
                    .await
                    .map_err(|e| format!("{e:?}"))
            }
        })
        .await;
        assert_eq!(registered, Ok(()));
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
