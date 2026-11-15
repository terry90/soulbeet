use std::future::Future;
use std::pin::Pin;

use api::login;
use dioxus::prelude::*;
use ui::{login_failure_message, Login};

use crate::auth::use_auth;
use crate::Route;

/// Leaves the login page once signed in: to the authorize request that sent
/// the browser here, with a full page load so the server answers it, or to
/// search. `return_to` is read from `window.location` with the `url` crate,
/// not from the route: the router percent-decodes the whole query before
/// splitting it, which would break an encoded `&` inside `return_to`.
pub(crate) fn leave_login(navigator: dioxus::router::Navigator) {
    #[cfg(feature = "web")]
    if let Some(window) = web_sys::window() {
        let return_to = window
            .location()
            .href()
            .ok()
            .and_then(|href| url::Url::parse(&href).ok())
            .and_then(|url| {
                url.query_pairs()
                    .find(|(name, _)| name == "return_to")
                    .map(|(_, value)| value.into_owned())
            });
        if let Some(target) = return_to
            .as_deref()
            .and_then(shared::oauth::accepted_return_to)
        {
            if window.location().assign(target).is_ok() {
                return;
            }
        }
    }
    navigator.push(Route::SearchPage {});
}

#[component]
pub fn LoginPage() -> Element {
    let navigator = use_navigator();
    let mut auth = use_auth();

    let login = use_callback(move |(username, password): (String, String)|
        -> Pin<Box<dyn Future<Output = Result<(), String>>>>
    {
        Box::pin(async move {
            match login(username, password).await {
                Ok(response) => {
                    auth.login(response);
                    leave_login(navigator);
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
