use dioxus::prelude::*;

use ui::Navbar;
use views::Home;

mod views;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
enum Route {
    #[layout(MobileNavbar)]
    #[route("/")]
    Home {},
}

const MAIN_CSS: Asset = asset!("/assets/main.css");

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    // Build cool things ✌️

    rsx! {
        // Global app resources
        document::Link { rel: "stylesheet", href: MAIN_CSS }

        Router::<Route> {}
    }
}

/// A mobile-specific Router around the shared `Navbar` component
/// which allows us to use the mobile-specific `Route` enum.
#[component]
fn MobileNavbar() -> Element {
    rsx! {
        Navbar {
            Link {
                class: "text-gray-300 hover:text-teal-400 hover:bg-white/5 px-3 py-2 rounded-md text-sm font-medium transition-colors",
                to: Route::Home {},
                "Search"
            }
        }

        main { class: "pt-24 pb-12 min-h-screen bg-gray-900",
            div { class: "max-w-7xl mx-auto px-4 sm:px-6 lg:px-8", Outlet::<Route> {} }
        }
    }
}
