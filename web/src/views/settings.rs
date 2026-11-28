use api::auth::SsoLinkResult;
use dioxus::prelude::*;
use ui::settings::{
    AccountPanel, AppConfigManager, ConnectedAppsPanel, ConnectionStatusPanel, DeletionChecksPanel,
    FolderAdminPanel, FolderManager, RemoteAccessPanel, SecurityStatusPanel, UserManager,
};
use ui::{IconLibrary, IconSlidersHorizontal};

#[derive(PartialEq, Clone, Copy, Default)]
pub(crate) enum SettingsTab {
    #[default]
    Library,
    Users,
    Config,
}

/// A tab another part of the app wants Settings to open on. The route takes no
/// parameter, so links set this before navigating.
#[derive(Clone, Copy)]
pub(crate) struct SettingsTabRequest(pub Signal<Option<SettingsTab>>);

/// How a single sign-on link that just came back from the provider ended,
/// until Settings shows it.
#[derive(Clone, Copy)]
pub(crate) struct SsoLinkNotice(pub Signal<Option<SsoLinkResult>>);

/// `/settings/sso/<code>`, where the provider's callback ends a link: opens
/// Settings on the account and shows the outcome there.
#[component]
pub fn SsoLinkResultPage(code: String) -> Element {
    let SettingsTabRequest(mut requested_tab) = use_context();
    let SsoLinkNotice(mut notice) = use_context();
    let nav = use_navigator();

    use_effect(move || {
        requested_tab.set(Some(SettingsTab::Users));
        notice.set(Some(SsoLinkResult::from_code(&code)));
        nav.replace(crate::Route::SettingsPage {});
    });

    rsx! {}
}

#[component]
pub fn SettingsPage() -> Element {
    let mut active_tab = use_signal(SettingsTab::default);
    let auth = crate::auth::use_auth();
    let SettingsTabRequest(mut requested_tab) = use_context();
    let SsoLinkNotice(sso_notice) = use_context();

    use_effect(move || {
        if let Some(tab) = requested_tab() {
            active_tab.set(tab);
            requested_tab.set(None);
        }
    });

    rsx! {
        div { class: "w-full max-w-4xl mx-auto space-y-6 text-beet-text-primary",
            header { class: "space-y-1.5",
                h1 { class: "text-[24px] font-semibold tracking-tight text-beet-text-primary leading-tight",
                    "Settings"
                }
                p { class: "text-[13px] text-beet-text-secondary",
                    "Manage library folders, accounts, download backends, and server connections."
                }
            }

            // Segmented Studio tab strip
            nav {
                aria_label: "Settings sections",
                class: "inline-flex items-center gap-1 p-1 rounded-[12px] bg-beet-surface-low border border-beet-border-subtle max-w-full overflow-x-auto no-scrollbar",
                TabButton {
                    label: "Library",
                    tab: SettingsTab::Library,
                    active: active_tab() == SettingsTab::Library,
                    onclick: move |_| active_tab.set(SettingsTab::Library),
                }
                TabButton {
                    label: "Users",
                    tab: SettingsTab::Users,
                    active: active_tab() == SettingsTab::Users,
                    onclick: move |_| active_tab.set(SettingsTab::Users),
                }
                TabButton {
                    label: "Config",
                    tab: SettingsTab::Config,
                    active: active_tab() == SettingsTab::Config,
                    onclick: move |_| active_tab.set(SettingsTab::Config),
                }
            }

            // Tab content
            div { class: "pt-2",
                match active_tab() {
                    SettingsTab::Library => rsx! {
                        div { class: "space-y-6",
                            FolderManager {}
                            DeletionChecksPanel {}
                            if auth.is_admin() {
                                FolderAdminPanel {}
                            }
                        }
                    },
                    SettingsTab::Users => rsx! {
                        div { class: "space-y-6",
                            AccountPanel { sso_notice }
                            ConnectedAppsPanel {}
                            if auth.is_admin() {
                                UserManager {}
                            }
                        }
                    },
                    SettingsTab::Config => rsx! {
                        div { class: "space-y-6",
                            if auth.is_admin() {
                                AppConfigManager {}
                                SecurityStatusPanel {}
                                RemoteAccessPanel {}
                            }
                            ConnectionStatusPanel {}
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn TabButton(
    label: &'static str,
    tab: SettingsTab,
    active: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let state_class = if active {
        "bg-beet-surface-high text-beet-text-primary border-beet-border-default"
    } else {
        "text-beet-text-secondary border-transparent hover:text-beet-text-primary hover:bg-beet-surface/60"
    };

    rsx! {
        button {
            r#type: "button",
            aria_current: active.then_some("page"),
            class: "inline-flex items-center gap-2 h-9 px-3.5 rounded-[8px] border text-[13px] font-medium transition-colors cursor-pointer shrink-0 {state_class}",
            onclick: move |e| onclick.call(e),
            match tab {
                SettingsTab::Library => rsx! { IconLibrary { size: 15 } },
                SettingsTab::Users => rsx! { IconUsers { size: 15 } },
                SettingsTab::Config => rsx! { IconSlidersHorizontal { size: 15 } },
            }
            span { "{label}" }
        }
    }
}

/// Crisp Lucide `users` icon matching `ui::icons` geometry (`1.75` stroke).
#[component]
fn IconUsers(#[props(default = 15)] size: u32) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0",
            path { d: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" }
            circle { cx: "9", cy: "7", r: "4" }
            path { d: "M22 21v-2a4 4 0 0 0-3-3.87" }
            path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_open_on_library() {
        assert!(SettingsTab::default() == SettingsTab::Library);
    }
}
