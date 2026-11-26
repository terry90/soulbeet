//! Persistent 84px Refined Midnight Studio bottom player bar with isolated high-frequency
//! scrubber, context-aware curation (Discovery vs Permanent Library), Connect device switcher,
//! volume control, and right-drawer toggles.

use dioxus::prelude::*;
use shared::client::{ConnectDevice, DevicePlatform};

use crate::{
    format_discovery_badge, format_duration, use_player, IconCar, IconCheck, IconInfo, IconLaptop,
    IconListMusic, IconLoader, IconMic2, IconMonitor, IconMusic, IconPause, IconPlay, IconRadio,
    IconRepeat, IconRepeatOne, IconShuffle, IconSkipBack, IconSkipForward, IconSmartphone,
    IconSparkles, IconStar, IconThumbsDown, IconTrash2, IconVolume1, IconVolume2, IconVolumeX,
    PlayerTrack, RepeatMode, StarStrip, StarStripSize,
};

use super::DrawerTab;

/// Isolated playback scrubber reading `player.position_signal()` and `player.duration_signal()`
/// so 4–10Hz audio time updates ONLY re-render this compact scrubber and never the rest of the
/// player bar or studio shell.
#[component]
fn PlayerScrubber(has_track: bool, codec_badge: Option<String>) -> Element {
    let player = use_player();
    let pos = *player.position_signal().read();
    let dur = *player.duration_signal().read();
    let max_dur = if dur.is_finite() && dur > 0.0 {
        dur
    } else {
        1.0
    };
    let clamped_pos = if pos.is_finite() {
        pos.clamp(0.0, max_dur)
    } else {
        0.0
    };
    let pct = if dur > 0.0 {
        ((clamped_pos / dur) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    rsx! {
        div { class: "flex items-center gap-2.5 w-full max-w-xl",
            span { class: "w-10 text-right tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                "{format_duration(clamped_pos.floor() as u32)}"
            }

            div { class: "relative flex-1 h-4 flex items-center group",
                div { class: "w-full h-1 rounded-full bg-beet-surface-highest overflow-hidden",
                    div {
                        class: "h-full rounded-full bg-beet-leaf-solid transition-[width] duration-75",
                        style: "width: {pct:.2}%;",
                    }
                }
                input {
                    r#type: "range",
                    min: "0",
                    max: "{max_dur:.1}",
                    step: "0.5",
                    value: "{clamped_pos:.1}",
                    disabled: !has_track || dur <= 0.0,
                    aria_label: "Seek playback position",
                    class: "absolute inset-0 w-full h-full opacity-0 cursor-pointer disabled:cursor-default",
                    oninput: move |evt| {
                        if let Ok(next) = evt.value().parse::<f64>() {
                            player.seek(next);
                        }
                    },
                }
            }

            span { class: "w-10 text-left tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                "{format_duration(dur.floor() as u32)}"
            }

            if let Some(badge) = codec_badge.as_deref().filter(|b| !b.trim().is_empty()) {
                span { class: "hidden md:inline-flex items-center px-1.5 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold tracking-wider shrink-0",
                    "{badge}"
                }
            }
        }
    }
}

/// Persistent 84px studio bottom player bar shared across Web and Desktop.
#[component]
pub fn StudioPlayerBar(
    active_drawer_tab: Option<DrawerTab>,
    on_toggle_drawer: EventHandler<DrawerTab>,
    #[props(default)] connect_devices: Vec<ConnectDevice>,
    #[props(default)] local_device_id: String,
    #[props(default)] acquiring_in_flight: bool,
    on_keep: EventHandler<PlayerTrack>,
    on_drop: EventHandler<PlayerTrack>,
    on_star: EventHandler<(PlayerTrack, bool)>,
    on_rate: EventHandler<(PlayerTrack, u8)>,
    on_start_radio: EventHandler<PlayerTrack>,
    #[props(default)] on_claim_active_device: Option<EventHandler<()>>,
    #[props(default)] on_transfer_to_device: Option<EventHandler<String>>,
) -> Element {
    let player = use_player();
    let state = player.snapshot();
    let mut connect_open = use_signal(|| false);
    let mut failed_art = use_signal(|| None::<String>);

    let current = state.current_track.clone();
    let has_track = current.is_some();
    let is_playing = state.status.is_playing();
    let is_busy = state.status.is_busy() || acquiring_in_flight;
    let error_msg = match &state.status {
        crate::PlaybackStatus::Error(msg) => Some(msg.clone()),
        _ => None,
    };

    let show_art = current
        .as_ref()
        .and_then(|t| t.cover_art_url.as_ref())
        .filter(|u| !u.is_empty() && failed_art.read().as_ref() != Some(*u))
        .cloned();

    let vol_pct = if state.muted {
        0
    } else {
        (state.volume.clamp(0.0, 1.0) * 100.0).round() as u32
    };

    let other_device_count = connect_devices
        .iter()
        .filter(|d| d.device_id != local_device_id)
        .count();

    rsx! {
        footer {
            aria_label: "Audio player",
            class: "relative shrink-0 h-[84px] bg-beet-surface-low border-t border-beet-border-subtle px-3 sm:px-5 flex items-center justify-between gap-3 z-30",

            // Signature Soulbeet 2px seam-light sweep when buffering / loading / acquiring
            if is_busy {
                div {
                    class: "beet-seam-light absolute inset-x-0 top-0 pointer-events-none",
                    aria_hidden: "true",
                }
            }

            // LEFT (32%): Artwork, Title/Artist, Discovery pill, Context-Aware Curation
            div { class: "flex items-center gap-3 min-w-0 flex-1 lg:w-[32%] lg:flex-none",
                // 52px Cover Art
                button {
                    r#type: "button",
                    aria_label: "Open Track Inspector",
                    title: "Inspect current track",
                    disabled: !has_track,
                    class: "relative w-12 h-12 sm:w-[52px] sm:h-[52px] rounded-[8px] overflow-hidden bg-beet-surface border border-beet-border-subtle shrink-0 flex items-center justify-center cursor-pointer disabled:cursor-default",
                    onclick: move |_| on_toggle_drawer.call(DrawerTab::Inspector),
                    if let Some(url) = show_art {
                        img {
                            src: "{url}",
                            alt: "",
                            class: "w-full h-full object-cover",
                            onerror: move |_| failed_art.set(Some(url.clone())),
                        }
                    } else {
                        IconMusic { size: 20, class: "text-beet-text-disabled".to_string() }
                    }
                }

                // Track Meta
                if let Some(track) = current.clone() {
                    {
                        let track_keep = track.clone();
                        let track_drop = track.clone();
                        let track_star = track.clone();
                        let track_rate = track.clone();
                        let track_radio = track.clone();
                        let is_starred = track.starred;

                        rsx! {
                            div { class: "min-w-0 flex-1",
                                div { class: "flex items-center gap-1.5 min-w-0",
                                    p { class: "text-[14px] font-semibold text-beet-text-primary truncate leading-tight",
                                        "{track.title}"
                                    }
                                    if track.is_discovery {
                                        span { class: "inline-flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text tabular-nums-mono text-[10px] font-semibold tracking-wider shrink-0",
                                            IconSparkles { size: 10 }
                                            "{format_discovery_badge(track.days_remaining)}"
                                        }
                                    }
                                }
                                p { class: "text-[12px] text-beet-text-secondary truncate mt-0.5",
                                    "{track.artist}"
                                }
                                if let Some(ref err) = error_msg {
                                    p { class: "text-[11px] text-beet-crimson-text truncate mt-0.5",
                                        "{err}"
                                    }
                                }
                            }

                            // Context-Aware Quick Curation
                            div { class: "hidden sm:flex items-center gap-1.5 shrink-0",
                                if track.is_discovery {
                                    button {
                                        r#type: "button",
                                        title: "Keep in permanent library (P)",
                                        aria_label: "Keep discovery track in library",
                                        class: "inline-flex items-center gap-1 h-8 px-2.5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold hover:brightness-105 cursor-pointer transition-[filter]",
                                        onclick: move |_| on_keep.call(track_keep.clone()),
                                        IconCheck { size: 14 }
                                        span { class: "hidden xl:inline", "Keep" }
                                    }
                                    button {
                                        r#type: "button",
                                        title: "Drop from discovery (X)",
                                        aria_label: "Drop discovery track",
                                        class: "w-8 h-8 rounded-full inline-flex items-center justify-center bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white cursor-pointer transition-colors",
                                        onclick: move |_| on_drop.call(track_drop.clone()),
                                        IconThumbsDown { size: 14 }
                                    }
                                } else {
                                    button {
                                        r#type: "button",
                                        aria_pressed: if is_starred { "true" } else { "false" },
                                        title: if is_starred { "Unstar track (S)" } else { "Star track (S)" },
                                        aria_label: if is_starred { "Unstar track" } else { "Star track" },
                                        class: if is_starred {
                                            "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-gold hover:bg-beet-gold-surface cursor-pointer transition-colors"
                                        } else {
                                            "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-gold hover:bg-beet-surface-high cursor-pointer transition-colors"
                                        },
                                        onclick: move |_| on_star.call((track_star.clone(), !is_starred)),
                                        IconStar { size: 16, filled: is_starred }
                                    }
                                    div { class: "hidden 2xl:flex items-center",
                                        StarStrip {
                                            rating: track.rating,
                                            size: StarStripSize::Compact,
                                            on_rate: move |r| on_rate.call((track_rate.clone(), r)),
                                            aria_label: format!("Rate {}", track.title),
                                        }
                                    }
                                    button {
                                        r#type: "button",
                                        title: "Start Track Radio (R)",
                                        aria_label: "Start Track Radio",
                                        class: "hidden xl:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-text-tertiary hover:text-beet-accent-text hover:bg-beet-surface-high cursor-pointer transition-colors",
                                        onclick: move |_| on_start_radio.call(track_radio.clone()),
                                        IconRadio { size: 15 }
                                    }
                                    button {
                                        r#type: "button",
                                        title: "Remove from library (X)",
                                        aria_label: "Remove track from library",
                                        class: "hidden xl:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-text-tertiary hover:text-beet-crimson-text hover:bg-beet-crimson-surface cursor-pointer transition-colors",
                                        onclick: move |_| on_drop.call(track_drop.clone()),
                                        IconTrash2 { size: 15 }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    div { class: "min-w-0",
                        p { class: "text-[14px] font-medium text-beet-text-secondary truncate",
                            "Ready to listen"
                        }
                        p { class: "text-[12px] text-beet-text-tertiary truncate",
                            "Pick a track, mix, or press Ctrl+K"
                        }
                    }
                }
            }

            // CENTER (40%): Transport + Isolated Scrubber
            div { class: "flex flex-col items-center justify-center gap-1 flex-1 max-w-xl",
                div { class: "flex items-center gap-2 sm:gap-3",
                    button {
                        r#type: "button",
                        aria_pressed: if state.shuffle { "true" } else { "false" },
                        aria_label: "Toggle shuffle",
                        title: "Shuffle",
                        class: if state.shuffle {
                            "hidden sm:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-leaf-text bg-beet-leaf-surface cursor-pointer transition-colors"
                        } else {
                            "hidden sm:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors"
                        },
                        onclick: move |_| player.toggle_shuffle(),
                        IconShuffle { size: 15 }
                    }

                    button {
                        r#type: "button",
                        aria_label: "Previous track",
                        title: "Previous (Shift+←)",
                        disabled: !has_track,
                        class: "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-40 cursor-pointer transition-colors",
                        onclick: move |_| player.previous(),
                        IconSkipBack { size: 17 }
                    }

                    button {
                        r#type: "button",
                        aria_label: if is_playing { "Pause" } else { "Play" },
                        title: "Play / Pause (Space)",
                        disabled: !has_track && state.queue.is_empty(),
                        class: "w-10 h-10 rounded-full inline-flex items-center justify-center bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 active:scale-95 disabled:opacity-40 cursor-pointer transition-[filter,transform]",
                        onclick: move |_| player.toggle_play_pause(),
                        if state.status.is_busy() {
                            IconLoader { size: 18 }
                        } else if is_playing {
                            IconPause { size: 18, filled: true }
                        } else {
                            IconPlay { size: 18, filled: true, class: "ml-0.5".to_string() }
                        }
                    }

                    button {
                        r#type: "button",
                        aria_label: "Next track",
                        title: "Next (Shift+→)",
                        disabled: !has_track,
                        class: "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-40 cursor-pointer transition-colors",
                        onclick: move |_| player.next(),
                        IconSkipForward { size: 17 }
                    }

                    button {
                        r#type: "button",
                        aria_label: "{state.repeat.aria_label()}",
                        title: "{state.repeat.aria_label()}",
                        class: if state.repeat != RepeatMode::Off {
                            "hidden sm:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-leaf-text bg-beet-leaf-surface cursor-pointer transition-colors"
                        } else {
                            "hidden sm:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors"
                        },
                        onclick: move |_| player.cycle_repeat(),
                        if state.repeat == RepeatMode::One {
                            IconRepeatOne { size: 15 }
                        } else {
                            IconRepeat { size: 15 }
                        }
                    }
                }

                PlayerScrubber {
                    has_track,
                    codec_badge: current.as_ref().and_then(|t| t.codec_badge.clone()),
                }
            }

            // RIGHT (28%): Drawer Toggles, Connect Switcher, Volume
            div { class: "hidden md:flex items-center justify-end gap-1.5 lg:w-[28%] shrink-0",
                // Lyrics Toggle
                button {
                    r#type: "button",
                    aria_pressed: if active_drawer_tab == Some(DrawerTab::Lyrics) { "true" } else { "false" },
                    aria_label: "Toggle Lyrics",
                    title: "Lyrics (L)",
                    class: drawer_toggle_class(active_drawer_tab == Some(DrawerTab::Lyrics)),
                    onclick: move |_| on_toggle_drawer.call(DrawerTab::Lyrics),
                    IconMic2 { size: 16 }
                }

                // Queue Toggle
                button {
                    r#type: "button",
                    aria_pressed: if active_drawer_tab == Some(DrawerTab::Queue) { "true" } else { "false" },
                    aria_label: "Toggle Queue",
                    title: "Queue (Q)",
                    class: drawer_toggle_class(active_drawer_tab == Some(DrawerTab::Queue)),
                    onclick: move |_| on_toggle_drawer.call(DrawerTab::Queue),
                    IconListMusic { size: 16 }
                }

                // Inspector Toggle
                button {
                    r#type: "button",
                    aria_pressed: if active_drawer_tab == Some(DrawerTab::Inspector) { "true" } else { "false" },
                    aria_label: "Toggle Track Inspector",
                    title: "Inspector (I)",
                    class: drawer_toggle_class(active_drawer_tab == Some(DrawerTab::Inspector)),
                    onclick: move |_| on_toggle_drawer.call(DrawerTab::Inspector),
                    IconInfo { size: 16 }
                }

                // Connect Device Switcher
                div { class: "relative",
                    button {
                        r#type: "button",
                        aria_expanded: if connect_open() { "true" } else { "false" },
                        aria_label: "Connect devices",
                        title: "Soulbeet Connect devices",
                        class: if connect_open() || other_device_count > 0 {
                            "relative w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-leaf-text hover:bg-beet-surface-high cursor-pointer transition-colors"
                        } else {
                            "relative w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors"
                        },
                        onclick: move |_| connect_open.toggle(),
                        IconLaptop { size: 16 }
                        if other_device_count > 0 {
                            span { class: "absolute top-1.5 right-1.5 w-1.5 h-1.5 rounded-full bg-beet-leaf-glow" }
                        }
                    }

                    if connect_open() {
                        div {
                            class: "fixed inset-0 z-40",
                            onclick: move |_| connect_open.set(false),
                        }
                        div {
                            role: "dialog",
                            aria_label: "Connect devices",
                            class: "absolute bottom-11 right-0 z-50 w-72 rounded-[12px] bg-beet-surface-high border border-beet-border-default shadow-2xl p-3 space-y-2.5",
                            div { class: "flex items-center justify-between",
                                span { class: "text-[12px] font-semibold text-beet-text-primary",
                                    "Soulbeet Connect"
                                }
                                span { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-leaf-text",
                                    "{connect_devices.len()} ONLINE"
                                }
                            }

                            if connect_devices.is_empty() {
                                p { class: "text-[12px] text-beet-text-tertiary py-2",
                                    "Connecting to multi-device hub…"
                                }
                            } else {
                                div { class: "space-y-1",
                                    for dev in connect_devices.iter() {
                                        {
                                            let is_self = dev.device_id == local_device_id;
                                            let target_id = dev.device_id.clone();
                                            let platform = dev.platform;
                                            rsx! {
                                                button {
                                                    key: "{dev.device_id}",
                                                    r#type: "button",
                                                    class: if dev.is_active {
                                                        "w-full flex items-center justify-between gap-2 px-2.5 py-2 rounded-[8px] bg-beet-leaf-surface border border-beet-leaf-solid/35 text-left cursor-pointer"
                                                    } else {
                                                        "w-full flex items-center justify-between gap-2 px-2.5 py-2 rounded-[8px] hover:bg-beet-surface-highest border border-transparent text-left cursor-pointer transition-colors"
                                                    },
                                                    onclick: move |_| {
                                                        if is_self {
                                                            if let Some(cb) = &on_claim_active_device {
                                                                cb.call(());
                                                            }
                                                        } else if let Some(cb) = &on_transfer_to_device {
                                                            cb.call(target_id.clone());
                                                        }
                                                        connect_open.set(false);
                                                    },
                                                    div { class: "flex items-center gap-2 min-w-0",
                                                        match platform {
                                                            DevicePlatform::Desktop => rsx! { IconMonitor { size: 15, class: "text-beet-text-secondary".to_string() } },
                                                            DevicePlatform::Android => rsx! { IconSmartphone { size: 15, class: "text-beet-text-secondary".to_string() } },
                                                            DevicePlatform::AndroidAuto => rsx! { IconCar { size: 15, class: "text-beet-text-secondary".to_string() } },
                                                            DevicePlatform::Web => rsx! { IconLaptop { size: 15, class: "text-beet-text-secondary".to_string() } },
                                                        }
                                                        div { class: "min-w-0",
                                                            p { class: "text-[13px] font-medium text-beet-text-primary truncate",
                                                                "{dev.name}"
                                                                if is_self {
                                                                    span { class: "text-[11px] text-beet-text-tertiary ml-1", "(This)" }
                                                                }
                                                            }
                                                            if let Some(ref snap) = dev.snapshot {
                                                                if let Some(ref t) = snap.title {
                                                                    p { class: "text-[11px] text-beet-text-tertiary truncate",
                                                                        "{t}"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if dev.is_active {
                                                        span { class: "tabular-nums-mono text-[10px] font-semibold text-beet-leaf-text shrink-0",
                                                            "ACTIVE"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Volume Control
                div { class: "hidden xl:flex items-center gap-1.5 pl-1.5 border-l border-beet-border-subtle",
                    button {
                        r#type: "button",
                        aria_label: if state.muted { "Unmute" } else { "Mute" },
                        title: "Mute / Unmute (M)",
                        class: "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors",
                        onclick: move |_| player.toggle_mute(),
                        if state.muted || state.volume <= 0.001 {
                            IconVolumeX { size: 16 }
                        } else if state.volume < 0.5 {
                            IconVolume1 { size: 16 }
                        } else {
                            IconVolume2 { size: 16 }
                        }
                    }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "100",
                        value: "{vol_pct}",
                        aria_label: "Volume",
                        class: "w-20 accent-beet-leaf-solid cursor-pointer h-1",
                        oninput: move |evt| {
                            if let Ok(v) = evt.value().parse::<f32>() {
                                player.set_volume((v / 100.0).clamp(0.0, 1.0));
                            }
                        },
                    }
                }
            }
        }
    }
}

fn drawer_toggle_class(active: bool) -> &'static str {
    if active {
        "w-8 h-8 rounded-lg inline-flex items-center justify-center bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text cursor-pointer transition-colors"
    } else {
        "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors"
    }
}
