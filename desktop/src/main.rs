use dioxus::prelude::*;

use ui::{Downloads, Navbar};
use views::Home;

mod views;

#[derive(Debug, Clone, Routable, PartialEq)]
#[rustfmt::skip]
enum Route {
    #[layout(DesktopNavbar)]
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

/// A desktop-specific Router around the shared `Navbar` component
/// which allows us to use the desktop-specific `Route` enum.
#[component]
fn DesktopNavbar() -> Element {
    let mut downloads_open = use_signal(|| false);

    rsx! {
        Navbar {
            Link {
                class: "text-gray-300 hover:text-teal-400 hover:bg-white/5 px-3 py-2 rounded-md text-sm font-medium transition-colors",
                to: Route::Home {},
                "Home"
            }
            button {
                class: "text-gray-300 hover:text-teal-400 hover:bg-white/5 px-3 py-2 rounded-md text-sm font-medium transition-colors ml-4",
                onclick: move |_| downloads_open.set(!downloads_open()),
                "Downloads"
            }
        }

        main { class: "pt-24 pb-12 min-h-screen bg-gray-900",
            div { class: "max-w-7xl mx-auto px-4 sm:px-6 lg:px-8", Outlet::<Route> {} }
        }
        Downloads { is_open: downloads_open }
    }
}
