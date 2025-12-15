use dioxus::prelude::*;
use ui::settings::{FolderManager, UserManager};

#[component]
pub fn SettingsPage() -> Element {
    rsx! {
        div { class: "fixed top-1/4 -left-10 w-64 h-64 bg-beet-accent/10 rounded-full blur-[100px] pointer-events-none" }
        div { class: "fixed bottom-1/4 -right-10 w-64 h-64 bg-beet-leaf/10 rounded-full blur-[100px] pointer-events-none" }

        div { class: "space-y-8 text-white w-full max-w-3xl z-10 mx-auto",
            div { class: "text-center mb-8",
                h1 { class: "text-4xl font-bold text-beet-accent mb-2 font-display",
                    "Settings"
                }
                p { class: "text-gray-400 font-mono", "Manage your library and user preferences." }
            }

            FolderManager {}
            UserManager {}
        }
    }
}
