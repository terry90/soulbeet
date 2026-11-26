//! Adaptive 3-pane Studio Shell chrome: Left Sidebar Rail (240px on `lg+`),
//! Responsive Mobile Bottom Navigation (`< lg`), and Sticky Top Bar (`Ctrl+K`, Offline Pill).

use dioxus::prelude::*;
use shared::recommendation::ContextualMixSummary;

use crate::{
    IconActivity, IconCommand, IconCompass, IconHome, IconLibrary, IconListMusic, IconLogOut,
    IconSearch, IconSettings, IconSparkles, Logo, OfflineTogglePill,
};

use super::format_mix_title;

/// Primary navigation sections of the Refined Midnight Studio shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudioNavSection {
    Home,
    Discover,
    Search,
    Library,
    Activity,
    Settings,
}

impl StudioNavSection {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Discover => "Discover",
            Self::Search => "Search",
            Self::Library => "Library",
            Self::Activity => "Activity",
            Self::Settings => "Settings",
        }
    }

    pub const fn href(self) -> &'static str {
        match self {
            Self::Home => "/",
            Self::Discover => "/discover",
            Self::Search => "/search",
            Self::Library => "/library",
            Self::Activity => "/activity",
            Self::Settings => "/settings",
        }
    }
}

/// Lightweight playlist entry displayed in the Left Rail's scrollable Playlists list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarPlaylistItem {
    pub id: String,
    pub name: String,
    pub song_count: u32,
    pub is_smart: bool,
    pub is_offline_pinned: bool,
}

