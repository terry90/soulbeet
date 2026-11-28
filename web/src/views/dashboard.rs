//! Legacy `/dashboard` route redirected cleanly to `/library`.

use dioxus::prelude::*;

use crate::Route;

#[component]
pub fn DashboardPage() -> Element {
    let nav = use_navigator();

    use_effect(move || {
        nav.replace(Route::LibraryPage {});
    });

    rsx! {}
}
