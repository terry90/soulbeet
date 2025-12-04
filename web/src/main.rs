use auth::{use_auth, AuthProvider};
use dioxus::prelude::*;

use ui::Navbar;
use views::{Home, Login, Settings};

mod auth;
mod views;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[route("/login")]
    Login {},

    #[layout(AuthGuard)]
        #[layout(WebNavbar)]
            #[route("/")]
            Home {},
            #[route("/settings")]
            Settings {},
}

const FAVICON: Asset = asset!("/assets/favicon.ico");
const MAIN_CSS: Asset = asset!("/assets/tailwind.css");

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        document::Link { rel: "stylesheet", href: MAIN_CSS }

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
        if !is_logged_in && !matches!(current, Route::Login {}) {
            nav.replace(Route::Login {});
        }

        // If logged in and on /login → go to home
        if is_logged_in && matches!(current, Route::Login {}) {
            nav.replace(Route::Home {});
        }
    });

    rsx! {
        Outlet::<Route> {}
    }
}

#[component]
fn WebNavbar() -> Element {
    let mut auth = use_auth();
    let nav = use_navigator();

    let logout = move |_| {
        auth.logout();
        nav.replace(Route::Login {});
    };

    rsx! {
        Navbar {
            Link { to: Route::Home {}, "Home" }
            Link { to: Route::Settings {}, "Settings" }
            button {
                class: "text-gray-300 hover:text-white px-3 py-2 rounded-md text-sm font-medium",
                onclick: logout,
                "Logout"
            }
        }

        Outlet::<Route> {}
    }
}
