use auth::{use_auth, AuthProvider};
use dioxus::prelude::*;

use ui::{Downloads, Navbar, SearchReset};
use views::{LoginPage, SearchPage, SettingsPage};

mod auth;
mod views;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(AuthGuard)]
        #[route("/login")]
        LoginPage {},

        #[layout(WebNavbar)]
            #[route("/")]
            SearchPage {},
            #[route("/settings")]
            SettingsPage {},
}

const FAVICON: Asset = asset!("/assets/favicon.ico");
const MAIN_CSS: Asset = asset!("/assets/tailwind.css");

fn main() {
    #[cfg(feature = "server")]
    {
        use tower_cookies::CookieManagerLayer;

        dioxus::serve(|| async move {
            Ok(dioxus::server::router(App).layer(CookieManagerLayer::new()))
        });
    }

    #[cfg(not(feature = "server"))]
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        document::Title { "SoulBeet" }

        AuthProvider { Router::<Route> {} }
    }
}

#[component]
fn AuthGuard() -> Element {
    let auth = use_auth();
    let nav = use_navigator();
    let current = use_route::<Route>();

    use_effect(move || {
        let is_logged_in = auth.is_logged_in();

        // If not logged in AND we're not already on /login -> go to login
        if !is_logged_in && !matches!(current, Route::LoginPage {}) {
            nav.replace(Route::LoginPage {});
        }

        // If logged in and on /login -> go to home
        if is_logged_in && matches!(current, Route::LoginPage {}) {
            nav.replace(Route::SearchPage {});
        }
    });

    rsx! {
        Outlet::<Route> {}
    }
}

#[component]
fn WebNavbar() -> Element {
    let mut auth = use_auth();
    let mut downloads_open = use_signal(|| false);
    let mut search_reset = use_signal(|| 0);

    use_context_provider(|| SearchReset(search_reset));

    let logout = move |_| {
        spawn(async move {
            auth.logout().await;
        });
    };

    rsx! {
        Navbar {
            Link {
                class: "text-gray-300 hover:text-teal-400 hover:bg-white/5 px-3 py-2 rounded-md text-sm font-medium transition-colors",
                to: Route::SearchPage {},
                onclick: move |_| search_reset += 1,
                "Search"
            }
            Link {
                class: "text-gray-300 hover:text-teal-400 hover:bg-white/5 px-3 py-2 rounded-md text-sm font-medium transition-colors",
                to: Route::SettingsPage {},
                "Settings"
            }
            button {
                class: "text-gray-300 hover:text-teal-400 hover:bg-white/5 px-3 py-2 rounded-md text-sm font-medium transition-colors ml-4",
                onclick: move |_| downloads_open.set(!downloads_open()),
                "Downloads"
            }
            button {
                class: "text-gray-300 hover:text-white hover:bg-red-500/20 px-3 py-2 rounded-md text-sm font-medium transition-colors ml-4",
                onclick: logout,
                "Logout"
            }
        }

        main { class: "pt-24 pb-12 min-h-screen bg-gray-900",
            div { class: "max-w-7xl mx-auto px-4 sm:px-6 lg:px-8", Outlet::<Route> {} }
        }
        Downloads { is_open: downloads_open }
    }
}
