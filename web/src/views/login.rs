use std::fmt::{self, Display, Formatter};
use std::future::Future;
use std::pin::Pin;

use api::login;
use dioxus::prelude::*;
use ui::{login_failure_message, Login};

use crate::auth::use_auth;
use crate::Route;

/// The query of `Route::LoginReturnPage`: the authorize request that sent
/// the browser to sign in, exactly as the server encoded it with
/// `shared::oauth::return_to_query`. The router hands `From<&str>` the
/// whole query percent-decoded once, and writes back what `Display` gives
/// with only a few bytes encoded, so `Display` encodes the request itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReturnTo(Option<String>);

impl ReturnTo {
    /// The request, when it is one Soulful may send the browser back to.
    pub(crate) fn accepted(&self) -> Option<&str> {
        self.0
            .as_deref()
            .and_then(shared::oauth::accepted_return_to)
    }
}

impl From<&str> for ReturnTo {
    fn from(decoded_query: &str) -> Self {
        Self(shared::oauth::return_to_in(decoded_query).map(str::to_owned))
    }
}

impl Display for ReturnTo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(back) => f.write_str(&shared::oauth::return_to_query(back)),
            None => Ok(()),
        }
    }
}

/// Leaves the login page once signed in, as `AuthGuard` alone does: to
/// `return_to`, an accepted authorize request, with a full page load so the
/// server answers it, or to search in place of the login page's history
/// entry.
// Only the web build can load another page.
#[cfg_attr(not(feature = "web"), allow(unused_variables))]
pub(crate) fn leave_login(navigator: dioxus::router::Navigator, return_to: Option<&str>) {
    #[cfg(feature = "web")]
    if let (Some(target), Some(window)) = (return_to, web_sys::window()) {
        if window.location().assign(target).is_ok() {
            return;
        }
    }
    navigator.replace(Route::SearchPage {});
}

/// Signs in; `AuthGuard` then sees the session and leaves the page.
#[component]
pub fn LoginPage() -> Element {
    let mut auth = use_auth();

    let login = use_callback(move |(username, password): (String, String)|
        -> Pin<Box<dyn Future<Output = Result<(), String>>>>
    {
        Box::pin(async move {
            match login(username, password).await {
                Ok(response) => {
                    auth.login(response);
                    Ok(())
                }
                Err(e) => Err(login_failure_message(&e)),
            }
        })
    });

    rsx! {
        Login { login }
    }
}

/// The login page on the way to an app's request; `AuthGuard` reads the
/// request from the route.
#[component]
pub fn LoginReturnPage(return_to: ReturnTo) -> Element {
    rsx! {
        LoginPage {}
    }
}
