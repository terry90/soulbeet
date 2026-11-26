use dioxus::prelude::*;
use shared::navidrome::{LibraryStats, TopPlayedTrack};
use ui::dashboard::{DashboardTab, DashboardTabs, DeletionHistoryTab, StatsOverview};
use ui::discovery::DiscoveryOverview;
use ui::{
    friendly_error, subsonic_cover_art_url, try_use_player, IconLoader, IconRefreshCw, PlayerTrack,
};

use crate::auth::use_auth;
use crate::web_player::WebStudioContext;

#[component]
pub fn DashboardPage() -> Element {
    let mut active_tab = use_signal(DashboardTab::default);

    rsx! {
        div { class: "w-full max-w-5xl mx-auto px-4 sm:px-6 py-6 sm:py-8 space-y-6 text-beet-text-primary",
            header { class: "space-y-1.5",
                h1 { class: "text-[24px] font-semibold tracking-tight text-beet-text-primary leading-tight",
                    "Dashboard"
                }
                p { class: "text-[13px] text-beet-text-secondary",
                    "Library health, listening insights, deletion history, and discovery automation."
                }
            }

            DashboardTabs {
                active: active_tab(),
                on_change: move |tab| active_tab.set(tab),
            }

            div { class: "pt-2",
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
    let auth = use_auth();
    let player = try_use_player();
    let studio = try_use_context::<WebStudioContext>();

    let mut refreshing = use_signal(|| false);
    let mut stats_state: Signal<Option<Result<LibraryStats, String>>> = use_signal(|| None);

    let fetch_stats = use_callback(move |()| {
        refreshing.set(true);
        spawn(async move {
            let res = auth
                .call(api::get_library_stats())
                .await
                .map_err(|e| friendly_error(&e));
            stats_state.set(Some(res));
            refreshing.set(false);
        });
    });

    use_effect(move || {
        if stats_state.peek().is_none() {
            fetch_stats.call(());
        }
    });

    let on_play_track = if let (Some(player), Some(mut studio)) = (player, studio) {
        Some(EventHandler::new(move |clicked: TopPlayedTrack| {
            if player.offline_only() {
                studio.show_toast("Offline Only mode is active — remote playback is unavailable.");
                return;
            }
            let nav = studio.navidrome_info();
            let top_tracks: Vec<TopPlayedTrack> = stats_state
                .peek()
                .as_ref()
                .and_then(|res| res.as_ref().ok())
                .map(|s| s.top_played_tracks.clone())
                .unwrap_or_else(|| vec![clicked.clone()]);

            let start_idx = top_tracks
                .iter()
                .position(|t| t.song_id == clicked.song_id)
                .unwrap_or(0);

            let queue: Vec<PlayerTrack> = top_tracks
                .into_iter()
                .map(|t| {
                    let cover = nav
                        .as_ref()
                        .map(|info| subsonic_cover_art_url(info, &t.song_id, 256));
                    let mut track = PlayerTrack {
                        id: t.song_id.clone(),
                        title: t.title,
                        artist: t.artist,
                        album: (!t.album.is_empty()).then_some(t.album),
                        cover_art_url: cover,
                        cover_art_id: Some(t.song_id),
                        duration_secs: t.duration,
                        rating: t.rating.unwrap_or(0).min(5),
                        ..Default::default()
                    };
                    studio.apply_curation_overrides(&mut track);
                    track
                })
                .collect();

            if !queue.is_empty() {
                player.play_queue(queue, start_idx);
            }
        }))
    } else {
        None
    };

    let culture = studio.map(|s| s.culture_profile.read().clone());
    let pending_triage = studio.map(|s| s.visible_triage_tracks().len());
    let binding = stats_state.read();

    match &*binding {
        Some(Ok(stats)) => rsx! {
            StatsOverview {
                stats: stats.clone(),
                culture,
                pending_triage,
                on_play_track,
                on_refresh: move |_| fetch_stats.call(()),
                refreshing: refreshing(),
            }
        },
        Some(Err(err)) => rsx! {
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-6 text-center space-y-3",
                p { class: "text-[13px] text-beet-text-secondary",
                    "Failed to load library stats: {err}"
                }
                button {
                    r#type: "button",
                    disabled: refreshing(),
                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-primary hover:bg-beet-surface-highest disabled:opacity-50 transition-colors cursor-pointer",
                    onclick: move |_| fetch_stats.call(()),
                    IconRefreshCw {
                        size: 13,
                        class: if refreshing() { "animate-spin motion-reduce:animate-none".to_string() } else { String::new() },
                    }
                    span { if refreshing() { "Retrying…" } else { "Retry" } }
                }
            }
        },
        None => rsx! {
            div { class: "py-10 flex items-center justify-center gap-2 text-beet-text-tertiary text-[13px]",
                IconLoader { size: 16 }
                span { "Loading library stats…" }
            }
        },
    }
}