/// 240px desktop left navigation rail (`surface.sunken` `#0B0312`).
#[component]
pub fn StudioSidebar(
    active: StudioNavSection,
    on_navigate: EventHandler<StudioNavSection>,
    offline_only: bool,
    on_toggle_offline: EventHandler<bool>,
    #[props(default)] active_transfers_count: usize,
    #[props(default)] pinned_seasonal: Option<ContextualMixSummary>,
    #[props(default)] on_open_seasonal: Option<EventHandler<String>>,
    #[props(default)] playlists: Vec<SidebarPlaylistItem>,
    #[props(default)] on_select_playlist: Option<EventHandler<String>>,
    #[props(default)] username: Option<String>,
    #[props(default)] on_whats_new: Option<EventHandler<()>>,
    on_logout: EventHandler<()>,
) -> Element {
    let nav_items = [
        StudioNavSection::Home,
        StudioNavSection::Discover,
        StudioNavSection::Search,
        StudioNavSection::Library,
        StudioNavSection::Activity,
        StudioNavSection::Settings,
    ];

    rsx! {
        aside {
            aria_label: "Primary studio navigation",
            class: "hidden lg:flex flex-col w-[240px] shrink-0 h-full bg-beet-sunken border-r border-beet-border-subtle select-none",

            // Brand Header (32px pixel-art Logo — exact multiple of 16px!)
            div { class: "h-14 px-4 flex items-center gap-3 border-b border-beet-border-subtle shrink-0",
                button {
                    r#type: "button",
                    class: "flex items-center gap-2.5 text-left cursor-pointer group",
                    onclick: move |_| on_navigate.call(StudioNavSection::Home),
                    Logo { size: 32 }
                    div { class: "leading-none",
                        span { class: "block text-[16px] font-bold tracking-tight text-beet-text-primary group-hover:text-beet-leaf-text transition-colors",
                            "Soulbeet"
                        }
                        span { class: "block tabular-nums-mono text-[9px] font-semibold uppercase tracking-[0.14em] text-beet-text-tertiary mt-0.5",
                            "STUDIO"
                        }
                    }
                }
            }

            // Primary Navigation
            nav { class: "p-3 space-y-1 shrink-0", aria_label: "Main sections",
                for section in nav_items {
                    {
                        let is_active = active == section;
                        let item_cls = if is_active {
                            "bg-beet-surface-high text-beet-text-primary border-beet-border-default"
                        } else {
                            "text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-low border-transparent"
                        };
                        let icon_cls = if is_active {
                            "text-beet-leaf-text".to_string()
                        } else {
                            "text-beet-text-tertiary".to_string()
                        };
                        rsx! {
                            a {
                                key: "{section.label()}",
                                href: "{section.href()}",
                                aria_current: if is_active { Some("page") } else { None },
                                class: "w-full h-9 px-3 rounded-[8px] border flex items-center justify-between gap-2 text-[14px] font-medium transition-colors duration-150 cursor-pointer {item_cls}",
                                onclick: move |evt| {
                                    evt.prevent_default();
                                    on_navigate.call(section);
                                },
                                span { class: "flex items-center gap-2.5 min-w-0",
                                    match section {
                                        StudioNavSection::Home => rsx! { IconHome { size: 17, class: icon_cls } },
                                        StudioNavSection::Discover => rsx! { IconCompass { size: 17, class: icon_cls } },
                                        StudioNavSection::Search => rsx! { IconSearch { size: 17, class: icon_cls } },
                                        StudioNavSection::Library => rsx! { IconLibrary { size: 17, class: icon_cls } },
                                        StudioNavSection::Activity => rsx! { IconActivity { size: 17, class: icon_cls } },
                                        StudioNavSection::Settings => rsx! { IconSettings { size: 17, class: icon_cls } },
                                    }
                                    span { class: "truncate", "{section.label()}" }
                                }
                                if section == StudioNavSection::Activity && active_transfers_count > 0 {
                                    span { class: "px-1.5 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/35 text-beet-accent-text tabular-nums-mono text-[10px] font-semibold",
                                        "{active_transfers_count}"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Offline Mode Pill
            div { class: "px-3 pb-3 border-b border-beet-border-subtle shrink-0",
                OfflineTogglePill {
                    offline_only,
                    on_toggle: move |next| on_toggle_offline.call(next),
                    compact: true,
                    class: "w-full justify-center".to_string(),
                }
            }

            // Pinned Seasonal Pack + Scrollable Playlists
            div { class: "flex-1 overflow-y-auto studio-scrollbar px-3 py-3 space-y-4",
                if let Some(seasonal) = pinned_seasonal {
                    {
                        let slug = seasonal.slug.clone();
                        let title = format_mix_title(&seasonal);
                        rsx! {
                            div { class: "space-y-1.5",
                                p { class: "px-2 tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                    "Seasonal"
                                }
                                button {
                                    r#type: "button",
                                    class: "w-full p-2.5 rounded-[10px] bg-beet-accent-surface border border-beet-accent/30 hover:border-beet-accent/55 text-left transition-colors cursor-pointer space-y-1",
                                    onclick: move |_| {
                                        if let Some(cb) = &on_open_seasonal {
                                            cb.call(slug.clone());
                                        } else {
                                            on_navigate.call(StudioNavSection::Discover);
                                        }
                                    },
                                    div { class: "flex items-center gap-1.5 text-beet-accent-text",
                                        IconSparkles { size: 13 }
                                        span { class: "text-[13px] font-semibold truncate", "{title}" }
                                    }
                                    p { class: "tabular-nums-mono text-[10px] text-beet-text-secondary",
                                        "{seasonal.track_count} TRACKS · SEASONAL PACK"
                                    }
                                }
                            }
                        }
                    }
                }

                // Playlists List
                div { class: "space-y-1",
                    p { class: "px-2 tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                        "Playlists"
                    }
                    if playlists.is_empty() {
                        p { class: "px-2 py-2 text-[12px] text-beet-text-disabled",
                            "Connect Navidrome to browse playlists."
                        }
                    } else {
                        for pl in playlists.iter() {
                            {
                                let pid = pl.id.clone();
                                rsx! {
                                    button {
                                        key: "{pl.id}",
                                        r#type: "button",
                                        class: "w-full px-2 py-1.5 rounded-[7px] flex items-center justify-between gap-2 text-left text-[13px] text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-low transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            if let Some(cb) = &on_select_playlist {
                                                cb.call(pid.clone());
                                            } else {
                                                on_navigate.call(StudioNavSection::Library);
                                            }
                                        },
                                        span { class: "flex items-center gap-2 min-w-0",
                                            IconListMusic { size: 14, class: "text-beet-text-tertiary".to_string() }
                                            span { class: "truncate", "{pl.name}" }
                                        }
                                        span { class: "flex items-center gap-1.5 shrink-0",
                                            if pl.is_offline_pinned {
                                                span { class: "w-1.5 h-1.5 rounded-full bg-beet-leaf-glow", title: "Pinned offline" }
                                            }
                                            span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                                "{pl.song_count}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Account Footer
            div { class: "p-3 border-t border-beet-border-subtle flex items-center justify-between gap-2 shrink-0",
                div { class: "min-w-0",
                    p { class: "text-[13px] font-medium text-beet-text-primary truncate",
                        {username.as_deref().unwrap_or("Listener")}
                    }
                    div { class: "flex items-center gap-2",
                        p { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary",
                            "SOULBEET"
                        }
                        if let Some(whats_new_cb) = on_whats_new {
                            button {
                                r#type: "button",
                                class: "tabular-nums-mono text-[10px] text-beet-text-tertiary hover:text-beet-accent-text transition-colors cursor-pointer",
                                onclick: move |_| whats_new_cb.call(()),
                                "What's new"
                            }
                        }
                    }
                }
                button {
                    r#type: "button",
                    aria_label: "Sign out",
                    title: "Sign out",
                    class: "w-8 h-8 rounded-[8px] inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-crimson-text hover:bg-beet-crimson-surface transition-colors cursor-pointer shrink-0",
                    onclick: move |_| on_logout.call(()),
                    IconLogOut { size: 16 }
                }
            }
        }
    }
}

/// Sticky Center-Stage Top Bar with clickable `Ctrl+K` Command Palette trigger,
/// global `OfflineTogglePill`, live Activity badge, and mobile Sign Out access.
#[component]
pub fn StudioTopBar(
    active: StudioNavSection,
    offline_only: bool,
    on_toggle_offline: EventHandler<bool>,
    on_open_palette: EventHandler<()>,
    on_open_downloads: EventHandler<()>,
    #[props(default)] on_downloads_toggle_mounted: Option<EventHandler<std::rc::Rc<MountedData>>>,
    #[props(default)] on_logout: Option<EventHandler<()>>,
    #[props(default)] active_transfers_count: usize,
    #[props(default)] culture_badge: Option<String>,
) -> Element {
    rsx! {
        header { class: "h-14 px-3 sm:px-6 border-b border-beet-border-subtle bg-beet-canvas/90 backdrop-blur-md flex items-center justify-between gap-2 sm:gap-3 shrink-0 z-20 overflow-hidden",
            // Left: Mobile Logo + Section Title (title shown on sm+ so 390px mobile stays uncrowded)
            div { class: "flex items-center gap-2 sm:gap-2.5 shrink-0",
                div { class: "lg:hidden w-8 h-8 shrink-0 flex items-center justify-center",
                    Logo { size: 32, class: "w-8 h-8 shrink-0 block".to_string() }
                }
                span { class: "hidden sm:block text-[17px] font-semibold tracking-[-0.015em] text-beet-text-primary whitespace-nowrap",
                    "{active.label()}"
                }
            }

            // Center: Clickable Command Palette Trigger (`min-w-0 overflow-hidden` prevents overlap on 390px)
            button {
                r#type: "button",
                aria_label: "Open Command Palette",
                class: "flex-1 min-w-0 max-w-md h-9 px-2.5 sm:px-3 rounded-[9px] bg-beet-surface border border-beet-border-default hover:border-beet-border-strong text-beet-text-tertiary hover:text-beet-text-secondary flex items-center justify-between gap-2 overflow-hidden transition-colors cursor-pointer",
                onclick: move |_| on_open_palette.call(()),
                span { class: "flex items-center gap-1.5 sm:gap-2 min-w-0 overflow-hidden text-[13px]",
                    span { class: "shrink-0 inline-flex", IconSearch { size: 15 } }
                    span { class: "sm:hidden truncate", "Search…" }
                    span { class: "hidden sm:inline truncate", "Jump to view, track, or command…" }
                }
                span { class: "hidden sm:inline-flex items-center gap-1 px-1.5 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle tabular-nums-mono text-[10px] font-semibold text-beet-text-secondary shrink-0",
                    IconCommand { size: 10 }
                    "K"
                }
            }

            // Right: Culture Pill, Offline Toggle, Active Transfers, Mobile Sign Out
            div { class: "flex items-center gap-1.5 sm:gap-2 shrink-0",
                if let Some(badge) = culture_badge.as_deref().filter(|b| !b.is_empty()) {
                    span { class: "hidden xl:inline-flex items-center px-2.5 h-8 rounded-full bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[11px] font-semibold text-beet-text-secondary",
                        "{badge}"
                    }
                }

                div { class: "lg:hidden shrink-0",
                    OfflineTogglePill {
                        offline_only,
                        on_toggle: move |next| on_toggle_offline.call(next),
                        compact: true,
                        class: "px-2.5".to_string(),
                    }
                }

                button {
                    r#type: "button",
                    aria_label: "Active transfers drawer",
                    title: "Active transfers & imports",
                    onmounted: move |evt| {
                        if let Some(cb) = &on_downloads_toggle_mounted {
                            cb.call(evt.data());
                        }
                    },
                    class: if active_transfers_count > 0 {
                        "inline-flex items-center justify-center gap-1.5 h-9 w-9 sm:w-auto sm:px-3 rounded-full bg-beet-accent-surface border border-beet-accent/35 hover:border-beet-accent/50 hover:brightness-110 text-beet-accent-text tabular-nums-mono text-[12px] font-semibold cursor-pointer transition-[color,background-color,border-color,filter] shrink-0"
                    } else {
                        "inline-flex items-center justify-center gap-1.5 h-9 w-9 sm:w-auto sm:px-3 rounded-full bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high text-[12px] font-medium cursor-pointer transition-colors shrink-0"
                    },
                    onclick: move |_| on_open_downloads.call(()),
                    IconActivity { size: 15 }
                    span { class: "hidden sm:inline",
                        if active_transfers_count > 0 {
                            "{active_transfers_count} Active"
                        } else {
                            "Transfers"
                        }
                    }
                }

                if let Some(logout_cb) = on_logout {
                    button {
                        r#type: "button",
                        aria_label: "Sign out",
                        title: "Sign out",
                        class: "lg:hidden w-9 h-9 rounded-full inline-flex items-center justify-center bg-beet-surface border border-beet-border-default text-beet-text-tertiary hover:text-beet-crimson-text hover:bg-beet-crimson-surface transition-colors cursor-pointer shrink-0",
                        onclick: move |_| logout_cb.call(()),
                        IconLogOut { size: 16 }
                    }
                }
            }
        }
    }
}

/// Compact mobile bottom navigation bar (`< lg` viewports, including Activity & Settings).
#[component]
pub fn StudioMobileNav(
    active: StudioNavSection,
    on_navigate: EventHandler<StudioNavSection>,
) -> Element {
    let tabs = [
        StudioNavSection::Home,
        StudioNavSection::Discover,
        StudioNavSection::Search,
        StudioNavSection::Library,
        StudioNavSection::Activity,
        StudioNavSection::Settings,
    ];

    rsx! {
        nav {
            aria_label: "Mobile navigation",
            class: "lg:hidden h-14 px-1 bg-beet-sunken border-t border-beet-border-subtle flex items-center justify-around shrink-0 z-30",
            for section in tabs {
                {
                    let is_active = active == section;
                    let cls = if is_active {
                        "text-beet-leaf-text"
                    } else {
                        "text-beet-text-tertiary hover:text-beet-text-primary"
                    };
                    rsx! {
                        a {
                            key: "mob-{section.label()}",
                            href: "{section.href()}",
                            aria_current: if is_active { Some("page") } else { None },
                            class: "flex flex-col items-center justify-center gap-0.5 min-w-[48px] h-12 rounded-[8px] transition-colors cursor-pointer {cls}",
                            onclick: move |evt| {
                                evt.prevent_default();
                                on_navigate.call(section);
                            },
                            match section {
                                StudioNavSection::Home => rsx! { IconHome { size: 17 } },
                                StudioNavSection::Discover => rsx! { IconCompass { size: 17 } },
                                StudioNavSection::Search => rsx! { IconSearch { size: 17 } },
                                StudioNavSection::Library => rsx! { IconLibrary { size: 17 } },
                                StudioNavSection::Activity => rsx! { IconActivity { size: 17 } },
                                StudioNavSection::Settings => rsx! { IconSettings { size: 17 } },
                            }
                            span { class: "text-[10px] font-medium", "{section.label()}" }
                        }
                    }
                }
            }
        }
    }
}
