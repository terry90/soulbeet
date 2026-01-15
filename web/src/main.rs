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
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1" }
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
        let mut backoff_ms: u32 = 1_000;
        const MAX_BACKOFF_MS: u32 = 30_000;

        loop {
            let stream = auth.call(api::download_updates_stream()).await;

            match stream {
                Ok(mut s) => {
                    // Reset backoff on successful connection
                    backoff_ms = 1_000;

                    while let Some(result) = s.next().await {
                        match result {
                            Ok(data) => {
                                let mut map = downloads.write();
                                for file in data {
                                    // Use filename as key for consistent deduplication
                                    // (initial queued entries and slskd responses share the same filename)
                                    map.insert(file.filename.clone(), file);
                                }
                            }
                            Err(e) => {
                                warn!("Error receiving download update: {:?}, reconnecting", e);
                                // Stream is corrupted, break and reconnect
                                break;
                            }
                        }
                    }
                    // Stream ended or errored, wait before reconnecting
                    gloo_timers::future::TimeoutFuture::new(backoff_ms).await;
                }
                Err(e) => {
                    warn!(
                        "Failed to connect to download updates stream: {:?}, retrying in {}ms",
                        e, backoff_ms
                    );
                    gloo_timers::future::TimeoutFuture::new(backoff_ms).await;
                    // Exponential backoff with cap
                    backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
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
                    span { class: "hidden md:block", "Search" }
                    svg {
                        class: "md:hidden w-6 h-6",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
                        }
                    }
                }
                Link {
                    class: "nav-link text-white font-medium border-b-2 border-transparent hover:border-beet-accent pb-0.5",
                    active_class: "border-beet-accent",
                    to: Route::SettingsPage {},
                    span { class: "hidden md:block", "Settings" }
                    svg {
                        class: "md:hidden w-6 h-6",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z",
                        }
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M15 12a3 3 0 11-6 0 3 3 0 016 0z",
                        }
                    }
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

            main { class: "px-4 sm:px-6 lg:px-8 flex-grow flex flex-col relative overflow-y-auto w-full py-8 no-scrollbar",
                Outlet::<Route> {}
            }
            Downloads { is_open: downloads_open, downloads }
        }
    }
}
