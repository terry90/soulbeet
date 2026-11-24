use dioxus::prelude::*;
use ui::dashboard::{DashboardTab, DashboardTabs, DeletionHistoryTab, StatsOverview};
use ui::discovery::DiscoveryOverview;
use ui::{BackgroundGlows, GlowColor, GlowSpread};

#[component]
pub fn DashboardPage() -> Element {
    let mut active_tab = use_signal(DashboardTab::default);

    rsx! {
        BackgroundGlows { left: GlowColor::Blue, spread: GlowSpread::Tight }

        div { class: "space-y-6 text-white w-full max-w-5xl z-10 mx-auto",
            div { class: "text-center mb-6",
                h1 { class: "text-4xl font-bold text-beet-accent mb-2 font-display",
                    "Dashboard"
                }
            }

            div { class: "flex justify-center",
                DashboardTabs {
                    active: active_tab(),
                    on_change: move |tab| active_tab.set(tab),
                }
            }

            div { class: "pt-6",
                match active_tab() {
                    DashboardTab::Overview => rsx! { OverviewTab {} },
                    DashboardTab::History => rsx! { DeletionHistoryTab {} },
                    DashboardTab::Discovery => rsx! { DiscoveryOverview {} },
                }
            }
        }
    }
}

#[component]
fn OverviewTab() -> Element {
    let stats_resource = use_resource(|| async { api::get_library_stats().await });
    let binding = stats_resource.read();

    match &*binding {
        Some(Ok(s)) => {
            let s = s.clone();
            rsx! { StatsOverview { stats: s } }
        }
        Some(Err(_)) => rsx! {
            div { class: "text-center text-gray-500 font-mono",
                "Failed to load library stats. Is Navidrome configured?"
            }
        },
        None => rsx! {
            div { class: "text-center text-gray-400 font-mono animate-pulse",
                "Loading stats..."
            }
        },
    }
}
