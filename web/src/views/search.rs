use dioxus::prelude::*;
use ui::Search;

#[component]
pub fn SearchPage() -> Element {
    rsx! {
        Search {}
    }
}
