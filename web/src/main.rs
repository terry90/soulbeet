use auth::{use_auth, AuthProvider};
use dioxus::prelude::*;
use shared::download::DownloadProgress;
use shared::system::NavidromeStatus;
use std::collections::HashMap;

#[cfg(feature = "web")]
use dioxus::fullstack::WebSocketOptions;
#[cfg(feature = "web")]
use websocket::use_resilient_websocket;

use ui::{Downloads, Layout, Navbar, SearchPrefill, SearchReset, SettingsProvider};
use views::{DashboardPage, LoginPage, SearchPage, SettingsPage};

mod auth;
mod views;
#[cfg(feature = "web")]
mod websocket;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(AuthGuard)]
        #[route("/login")]
        LoginPage {},

        #[layout(WebNavbar)]
            #[route("/")]
            SearchPage {},
            #[route("/dashboard")]
            DashboardPage {},
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
            // Start background cleanup task for user channels
            api::globals::start_channel_cleanup_task();

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

        AuthProvider {
            SettingsProvider {
                Router::<Route> {}
            }
        }
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
    #[allow(unused_mut)] // mutated in websocket callback (web feature only)
    let mut downloads = use_signal::<HashMap<String, DownloadProgress>>(HashMap::new);

    let search_prefill = use_signal(|| None::<(String, String)>);
    use_context_provider(|| SearchReset(search_reset));
    use_context_provider(|| SearchPrefill(search_prefill));

    #[cfg(feature = "web")]
    use_resilient_websocket(
        || api::download_updates_ws(WebSocketOptions::new()),
        move |data: Vec<DownloadProgress>| {
            let mut map = downloads.write();
            for file in data {
                map.insert(file.item.clone(), file);
            }
        },
    );

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
                    to: Route::DashboardPage {},
                    span { class: "hidden md:block", "Dashboard" }
                    svg {
                        class: "md:hidden w-6 h-6",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        stroke_width: "1.5",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            d: "M3 13.125C3 12.504 3.504 12 4.125 12h2.25c.621 0 1.125.504 1.125 1.125v6.75C7.5 20.496 6.996 21 6.375 21h-2.25A1.125 1.125 0 013 19.875v-6.75zM9.75 8.625c0-.621.504-1.125 1.125-1.125h2.25c.621 0 1.125.504 1.125 1.125v11.25c0 .621-.504 1.125-1.125 1.125h-2.25a1.125 1.125 0 01-1.125-1.125V8.625zM16.5 4.125c0-.621.504-1.125 1.125-1.125h2.25C20.496 3 21 3.504 21 4.125v15.75c0 .621-.504 1.125-1.125 1.125h-2.25a1.125 1.125 0 01-1.125-1.125V4.125z",
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

            NavidromeBanner {}

            main { class: "px-4 sm:px-6 lg:px-8 flex-grow flex flex-col relative overflow-y-auto w-full py-8 no-scrollbar",
                Outlet::<Route> {}
            }
            Downloads { is_open: downloads_open, downloads }
        }
    }
}

#[component]
fn NavidromeBanner() -> Element {
    let auth = use_auth();
    let mut settings = ui::use_settings();
    let status = auth.navidrome_status();

    let dismissed = settings
        .get()
        .map(|s| s.navidrome_banner_dismissed)
        .unwrap_or(false);

    if dismissed
        || matches!(
            status,
            NavidromeStatus::Connected | NavidromeStatus::Unknown
        )
    {
        return rsx! {};
    }

    let (dot_color, label, message) = match status {
        NavidromeStatus::InvalidCredentials => (
            "bg-beet-accent",
            "NAVIDROME",
            "Credentials mismatch. Log in with your Navidrome password to reconnect.",
        ),
        NavidromeStatus::Offline => (
            "bg-gray-500",
            "NAVIDROME",
            "Unreachable. Discovery and rating sync paused.",
        ),
        NavidromeStatus::MissingReportRealPath => (
            "bg-yellow-500",
            "NAVIDROME",
            "ReportRealPath is not enabled on the Soulbeet player. Path features and auto-delete won't work. Enable it in Navidrome > Settings > Players.",
        ),
        _ => return rsx! {},
    };

    let dismiss = move |_| {
        spawn(async move {
            let _ = settings
                .update(api::UpdateUserSettings {
                    navidrome_banner_dismissed: Some(true),
                    ..Default::default()
                })
                .await;
        });
    };

    rsx! {
        div { class: "mx-4 sm:mx-6 lg:mx-8 mt-2 px-3 py-2 bg-beet-panel border border-white/10 rounded flex items-center gap-3 text-xs font-mono",
            span { class: format!("w-1.5 h-1.5 rounded-full {} shrink-0", dot_color) }
            span { class: "text-gray-500 uppercase tracking-widest shrink-0 hidden sm:inline", "{label}" }
            span { class: "text-gray-400 flex-1 min-w-0 truncate", "{message}" }
            button {
                class: "shrink-0 text-gray-600 hover:text-gray-300 transition-colors cursor-pointer p-2 -mr-1",
                onclick: dismiss,
                svg {
                    class: "w-3.5 h-3.5",
                    fill: "none",
                    stroke: "currentColor",
                    view_box: "0 0 24 24",
                    path {
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        stroke_width: "2",
                        d: "M6 18L18 6M6 6l12 12",
                    }
                }
            }
        }
    }
}
