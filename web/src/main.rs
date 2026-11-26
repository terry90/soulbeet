use auth::{use_auth, AuthProvider};
use dioxus::prelude::*;
#[cfg(test)]
use shared::system::NavidromeStatus;

use ui::{HealthProvider, SettingsProvider};
#[cfg(test)]
use views::navidrome_banner;
use views::{
    leave_login, ActivityPage, DashboardPage, DiscoverPage, HomePage, LibraryPage, LoginErrorPage,
    LoginErrorReturnPage, LoginPage, LoginReturnPage, ReturnTo, SearchPage, SettingsPage,
    SsoLinkResultPage, StudioShell,
};

mod auth;
#[cfg(feature = "server")]
mod compression;
mod fonts;
mod navidrome_web;
mod shortcuts;
mod views;
mod web_player;
#[cfg(feature = "web")]
mod websocket;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(AuthGuard)]
        #[route("/login")]
        LoginPage {},
        #[route("/login/return?:..return_to")]
        LoginReturnPage { return_to: ReturnTo },
        #[route("/login/error/:code")]
        LoginErrorPage { code: String },
        #[route("/login/error/:code/return?:..return_to")]
        LoginErrorReturnPage { code: String, return_to: ReturnTo },

        #[layout(StudioShell)]
            #[route("/")]
            HomePage {},
            #[route("/discover")]
            DiscoverPage {},
            #[route("/search")]
            SearchPage {},
            #[route("/library")]
            LibraryPage {},
            #[route("/activity")]
            ActivityPage {},
            #[route("/dashboard")]
            DashboardPage {},
            #[route("/settings")]
            SettingsPage {},
            #[route("/settings/sso/:code")]
            SsoLinkResultPage { code: String },
}

const FAVICON: Asset = asset!("/assets/favicon.ico");
const MAIN_CSS: Asset = asset!("/assets/tailwind.css");

