//! Collapsible 320px right contextual studio drawer (`Queue` / `Lyrics` / `Inspector`).

use dioxus::prelude::*;
use shared::client::{DeleteGuardPreview, TrackLyrics};

use crate::{
    classify_guard_mode, format_discovery_badge, format_duration, protection_kind_label,
    use_player, GuardModalMode, IconCheck, IconInfo, IconListMusic, IconLoader, IconMic2,
    IconMusic, IconPlay, IconRadio, IconShieldAlert, IconShieldCheck, IconSparkles, IconThumbsDown,
    IconTrash2, IconX, PlayerTrack,
};

use super::LyricsPanel;

/// Segmented contextual drawer tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerTab {
    Queue,
    Lyrics,
    Inspector,
}

impl DrawerTab {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Queue => "Queue",
            Self::Lyrics => "Lyrics",
            Self::Inspector => "Inspector",
        }
    }
}

/// 320px right contextual studio drawer (inline on `xl+`, slide-over overlay on smaller viewports).
#[component]
pub fn StudioRightDrawer(
    active_tab: Option<DrawerTab>,
    on_select_tab: EventHandler<DrawerTab>,
    on_close: EventHandler<()>,
    #[props(default)] inspected_track: Option<PlayerTrack>,
    #[props(default)] lyrics: Vec<TrackLyrics>,
    #[props(default)] lyrics_loading: bool,
    #[props(default)] lyrics_fetched: bool,
    #[props(default)] lyrics_error: Option<String>,
    on_fetch_lyrics: EventHandler<()>,
    #[props(default)] guard_preview: Option<DeleteGuardPreview>,
    #[props(default)] guard_loading: bool,
    on_refresh_guard: EventHandler<String>,
    on_open_guard_modal: EventHandler<PlayerTrack>,
    on_keep: EventHandler<PlayerTrack>,
    on_drop: EventHandler<PlayerTrack>,
    on_start_radio: EventHandler<PlayerTrack>,
) -> Element {
    // Hooks MUST run unconditionally before any early return (Dioxus 0.7 Rules of Hooks).
    let player = use_player();

    let Some(tab) = active_tab else {
        return rsx! {};
    };

    // Only subscribe to `PlayerState` once the drawer is open.
    let state = player.snapshot();
    let target_track = inspected_track.or_else(|| state.current_track.clone());

    rsx! {
        // Mobile/tablet backdrop for overlay mode (< xl)
        div {
            class: "fixed inset-0 bg-beet-sunken/70 backdrop-blur-xs z-30 xl:hidden",
            onclick: move |_| on_close.call(()),
        }

        aside {
            aria_label: "Studio contextual drawer",
            class: "fixed right-0 top-0 bottom-[84px] z-40 w-[320px] max-w-[92vw] xl:static xl:z-auto xl:bottom-auto xl:h-full bg-beet-sunken border-l border-beet-border-subtle flex flex-col shrink-0",

            // Segmented Header
            div { class: "h-14 px-3 border-b border-beet-border-subtle flex items-center justify-between gap-2 shrink-0",
                div {
                    role: "tablist",
                    aria_label: "Drawer views",
                    class: "flex items-center gap-1 p-1 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle flex-1",
                    for t in [DrawerTab::Queue, DrawerTab::Lyrics, DrawerTab::Inspector] {
                        {
                            let selected = tab == t;
                            let cls = if selected {
                                "bg-beet-surface-high text-beet-text-primary border-beet-border-default"
                            } else {
                                "text-beet-text-tertiary hover:text-beet-text-secondary border-transparent"
                            };
                            rsx! {
                                button {
                                    key: "{t.label()}",
                                    r#type: "button",
                                    role: "tab",
                                    aria_selected: if selected { "true" } else { "false" },
                                    class: "flex-1 h-7 px-2 rounded-[7px] border inline-flex items-center justify-center gap-1.5 text-[12px] font-medium transition-colors cursor-pointer {cls}",
                                    onclick: move |_| on_select_tab.call(t),
                                    match t {
                                        DrawerTab::Queue => rsx! { IconListMusic { size: 13 } },
                                        DrawerTab::Lyrics => rsx! { IconMic2 { size: 13 } },
                                        DrawerTab::Inspector => rsx! { IconInfo { size: 13 } },
                                    }
                                    span { "{t.label()}" }
                                }
                            }
                        }
                    }
                }

                button {
                    r#type: "button",
                    aria_label: "Close drawer",
                    title: "Close drawer",
                    class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer shrink-0",
                    onclick: move |_| on_close.call(()),
                    IconX { size: 15 }
                }
            }

            // Tab Body
            div { class: "flex-1 overflow-y-auto studio-scrollbar",
                match tab {
                    DrawerTab::Queue => rsx! {
                        QueueTabBody {}
                    },
                    DrawerTab::Lyrics => rsx! {
                        LyricsPanel {
                            lyrics,
                            loading: lyrics_loading,
                            fetched: lyrics_fetched,
                            error: lyrics_error,
                            has_track: state.current_track.is_some(),
                            on_fetch: move |_| on_fetch_lyrics.call(()),
                        }
                    },
                    DrawerTab::Inspector => rsx! {
                        InspectorTabBody {
                            track: target_track,
                            guard_preview,
                            guard_loading,
                            on_refresh_guard,
                            on_open_guard_modal,
                            on_keep,
                            on_drop,
                            on_start_radio,
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn QueueTabBody() -> Element {
    let player = use_player();
    let state = player.snapshot();
    let cur_idx = state.queue_index;
    let is_playing = state.status.is_playing();

    if state.queue.is_empty() {
        return rsx! {
            div { class: "h-full flex flex-col items-center justify-center text-center px-6 py-12 text-beet-text-tertiary space-y-2",
                IconListMusic { size: 28, class: "text-beet-text-disabled".to_string() }
                p { class: "text-[14px] font-medium text-beet-text-secondary",
                    "Queue is empty"
                }
                p { class: "text-[12px]",
                    "Play a track, mix, or Seed Radio to build your queue."
                }
            }
        };
    }

    let queue_clone = state.queue.clone();

    rsx! {
        div { class: "p-3 space-y-4",
            div { class: "flex items-center justify-between px-1",
                span { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                    "{state.queue.len()} IN QUEUE"
                }
                button {
                    r#type: "button",
                    class: "text-[11px] tabular-nums-mono font-semibold uppercase tracking-wider text-beet-text-tertiary hover:text-beet-crimson-text cursor-pointer transition-colors",
                    onclick: move |_| player.clear_queue(),
                    "Clear"
                }
            }

            div { class: "space-y-1", role: "list", aria_label: "Playback queue",
                for (idx, item) in state.queue.iter().enumerate() {
                    {
                        let is_current = cur_idx == Some(idx);
                        let q_for_jump = queue_clone.clone();
                        let row_cls = if is_current {
                            "bg-beet-surface border-beet-leaf-solid/35 hover:bg-beet-surface-high/70 hover:border-beet-leaf-solid/50"
                        } else {
                            "bg-transparent border-transparent hover:bg-beet-surface-low"
                        };
                        rsx! {
                            div {
                                key: "{idx}-{item.id}",
                                role: "listitem",
                                class: "group flex items-center gap-3 px-3 py-2 rounded-[8px] border transition-colors {row_cls}",
                                button {
                                    r#type: "button",
                                    class: "flex items-center gap-3 flex-1 min-w-0 text-left cursor-pointer",
                                    onclick: move |_| player.play_queue(q_for_jump.clone(), idx),
                                    span { class: "w-7 shrink-0 inline-flex items-center justify-end tabular-nums-mono text-[12px] text-beet-text-tertiary",
                                        if is_current && is_playing {
                                            span {
                                                class: "inline-flex items-end justify-end gap-[2px] h-3.5",
                                                aria_hidden: "true",
                                                span { class: "w-[3px] h-full rounded-full bg-beet-leaf-glow beet-eq-bar" }
                                                span {
                                                    class: "w-[3px] h-full rounded-full bg-beet-leaf-glow beet-eq-bar",
                                                    style: "animation-delay: 180ms;",
                                                }
                                                span {
                                                    class: "w-[3px] h-full rounded-full bg-beet-leaf-glow beet-eq-bar",
                                                    style: "animation-delay: 360ms;",
                                                }
                                            }
                                        } else if is_current {
                                            IconPlay {
                                                size: 12,
                                                filled: true,
                                                class: "text-beet-leaf-text".to_string(),
                                            }
                                        } else {
                                            "{idx + 1}"
                                        }
                                    }
                                    span { class: "block flex-1 min-w-0",
                                        span {
                                            class: if is_current {
                                                "block text-[13px] font-semibold text-beet-leaf-text truncate"
                                            } else {
                                                "block text-[13px] font-medium text-beet-text-primary truncate"
                                            },
                                            "{item.title}"
                                        }
                                        span { class: "block text-[11px] text-beet-text-secondary truncate",
                                            "{item.artist}"
                                        }
                                    }
                                }
                                if item.is_discovery {
                                    span { class: "px-1.5 py-0.5 rounded bg-beet-accent-surface text-beet-accent-text tabular-nums-mono text-[9px] font-semibold shrink-0",
                                        "DISC"
                                    }
                                } else if item.is_offline_cached {
                                    span { class: "w-1.5 h-1.5 rounded-full bg-beet-leaf-glow shrink-0", title: "Available offline" }
                                }
                                if let Some(dur) = item.duration_secs {
                                    span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                                        "{format_duration(dur)}"
                                    }
                                }
                                button {
                                    r#type: "button",
                                    aria_label: format!("Remove {} from queue", item.title),
                                    title: "Remove from queue",
                                    class: "w-6 h-6 rounded inline-flex items-center justify-center text-beet-text-disabled hover:text-beet-crimson-text opacity-0 group-hover:opacity-100 focus-visible:opacity-100 cursor-pointer transition-opacity shrink-0",
                                    onclick: move |_| player.remove_from_queue(idx),
                                    IconX { size: 13 }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn InspectorTabBody(
    track: Option<PlayerTrack>,
    guard_preview: Option<DeleteGuardPreview>,
    guard_loading: bool,
    on_refresh_guard: EventHandler<String>,
    on_open_guard_modal: EventHandler<PlayerTrack>,
    on_keep: EventHandler<PlayerTrack>,
    on_drop: EventHandler<PlayerTrack>,
    on_start_radio: EventHandler<PlayerTrack>,
) -> Element {
    let Some(track) = track else {
        return rsx! {
            div { class: "h-full flex flex-col items-center justify-center text-center px-6 py-12 text-beet-text-tertiary space-y-2",
                IconInfo { size: 28, class: "text-beet-text-disabled".to_string() }
                p { class: "text-[14px] font-medium text-beet-text-secondary",
                    "Track Inspector"
                }
                p { class: "text-[12px]",
                    "Play or click the info icon on any track to inspect signal chain, discovery provenance, and deletion safety."
                }
            }
        };
    };

    let track_keep = track.clone();
    let track_drop = track.clone();
    let track_radio = track.clone();
    let track_modal = track.clone();
    let song_id_refresh = track.id.clone();

    let matching_guard = guard_preview
        .as_ref()
        .filter(|g| g.song_id == track.id)
        .cloned();

    rsx! {
        div { class: "p-4 space-y-4",
            // Track Identity Card
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-3.5 space-y-3",
                div { class: "flex items-center gap-3",
                    div { class: "w-14 h-14 rounded-[8px] overflow-hidden bg-beet-surface-high border border-beet-border-subtle shrink-0 flex items-center justify-center",
                        if let Some(url) = track.cover_art_url.as_deref().filter(|u| !u.is_empty()) {
                            img { src: "{url}", alt: "", class: "w-full h-full object-cover" }
                        } else {
                            IconMusic { size: 22, class: "text-beet-text-disabled".to_string() }
                        }
                    }
                    div { class: "min-w-0 flex-1",
                        p { class: "text-[15px] font-semibold text-beet-text-primary leading-snug break-words",
                            "{track.title}"
                        }
                        p { class: "text-[13px] text-beet-text-secondary truncate",
                            "{track.artist}"
                        }
                        if let Some(alb) = track.album.as_deref().filter(|a| !a.is_empty()) {
                            p { class: "text-[12px] text-beet-text-tertiary truncate",
                                "{alb}"
                            }
                        }
                    }
                }

                div { class: "flex flex-wrap gap-2",
                    button {
                        r#type: "button",
                        class: "flex-1 inline-flex items-center justify-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-accent/40 hover:text-beet-accent-text text-[12px] font-medium cursor-pointer transition-colors",
                        onclick: move |_| on_start_radio.call(track_radio.clone()),
                        IconRadio { size: 13 }
                        span { "Track Radio" }
                    }
                    if track.is_discovery {
                        button {
                            r#type: "button",
                            class: "inline-flex items-center justify-center gap-1 h-8 px-3 rounded-full bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 text-[12px] font-semibold cursor-pointer transition-[filter]",
                            onclick: move |_| on_keep.call(track_keep.clone()),
                            IconCheck { size: 13 }
                            span { "Keep" }
                        }
                        button {
                            r#type: "button",
                            class: "inline-flex items-center justify-center gap-1 h-8 px-2.5 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white text-[12px] font-semibold cursor-pointer transition-colors",
                            onclick: move |_| on_drop.call(track_drop.clone()),
                            IconThumbsDown { size: 13 }
                        }
                    }
                }
            }

            // Discovery Provenance Card
            if track.is_discovery || track.reason_label.is_some() {
                div { class: "rounded-[12px] bg-beet-surface-low border border-beet-accent/25 p-3.5 space-y-2",
                    div { class: "flex items-center justify-between gap-2",
                        span { class: "inline-flex items-center gap-1.5 tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-accent-text",
                            IconSparkles { size: 12 }
                            "Why Recommended"
                        }
                        if track.is_discovery {
                            span { class: "px-1.5 py-0.5 rounded-full bg-beet-accent-surface text-beet-accent-text tabular-nums-mono text-[10px] font-semibold",
                                "{format_discovery_badge(track.days_remaining)}"
                            }
                        }
                    }
                    p { class: "text-[13px] text-beet-text-primary leading-relaxed",
                        {track.reason_label.as_deref().unwrap_or("Staged by Soulbeet Discovery for your review.")}
                    }
                }
            }

            // Audio Signal Chain
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-3.5 space-y-2",
                p { class: "tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                    "Audio Signal Chain"
                }
                dl { class: "space-y-1.5 text-[12px]",
                    div { class: "flex items-center justify-between gap-2",
                        dt { class: "text-beet-text-tertiary", "Format" }
                        dd { class: "tabular-nums-mono font-medium text-beet-text-primary",
                            {track.codec_badge.as_deref().unwrap_or("Direct OpenSubsonic Stream")}
                        }
                    }
                    div { class: "flex items-center justify-between gap-2",
                        dt { class: "text-beet-text-tertiary", "Storage" }
                        dd { class: "tabular-nums-mono font-medium text-beet-text-primary",
                            if track.is_discovery {
                                "Discovery Staging"
                            } else if track.is_offline_cached {
                                "Library · Cached Offline"
                            } else {
                                "Permanent Library"
                            }
                        }
                    }
                    if let Some(dur) = track.duration_secs {
                        div { class: "flex items-center justify-between gap-2",
                            dt { class: "text-beet-text-tertiary", "Duration" }
                            dd { class: "tabular-nums-mono font-medium text-beet-text-primary",
                                "{format_duration(dur)}"
                            }
                        }
                    }
                    div { class: "flex items-center justify-between gap-2",
                        dt { class: "text-beet-text-tertiary", "Track ID" }
                        dd { class: "tabular-nums-mono text-[11px] text-beet-text-secondary truncate max-w-[160px]",
                            "{track.id}"
                        }
                    }
                }
            }

            // Multi-Listener Delete Guard
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-3.5 space-y-3",
                div { class: "flex items-center justify-between gap-2",
                    span { class: "inline-flex items-center gap-1.5 tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                        IconShieldCheck { size: 13, class: "text-beet-leaf-text".to_string() }
                        "Multi-Listener Guard"
                    }
                    button {
                        r#type: "button",
                        disabled: guard_loading || track.id.is_empty(),
                        class: "text-[11px] tabular-nums-mono text-beet-accent-text hover:underline disabled:opacity-50 cursor-pointer",
                        onclick: move |_| on_refresh_guard.call(song_id_refresh.clone()),
                        if guard_loading { "Checking…" } else { "Audit" }
                    }
                }

                if guard_loading {
                    div { class: "flex items-center gap-2 text-[12px] text-beet-text-tertiary",
                        IconLoader { size: 13 }
                        span { "Inspecting household listener marks…" }
                    }
                } else if let Some(preview) = matching_guard {
                    {
                        let mode = classify_guard_mode(&preview);
                        rsx! {
                            div { class: "space-y-2 text-[12px]",
                                match mode {
                                    GuardModalMode::PendingDiscovery => rsx! {
                                        p { class: "text-beet-text-secondary",
                                            "Staged Discovery track. Safe to drop with 5-second Undo."
                                        }
                                    },
                                    GuardModalMode::PermanentDelete => rsx! {
                                        p { class: "text-beet-leaf-text",
                                            "Sole listener: no other household accounts protect this file."
                                        }
                                    },
                                    GuardModalMode::ListenerProtected => rsx! {
                                        div { class: "space-y-1.5",
                                            p { class: "text-beet-gold flex items-center gap-1.5 font-medium",
                                                IconShieldAlert { size: 14 }
                                                "Protected by other listeners"
                                            }
                                            for (i, item) in preview.other_listeners.iter().enumerate() {
                                                div {
                                                    key: "{i}",
                                                    class: "p-2 rounded bg-beet-surface-high border border-beet-border-subtle flex items-center justify-between gap-2",
                                                    span { class: "text-beet-text-primary", "{item.summary}" }
                                                    span { class: "tabular-nums-mono text-[9px] uppercase text-beet-text-tertiary shrink-0",
                                                        "{protection_kind_label(item.kind)}"
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    GuardModalMode::HardBlocked => rsx! {
                                        p { class: "text-beet-text-secondary",
                                            {preview.block_reason.as_deref().unwrap_or("Deletion is blocked by guard policy.")}
                                        }
                                    },
                                }
                            }
                        }
                    }
                } else {
                    p { class: "text-[12px] text-beet-text-tertiary",
                        "Run a live guard check before removing permanent tracks shared in your household."
                    }
                }

                if !track.is_discovery && !track.id.is_empty() {
                    button {
                        r#type: "button",
                        class: "w-full inline-flex items-center justify-center gap-1.5 h-8 px-3 rounded-[8px] bg-beet-surface-high border border-beet-border-subtle text-beet-text-secondary hover:text-beet-crimson-text hover:border-beet-crimson-text/30 hover:bg-beet-crimson-surface text-[12px] font-medium cursor-pointer transition-colors",
                        onclick: move |_| on_open_guard_modal.call(track_modal.clone()),
                        IconTrash2 { size: 13 }
                        span { "Remove / Guard Preview…" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlaybackStatus, PlayerCommand, PlayerProvider, PlayerState};

    fn sample_track(id: &str, title: &str) -> PlayerTrack {
        PlayerTrack {
            id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            ..Default::default()
        }
    }

    #[test]
    fn queue_uses_track_row_rhythm_and_replaces_raw_play_glyph() {
        let mut playing_dom = VirtualDom::new(|| {
            let state = use_signal(|| {
                let mut s = PlayerState::default();
                s.current_track = Some(sample_track("t1", "First Track"));
                s.queue = vec![
                    sample_track("t1", "First Track"),
                    sample_track("t2", "Second Track"),
                ];
                s.queue_index = Some(0);
                s.status = PlaybackStatus::Playing;
                s
            });
            let pos = use_signal(|| 0.0f64);
            let dur = use_signal(|| 180.0f64);
            rsx! {
                PlayerProvider {
                    state,
                    position_secs: pos,
                    duration_secs: dur,
                    on_command: move |_: PlayerCommand| {},
                    QueueTabBody {}
                }
            }
        });
        playing_dom.rebuild_in_place();
        let html = dioxus_ssr::render(&playing_dom);
        assert!(html.contains("gap-3 px-3 py-2"), "{html}");
        assert!(html.contains("w-7 shrink-0"), "{html}");
        assert!(html.contains("beet-eq-bar"), "{html}");
        assert!(!html.contains("▶"), "{html}");

        let mut paused_dom = VirtualDom::new(|| {
            let state = use_signal(|| {
                let mut s = PlayerState::default();
                s.current_track = Some(sample_track("t1", "First Track"));
                s.queue = vec![sample_track("t1", "First Track")];
                s.queue_index = Some(0);
                s.status = PlaybackStatus::Paused;
                s
            });
            let pos = use_signal(|| 0.0f64);
            let dur = use_signal(|| 180.0f64);
            rsx! {
                PlayerProvider {
                    state,
                    position_secs: pos,
                    duration_secs: dur,
                    on_command: move |_: PlayerCommand| {},
                    QueueTabBody {}
                }
            }
        });
        paused_dom.rebuild_in_place();
        let paused_html = dioxus_ssr::render(&paused_dom);
        assert!(!paused_html.contains("beet-eq-bar"), "{paused_html}");
        assert!(!paused_html.contains("▶"), "{paused_html}");
        assert!(paused_html.contains("<svg"), "{paused_html}");
    }
}
