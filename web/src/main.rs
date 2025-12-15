use auth::{use_auth, AuthProvider};
use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use shared::slskd::FileEntry;
use std::collections::HashMap;

use ui::{Downloads, Layout, Navbar, SearchReset};
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
    let mut downloads = use_signal::<HashMap<String, FileEntry>>(HashMap::new);

    use_context_provider(|| SearchReset(search_reset));

    use_future(move || async move {
        loop {
            let stream = auth.call(api::download_updates_stream()).await;

            match stream {
                Ok(mut s) => {
                    while let Some(Ok(data)) = s.next().await {
                        let mut map = downloads.write();
                        for file in data {
                            map.insert(file.id.clone(), file);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to connect to download updates stream: {:?}", e);
                    gloo_timers::future::TimeoutFuture::new(1_000).await;
                }
            }
        }
    });

    let logout = move |_| {
        spawn(async move {
            auth.logout().await;
        });
    };

    rsx! {
        Layout {
            Navbar {
                Link {
                    class: "nav-link text-white font-medium border-b-2 border-transparent hover:border-beet-accent pb-0.5",
                    active_class: "border-beet-accent",
                    to: Route::SearchPage {},
                    onclick: move |_| search_reset += 1,
                    "Search"
                }
                Link {
                    class: "nav-link text-white font-medium border-b-2 border-transparent hover:border-beet-accent pb-0.5",
                    active_class: "border-beet-accent",
                    to: Route::SettingsPage {},
                    "Settings"
                }

                // Separator
                div { class: "h-4 w-px bg-white/10" }

                // Downloads Toggle
                button {
                    class: "relative group p-2 hover:bg-white/5 rounded-lg transition-colors focus:outline-none cursor-pointer",
                    onclick: move |_| downloads_open.set(!downloads_open()),
                    svg {
                        class: "w-5 h-5 text-gray-300 group-hover:text-beet-leaf transition-colors",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4",
                        }
                    }

                    if !downloads.read().is_empty() {
                        span { class: "absolute top-1.5 right-1.5 flex h-2.5 w-2.5",
                            span { class: "animate-ping absolute inline-flex h-full w-full rounded-full bg-beet-accent opacity-75" }
                            span { class: "relative inline-flex rounded-full h-2.5 w-2.5 bg-beet-accent" }
                        }
                    }
                }

                button {
                    class: "nav-link text-red-400 hover:text-red-300 text-xs uppercase tracking-widest font-mono cursor-pointer",
                    onclick: logout,
                    "Logout"
                }
            }

            main { class: "flex-grow flex flex-col relative overflow-y-auto w-full py-8 no-scrollbar",
                Outlet::<Route> {}
            }
            Downloads { is_open: downloads_open, downloads }
        }
    }
}
