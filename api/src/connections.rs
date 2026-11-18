//! What Settings shows about the apps connected through OAuth and about the
//! address Soulful derives for its connection paths.

use serde::{Deserialize, Serialize};

/// One request the connection router answered, refusals included.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoggedRequest {
    /// Unix seconds.
    pub at: i64,
    pub method: String,
    /// The path without its query, which can carry codes and state, cut to
    /// 200 characters.
    pub path: String,
    /// The address derived for the request, when it could be.
    pub base: Option<String>,
    /// Why no address could be derived, cut to 200 characters.
    pub base_problem: Option<String>,
    /// Which header or rule gave the host, as `HostSource::as_str` names it.
    pub host_from: String,
    /// Which header or rule gave the scheme, as `SchemeSource::as_str` names it.
    pub scheme_from: String,
    // The headers the address is read from, each cut to 200 characters.
    pub host_header: Option<String>,
    pub x_forwarded_host: Option<String>,
    pub x_forwarded_proto: Option<String>,
    pub forwarded: Option<String>,
    /// None when the client left before the answer.
    pub status: Option<u16>,
    /// The `error` code of a connection layer refusal.
    pub refusal: Option<String>,
}

/// An app a user allowed to use Soulful as them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectedApp {
    pub grant_id: String,
    /// The host the app is known by, as the consent page named it: its
    /// client id URL's, or for a registered client the host of the redirect
    /// URI the user allowed.
    pub app_host: String,
    /// The name the app gave itself.
    pub client_name: Option<String>,
    /// The owner's username when the app is another user's.
    pub owner: Option<String>,
    pub last_used_at: Option<i64>,
    pub allow_delete: bool,
    /// The app is the caller's own.
    pub mine: bool,
}

/// How Soulful sees its own address for the request asking, with advice and
/// the latest connection requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteAccess {
    pub derived_base: Option<String>,
    pub base_problem: Option<String>,
    pub host_from: String,
    pub scheme_from: String,
    pub advice: Vec<String>,
    /// Newest first.
    pub recent: Vec<LoggedRequest>,
}
