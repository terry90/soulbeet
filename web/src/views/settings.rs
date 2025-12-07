use dioxus::prelude::*;
use ui::settings::{FolderManager, UserManager};

#[component]
pub fn SettingsPage() -> Element {
    rsx! {
        div { class: "space-y-8 text-white",
            div {
                h1 { class: "text-3xl font-bold text-teal-400 mb-2", "Settings" }
                p { class: "text-gray-400", "Manage your library and user preferences." }
            }

            FolderManager {}
            UserManager {}
        }
    }
}
