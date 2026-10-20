use dioxus::prelude::*;
use ui::settings::{AppConfigManager, FolderManager, PreferencesManager, UserManager};

#[derive(PartialEq, Clone, Copy, Default)]
pub(crate) enum SettingsTab {
    #[default]
    Search,
    Library,
    Users,
    Config,
}

/// A tab another part of the app wants Settings to open on. The route takes no
/// parameter, so links set this before navigating.
#[derive(Clone, Copy)]
pub(crate) struct SettingsTabRequest(pub Signal<Option<SettingsTab>>);

#[component]
pub fn SettingsPage() -> Element {
    let mut active_tab = use_signal(SettingsTab::default);
    let SettingsTabRequest(mut requested_tab) = use_context();

    use_effect(move || {
        if let Some(tab) = requested_tab() {
            active_tab.set(tab);
            requested_tab.set(None);
        }
    });

    rsx! {
        div { class: "fixed top-1/4 -left-10 w-64 h-64 bg-beet-accent/10 rounded-full blur-[100px] pointer-events-none" }
        div { class: "fixed bottom-1/4 -right-10 w-64 h-64 bg-beet-leaf/10 rounded-full blur-[100px] pointer-events-none" }

        div { class: "space-y-6 text-white w-full max-w-3xl z-10 mx-auto",
            div { class: "text-center mb-6",
                h1 { class: "text-4xl font-bold text-beet-accent mb-2 font-display",
                    "Settings"
                }
            }

            // Tab navigation - pill style matching navbar
            nav { class: "flex items-center justify-center gap-1 bg-beet-panel/50 p-1.5 rounded-full border border-white/5 backdrop-blur-sm w-fit mx-auto",
                TabButton {
                    label: "Search",
                    icon_path: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
                    active: active_tab() == SettingsTab::Search,
                    onclick: move |_| active_tab.set(SettingsTab::Search),
                }
                TabButton {
                    label: "Library",
                    icon_path: "M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z",
                    active: active_tab() == SettingsTab::Library,
                    onclick: move |_| active_tab.set(SettingsTab::Library),
                }
                TabButton {
                    label: "Users",
                    icon_path: "M12 4.354a4 4 0 110 5.292M15 21H3v-1a6 6 0 0112 0v1zm0 0h6v-1a6 6 0 00-9-5.197M13 7a4 4 0 11-8 0 4 4 0 018 0z",
                    active: active_tab() == SettingsTab::Users,
                    onclick: move |_| active_tab.set(SettingsTab::Users),
                }
                TabButton {
                    label: "Config",
                    icon_path: "M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z M15 12a3 3 0 11-6 0 3 3 0 016 0z",
                    active: active_tab() == SettingsTab::Config,
                    onclick: move |_| active_tab.set(SettingsTab::Config),
                }
            }

            // Tab content
            div { class: "pt-8",
                match active_tab() {
                    SettingsTab::Search => rsx! { PreferencesManager {} },
                    SettingsTab::Library => rsx! { FolderManager {} },
                    SettingsTab::Users => rsx! { UserManager {} },
                    SettingsTab::Config => rsx! { AppConfigManager {} },
                }
            }
        }
    }
}

#[component]
fn TabButton(
    label: &'static str,
    icon_path: &'static str,
    active: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = if active {
        "flex items-center gap-2 px-4 py-2 rounded-full bg-white/10 text-white text-sm font-medium transition-all cursor-pointer"
    } else {
        "flex items-center gap-2 px-4 py-2 rounded-full text-gray-400 text-sm font-medium hover:text-white hover:bg-white/5 transition-all cursor-pointer"
    };

    rsx! {
        button {
            class,
            onclick: move |e| onclick.call(e),
            svg {
                class: "w-4 h-4",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                view_box: "0 0 24 24",
                path { d: icon_path }
            }
            span { "{label}" }
        }
    }
}