fn main() {
    #[cfg(feature = "server")]
    {
        use tower_cookies::CookieManagerLayer;

        dioxus::serve(|| async move {
            // Nothing may sign a session or read a stored password without it.
            if let Err(e) = api::secret_key::load() {
                dioxus::logger::tracing::error!("{e}");
                std::process::exit(1);
            }
            api::services::store_passwords_under_the_current_key().await;
            api::ledger::start_server_ledger();
            api::core::playlists::fill::start();
            api::core::index::start();
            // Reads the unfinished jobs before the router below takes a request.
            api::core::reconcile::start().await;
            // Start background cleanup task for user channels
            api::globals::start_channel_cleanup_task();
            api::oauth::start_prune_task();
            // Check the beets config in the background; never awaited here.
            api::importer_health::start_startup_probe();
            // Drops the beets items whose file is gone, in the background.
            api::import_queue::start_library_prune();

            api::security::check_seeded_admin().await;
            api::folder_policy::log_folder_conflicts().await;
            api::models::user::log_username_collisions().await;
            if let Some(problem) = api::config::CONFIG.navidrome_music_path_problem() {
                dioxus::logger::tracing::warn!("{problem}");
            }
            for reason in api::security::gate_reasons() {
                dioxus::logger::tracing::warn!("MCP stays off: {}", reason.message);
            }

            // Only the app's pages get the framing header: the consent
            // page sets its own policy.
            Ok(compression::compress(
                dioxus::server::router(App)
                    .layer(axum::middleware::map_response(
                        api::connection::deny_framing,
                    ))
                    .merge(api::connection::router(
                        api::connection::ConnectionState::live(),
                    ))
                    .merge(api::oidc::router())
                    .merge(api::healthz::router())
                    .layer(CookieManagerLayer::new()),
            ))
        });
    }

    #[cfg(not(feature = "server"))]
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    rsx! {
        // Browsers that take SVG icons use the logo; the .ico (rendered from
        // it) covers the rest.
        document::Link { rel: "icon", href: FAVICON, sizes: "16x16 32x32 48x48 256x256" }
        document::Link { rel: "icon", href: ui::LOGO, r#type: "image/svg+xml" }
        fonts::Fonts {}
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1" }
        document::Title { "SoulBeet Studio" }

        AuthProvider {
            SettingsProvider {
                HealthProvider {
                    Router::<Route> {}
                }
            }
        }
    }
}

#[component]
fn AuthGuard() -> Element {
    let auth = use_auth();
    let nav = use_navigator();
    let current = use_route::<Route>();

    use_effect(move || match (auth.is_logged_in(), &current) {
        (
            false,
            Route::LoginPage {}
            | Route::LoginReturnPage { .. }
            | Route::LoginErrorPage { .. }
            | Route::LoginErrorReturnPage { .. },
        ) => {}
        (false, _) => {
            nav.replace(Route::LoginPage {});
        }
        (true, Route::LoginPage {} | Route::LoginErrorPage { .. }) => leave_login(nav, None),
        // Back to the request that asked for sign-in
        (
            true,
            Route::LoginReturnPage { return_to } | Route::LoginErrorReturnPage { return_to, .. },
        ) => leave_login(nav, return_to.accepted()),
        (true, _) => {}
    });

    rsx! {
        Outlet::<Route> {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENT: &str =
        "/login/return?return_to=%2Foauth%2Fauthorize%3Fclient_id%3Ddcr_e2e%26state%3Da%2526b";
    const BACK: &str = "/oauth/authorize?client_id=dcr_e2e&state=a%26b";

    #[test]
    fn a_refused_or_missing_navidrome_password_leads_to_connect_navidrome() {
        for status in [
            NavidromeStatus::InvalidCredentials,
            NavidromeStatus::NotLinked,
        ] {
            let (_, message, connect) = navidrome_banner(status).unwrap();
            assert!(connect, "{status:?}");
            assert!(message.contains("Connect Navidrome"), "{message}");
        }
        assert!(!navidrome_banner(NavidromeStatus::Offline).unwrap().2);
        assert_eq!(navidrome_banner(NavidromeStatus::Connected), None);
    }

    fn return_to_of(route: &Route) -> Option<&str> {
        match route {
            Route::LoginReturnPage { return_to } => return_to.accepted(),
            other => panic!("not the login return page: {other:?}"),
        }
    }

    #[test]
    fn the_login_return_route_carries_the_request_exactly() {
        let route: Route = SENT.parse().unwrap();
        assert_eq!(return_to_of(&route), Some(BACK));
        // The router replaces the address with this when it differs, so it
        // must keep the request as sent.
        assert_eq!(route.to_string(), SENT);
        assert_eq!(
            route.to_string(),
            shared::oauth::login_return_location(BACK)
        );
    }

    #[test]
    fn a_request_off_the_authorize_path_is_carried_but_refused() {
        let sent = shared::oauth::login_return_location("//evil.example/oauth/authorize");
        let route: Route = sent.parse().unwrap();
        assert_eq!(return_to_of(&route), None);
        assert_eq!(route.to_string(), sent);
    }

    #[test]
    fn a_single_sign_on_link_comes_back_to_its_result_page() {
        use api::auth::SsoLinkResult;
        let route: Route = SsoLinkResult::IdentityTaken.location().parse().unwrap();
        assert_eq!(
            route,
            Route::SsoLinkResultPage {
                code: "identity_taken".into()
            }
        );
    }

    #[test]
    fn a_failed_single_sign_on_keeps_the_request_it_was_for() {
        use api::auth::SsoError;
        let sent = SsoError::Denied.location(Some(BACK));
        let route: Route = sent.parse().unwrap();
        match &route {
            Route::LoginErrorReturnPage { code, return_to } => {
                assert_eq!(code, "denied");
                assert_eq!(return_to.accepted(), Some(BACK));
            }
            other => panic!("not the error page with a request: {other:?}"),
        }
        assert_eq!(route.to_string(), sent);

        // A crafted error address cannot send the browser off the
        // authorize path once signed in.
        let crafted = SsoError::Failed.location(Some("//evil.example/oauth/authorize"));
        match crafted.parse::<Route>().unwrap() {
            Route::LoginErrorReturnPage { return_to, .. } => assert_eq!(return_to.accepted(), None),
            other => panic!("not the error page with a request: {other:?}"),
        }

        // Without one, the error page keeps its bare address.
        let sent = SsoError::Denied.location(None);
        let route: Route = sent.parse().unwrap();
        assert_eq!(
            route,
            Route::LoginErrorPage {
                code: "denied".into()
            }
        );
        assert_eq!(route.to_string(), sent);
    }

    #[test]
    fn the_plain_login_route_keeps_its_bare_address() {
        let route: Route = "/login".parse().unwrap();
        assert_eq!(route, Route::LoginPage {});
        assert_eq!(route.to_string(), "/login");
    }

    #[test]
    fn studio_routes_parse_and_roundtrip_cleanly() {
        for (path, expected) in [
            ("/", Route::HomePage {}),
            ("/discover", Route::DiscoverPage {}),
            ("/search", Route::SearchPage {}),
            ("/library", Route::LibraryPage {}),
            ("/activity", Route::ActivityPage {}),
            ("/dashboard", Route::DashboardPage {}),
            ("/settings", Route::SettingsPage {}),
        ] {
            let parsed: Route = path.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), path);
        }
    }
}
