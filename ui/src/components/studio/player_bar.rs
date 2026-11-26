//! Persistent 84px Refined Midnight Studio bottom player bar with isolated high-frequency
//! scrubber, context-aware curation (Discovery vs Permanent Library), Connect device switcher,
//! volume control, and right-drawer toggles.

use std::rc::Rc;

use dioxus::prelude::*;
use shared::client::{ConnectDevice, DevicePlatform};

use crate::{
    format_discovery_badge, format_duration, use_player, IconCar, IconCheck, IconInfo, IconLaptop,
    IconListMusic, IconLoader, IconMic2, IconMonitor, IconMusic, IconPause, IconPictureInPicture2,
    IconPlay, IconRadio, IconRepeat, IconRepeatOne, IconShuffle, IconSkipBack, IconSkipForward,
    IconSmartphone, IconSparkles, IconStar, IconThumbsDown, IconTrash2, IconVolume1, IconVolume2,
    IconVolumeX, PlayerTrack, RepeatMode, StarStrip, StarStripSize,
};

use super::DrawerTab;

/// Computes the hovered percentage (`0.0..=100.0`) from a local horizontal offset and track width.
fn compute_hover_pct(offset_x: f64, width: f64) -> Option<f64> {
    if !offset_x.is_finite() || !width.is_finite() || width <= 0.0 {
        return None;
    }
    Some(((offset_x / width).clamp(0.0, 1.0)) * 100.0)
}

/// Clamps floating tooltip horizontal anchor percentage so it never clips past the track edges.
fn clamp_tooltip_left_pct(pct: f64) -> f64 {
    if !pct.is_finite() {
        return 6.0;
    }
    pct.clamp(6.0, 94.0)
}

/// Computes normalized playback progress (`0.0..=100.0`) for a given position and duration.
fn compute_progress_pct(pos: f64, dur: f64) -> f64 {
    if !pos.is_finite() || !dur.is_finite() || dur <= 0.0 {
        return 0.0;
    }
    ((pos.clamp(0.0, dur) / dur) * 100.0).clamp(0.0, 100.0)
}

/// Formats the preview timestamp for a hovered percentage along a track of `duration` seconds.
fn format_hover_timestamp(duration: f64, hover_pct: f64) -> String {
    if !duration.is_finite() || duration <= 0.0 || !hover_pct.is_finite() {
        return format_duration(0);
    }
    let frac = (hover_pct.clamp(0.0, 100.0)) / 100.0;
    let secs = (duration * frac).clamp(0.0, duration);
    format_duration(secs.floor() as u32)
}

/// Computes the integer volume percentage (`0..=100`), accounting for mute state and non-finite inputs.
fn compute_volume_pct(volume: f32, muted: bool) -> u32 {
    if muted || !volume.is_finite() {
        return 0;
    }
    (volume.clamp(0.0, 1.0) * 100.0).round() as u32
}

/// Isolated playback scrubber reading `player.position_signal()` and `player.duration_signal()`
/// so 4–10Hz audio time updates ONLY re-render this compact scrubber and never the rest of the
/// player bar or studio shell.
#[component]
fn PlayerScrubber(
    has_track: bool,
    codec_badge: Option<String>,
    #[props(default)] full_width: bool,
) -> Element {
    let player = use_player();
    let mut track_el = use_signal(|| None::<Rc<MountedData>>);
    let mut track_rect = use_signal(|| (0.0f64, 0.0f64));
    let mut hover_pct = use_signal(|| None::<f64>);
    let mut drag_secs = use_signal(|| None::<f64>);

    let pos = *player.position_signal().read();
    let dur = *player.duration_signal().read();
    let can_seek = has_track && dur.is_finite() && dur > 0.0;
    let max_dur = if can_seek { dur } else { 1.0 };

    let active_drag = if can_seek { drag_secs() } else { None };
    let is_dragging = active_drag.is_some();

    let display_pos = if let Some(ds) = active_drag {
        ds.clamp(0.0, max_dur)
    } else if pos.is_finite() {
        pos.clamp(0.0, max_dur)
    } else {
        0.0
    };
    let pct = compute_progress_pct(display_pos, if can_seek { dur } else { 0.0 });
    let active_hover = if can_seek { hover_pct() } else { None };

    let container_class = if full_width {
        "flex items-center gap-2.5 w-full"
    } else {
        "flex items-center gap-2.5 w-full max-w-xl"
    };

    let rail_height_class = match (full_width, is_dragging) {
        (true, true) => "h-2",
        (true, false) => "h-1.5 group-hover:h-2 group-focus-within:h-2",
        (false, true) => "h-1.5",
        (false, false) => "h-1 group-hover:h-1.5 group-focus-within:h-1.5",
    };

    let fill_class = if is_dragging {
        "relative h-full rounded-full bg-beet-leaf-glow transition-[background-color] duration-150"
    } else {
        "relative h-full rounded-full bg-beet-leaf-solid group-hover:bg-beet-leaf-glow group-focus-within:bg-beet-leaf-glow transition-[width,background-color] duration-75"
    };

    let thumb_visibility_class = if is_dragging {
        "scale-100 opacity-100"
    } else {
        "scale-0 opacity-0 group-hover:scale-100 group-hover:opacity-100 group-focus-within:scale-100 group-focus-within:opacity-100"
    };

    let tooltip_pct = if is_dragging {
        Some(pct)
    } else {
        active_hover
    };

    let dur_secs = if can_seek { dur.floor() as u32 } else { 0 };
    let display_secs = display_pos.floor() as u32;

    rsx! {
        div { class: "{container_class}",
            span { class: "w-10 text-right tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                "{format_duration(display_secs)}"
            }

            div {
                class: "relative flex-1 h-5 flex items-center group",
                onmounted: move |evt| {
                    let data = evt.data();
                    track_el.set(Some(data.clone()));
                    spawn(async move {
                        if let Ok(rect) = data.get_client_rect().await {
                            let w = rect.size.width;
                            if w.is_finite() && w > 0.0 {
                                track_rect.set((rect.origin.x, w));
                            }
                        }
                    });
                },
                onmouseenter: move |evt: MouseEvent| {
                    let client_x = evt.data().client_coordinates().x;
                    if let Some(el) = track_el.read().clone() {
                        spawn(async move {
                            if let Ok(rect) = el.get_client_rect().await {
                                let w = rect.size.width;
                                if w.is_finite() && w > 0.0 {
                                    track_rect.set((rect.origin.x, w));
                                    hover_pct.set(compute_hover_pct(client_x - rect.origin.x, w));
                                }
                            }
                        });
                    }
                },
                onmousemove: move |evt: MouseEvent| {
                    let client_x = evt.data().client_coordinates().x;
                    let (left, width) = *track_rect.read();
                    if width > 0.0 {
                        hover_pct.set(compute_hover_pct(client_x - left, width));
                    } else if let Some(el) = track_el.read().clone() {
                        spawn(async move {
                            if let Ok(rect) = el.get_client_rect().await {
                                let w = rect.size.width;
                                if w.is_finite() && w > 0.0 {
                                    track_rect.set((rect.origin.x, w));
                                    hover_pct.set(compute_hover_pct(client_x - rect.origin.x, w));
                                }
                            }
                        });
                    }
                },
                onmouseleave: move |_| {
                    hover_pct.set(None);
                },

                // Floating Commit Mono timestamp preview tooltip
                if let Some(tp) = tooltip_pct {
                    div {
                        class: "absolute -top-7 -translate-x-1/2 px-1.5 py-0.5 rounded-[6px] bg-beet-surface-high border border-beet-border-default text-[10px] tabular-nums-mono text-beet-text-primary shadow-md whitespace-nowrap pointer-events-none z-10",
                        style: "left: {clamp_tooltip_left_pct(tp):.1}%;",
                        "{format_hover_timestamp(dur, tp)}"
                    }
                }

                // Visible scrubber rail with smooth expansion and keyboard focus ring
                div { class: "relative w-full rounded-full bg-beet-surface-highest overflow-hidden transition-[height,background-color] duration-150 pointer-events-none group-focus-within:ring-2 group-focus-within:ring-beet-leaf-glow/70 group-focus-within:ring-offset-1 group-focus-within:ring-offset-beet-canvas {rail_height_class}",
                    // Subtle cursor ghost fill behind active fill
                    if let Some(hp) = active_hover {
                        div {
                            class: "absolute inset-y-0 left-0 rounded-full bg-beet-leaf-glow/20 pointer-events-none",
                            style: "width: {hp:.2}%;",
                        }
                    }
                    // Active playback fill
                    div {
                        class: "{fill_class}",
                        style: "width: {pct:.2}%;",
                    }
                }

                // Glowing playhead scrubber thumb
                if can_seek {
                    div {
                        class: "absolute top-1/2 -translate-x-1/2 -translate-y-1/2 w-3 h-3 rounded-full bg-white ring-2 ring-beet-leaf-glow shadow-[0_0_8px_rgba(0,229,141,0.55)] pointer-events-none transition-[transform,opacity] duration-150 {thumb_visibility_class}",
                        style: "left: {pct:.2}%;",
                    }
                }

                input {
                    r#type: "range",
                    min: "0",
                    max: "{max_dur:.1}",
                    step: "0.5",
                    value: "{display_pos:.1}",
                    disabled: !can_seek,
                    aria_label: "Seek playback position",
                    class: "absolute inset-0 w-full h-full opacity-0 cursor-pointer disabled:cursor-default",
                    onpointerdown: move |_| {
                        if can_seek {
                            drag_secs.set(Some(display_pos));
                        }
                    },
                    onmousedown: move |_| {
                        if can_seek {
                            drag_secs.set(Some(display_pos));
                        }
                    },
                    oninput: move |evt| {
                        if let Ok(next) = evt.value().parse::<f64>() {
                            let clamped = next.clamp(0.0, max_dur);
                            drag_secs.set(Some(clamped));
                            player.seek(clamped);
                        }
                    },
                    onchange: move |evt| {
                        if let Ok(next) = evt.value().parse::<f64>() {
                            player.seek(next.clamp(0.0, max_dur));
                        }
                        drag_secs.set(None);
                    },
                    onpointerup: move |_| {
                        if let Some(val) = drag_secs() {
                            player.seek(val.clamp(0.0, max_dur));
                        }
                        drag_secs.set(None);
                    },
                    onmouseup: move |_| {
                        if let Some(val) = drag_secs() {
                            player.seek(val.clamp(0.0, max_dur));
                        }
                        drag_secs.set(None);
                    },
                    onkeyup: move |_| {
                        drag_secs.set(None);
                    },
                    onblur: move |_| {
                        drag_secs.set(None);
                    },
                }
            }

            span { class: "w-10 text-left tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                "{format_duration(dur_secs)}"
            }

            if let Some(badge) = codec_badge.as_deref().filter(|b| !b.trim().is_empty()) {
                span { class: "hidden md:inline-flex items-center px-1.5 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold tracking-wider shrink-0",
                    "{badge}"
                }
            }
        }
    }
}

/// Custom-rail studio volume slider matching `PlayerScrubber`'s visual language:
/// hover-expand rail, active glow fill, playhead knob, floating `%` tooltip, and focus-within ring.
#[component]
fn PlayerVolumeSlider(volume: f32, muted: bool, on_change: EventHandler<f32>) -> Element {
    let mut track_el = use_signal(|| None::<Rc<MountedData>>);
    let mut track_rect = use_signal(|| (0.0f64, 0.0f64));
    let mut hover_pct = use_signal(|| None::<f64>);
    let mut is_dragging = use_signal(|| false);

    let effective_vol = if muted {
        0.0f32
    } else if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let vol_pct = compute_volume_pct(volume, muted);
    let dragging = is_dragging();

    let rail_height_class = if dragging {
        "h-1.5"
    } else {
        "h-1 group-hover:h-1.5 group-focus-within:h-1.5"
    };

    let fill_class = if dragging {
        "relative h-full rounded-full bg-beet-leaf-glow transition-[background-color] duration-150"
    } else {
        "relative h-full rounded-full bg-beet-leaf-solid group-hover:bg-beet-leaf-glow group-focus-within:bg-beet-leaf-glow transition-[width,background-color] duration-75"
    };

    let thumb_visibility_class = if dragging {
        "scale-100 opacity-100"
    } else {
        "scale-0 opacity-0 group-hover:scale-100 group-hover:opacity-100 group-focus-within:scale-100 group-focus-within:opacity-100"
    };

    let tooltip = if dragging {
        Some((f64::from(vol_pct), vol_pct))
    } else {
        hover_pct().map(|hp| (hp, hp.round().clamp(0.0, 100.0) as u32))
    };

    rsx! {
        div {
            class: "relative w-20 h-5 flex items-center group",
            onmounted: move |evt| {
                let data = evt.data();
                track_el.set(Some(data.clone()));
                spawn(async move {
                    if let Ok(rect) = data.get_client_rect().await {
                        let w = rect.size.width;
                        if w.is_finite() && w > 0.0 {
                            track_rect.set((rect.origin.x, w));
                        }
                    }
                });
            },
            onmouseenter: move |evt: MouseEvent| {
                let client_x = evt.data().client_coordinates().x;
                if let Some(el) = track_el.read().clone() {
                    spawn(async move {
                        if let Ok(rect) = el.get_client_rect().await {
                            let w = rect.size.width;
                            if w.is_finite() && w > 0.0 {
                                track_rect.set((rect.origin.x, w));
                                hover_pct.set(compute_hover_pct(client_x - rect.origin.x, w));
                            }
                        }
                    });
                }
            },
            onmousemove: move |evt: MouseEvent| {
                let client_x = evt.data().client_coordinates().x;
                let (left, width) = *track_rect.read();
                if width > 0.0 {
                    hover_pct.set(compute_hover_pct(client_x - left, width));
                } else if let Some(el) = track_el.read().clone() {
                    spawn(async move {
                        if let Ok(rect) = el.get_client_rect().await {
                            let w = rect.size.width;
                            if w.is_finite() && w > 0.0 {
                                track_rect.set((rect.origin.x, w));
                                hover_pct.set(compute_hover_pct(client_x - rect.origin.x, w));
                            }
                        }
                    });
                }
            },
            onmouseleave: move |_| {
                hover_pct.set(None);
            },

            if let Some((anchor_pct, label_pct)) = tooltip {
                div {
                    class: "absolute -top-7 -translate-x-1/2 px-1.5 py-0.5 rounded-[6px] bg-beet-surface-high border border-beet-border-default text-[10px] tabular-nums-mono text-beet-text-primary shadow-md whitespace-nowrap pointer-events-none z-10",
                    style: "left: {clamp_tooltip_left_pct(anchor_pct):.1}%;",
                    "{label_pct}%"
                }
            }

            div { class: "relative w-full rounded-full bg-beet-surface-highest overflow-hidden transition-[height,background-color] duration-150 pointer-events-none group-focus-within:ring-2 group-focus-within:ring-beet-leaf-glow/70 group-focus-within:ring-offset-1 group-focus-within:ring-offset-beet-canvas {rail_height_class}",
                if let Some(hp) = hover_pct() {
                    div {
                        class: "absolute inset-y-0 left-0 rounded-full bg-beet-leaf-glow/20 pointer-events-none",
                        style: "width: {hp:.2}%;",
                    }
                }
                div {
                    class: "{fill_class}",
                    style: "width: {vol_pct}%;",
                }
            }

            div {
                class: "absolute top-1/2 -translate-x-1/2 -translate-y-1/2 w-3 h-3 rounded-full bg-white ring-2 ring-beet-leaf-glow shadow-[0_0_8px_rgba(0,229,141,0.55)] pointer-events-none transition-[transform,opacity] duration-150 {thumb_visibility_class}",
                style: "left: {vol_pct}%;",
            }

            input {
                r#type: "range",
                min: "0",
                max: "1",
                step: "0.02",
                value: "{effective_vol:.2}",
                aria_label: "Volume",
                class: "absolute inset-0 w-full h-full opacity-0 cursor-pointer",
                onpointerdown: move |_| is_dragging.set(true),
                onmousedown: move |_| is_dragging.set(true),
                oninput: move |evt| {
                    if let Ok(v) = evt.value().parse::<f32>() {
                        on_change.call(v.clamp(0.0, 1.0));
                    }
                },
                onchange: move |evt| {
                    if let Ok(v) = evt.value().parse::<f32>() {
                        on_change.call(v.clamp(0.0, 1.0));
                    }
                    is_dragging.set(false);
                },
                onpointerup: move |_| is_dragging.set(false),
                onmouseup: move |_| is_dragging.set(false),
                onblur: move |_| is_dragging.set(false),
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
    #[props(default)] mini_player_open: bool,
    #[props(default)] on_toggle_mini_player: Option<EventHandler<()>>,
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

    let other_device_count = connect_devices
        .iter()
        .filter(|d| d.device_id != local_device_id)
        .count();

    rsx! {
        footer {
            aria_label: "Audio player",
            class: "relative shrink-0 bg-beet-surface-low border-t border-beet-border-subtle px-3 sm:px-5 pt-2 pb-2.5 md:py-0 md:h-[84px] flex flex-col md:flex-row md:items-center md:justify-between gap-2 md:gap-3 z-30",

            // Signature Soulbeet 2px seam-light sweep when buffering / loading / acquiring
            if is_busy {
                div {
                    class: "beet-seam-light absolute inset-x-0 top-0 pointer-events-none",
                    aria_hidden: "true",
                }
            }

            // Mobile (< md): Dedicated full-width seek bar above controls so it is clear and easy to scrub
            div { class: "w-full md:hidden",
                PlayerScrubber {
                    has_track,
                    codec_badge: None,
                    full_width: true,
                }
            }

            // Controls row (flex row on mobile; dissolved via `md:contents` into the 3-column desktop bar)
            div { class: "flex items-center justify-between gap-2 w-full md:contents",
            // LEFT (32%): Artwork, Title/Artist, Discovery pill, Context-Aware Curation
            div { class: "flex items-center gap-2.5 sm:gap-3 min-w-0 flex-1 lg:w-[32%] lg:flex-none",
                // Cover Art (40px on mobile, 52px on desktop)
                button {
                    r#type: "button",
                    aria_label: "Open Track Inspector",
                    title: "Inspect current track",
                    disabled: !has_track,
                    class: "group/art relative w-10 h-10 md:w-[52px] md:h-[52px] rounded-[8px] overflow-hidden bg-beet-surface border border-beet-border-subtle hover:border-beet-border-strong disabled:hover:border-beet-border-subtle shrink-0 flex items-center justify-center cursor-pointer disabled:cursor-default transition-colors",
                    onclick: move |_| on_toggle_drawer.call(DrawerTab::Inspector),
                    if let Some(url) = show_art {
                        img {
                            src: "{url}",
                            alt: "",
                            class: "w-full h-full object-cover group-hover/art:scale-105 transition-transform duration-200",
                            onerror: move |_| failed_art.set(Some(url.clone())),
                        }
                    } else {
                        IconMusic { size: 20, class: "text-beet-text-disabled".to_string() }
                    }
                    if has_track {
                        span {
                            class: "absolute inset-0 bg-beet-canvas/55 opacity-0 group-hover/art:opacity-100 transition-opacity duration-150 flex items-center justify-center pointer-events-none",
                            aria_hidden: "true",
                            IconInfo { size: 16, class: "text-beet-text-primary".to_string() }
                        }
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

            // CENTER (40%): Transport + Isolated Scrubber (scrubber shown inline on md+; full-width row on mobile)
            div { class: "flex flex-col items-center justify-center gap-1 shrink-0 md:flex-1 md:max-w-xl",
                div { class: "flex items-center gap-1 sm:gap-3",
                    button {
                        r#type: "button",
                        aria_pressed: if state.shuffle { "true" } else { "false" },
                        aria_label: "Toggle shuffle",
                        title: "Shuffle",
                        class: if state.shuffle {
                            "hidden sm:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-leaf-text bg-beet-leaf-surface hover:bg-beet-surface-high hover:brightness-110 cursor-pointer transition-[color,background-color,filter]"
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
                            "hidden sm:inline-flex w-8 h-8 rounded-lg items-center justify-center text-beet-leaf-text bg-beet-leaf-surface hover:bg-beet-surface-high hover:brightness-110 cursor-pointer transition-[color,background-color,filter]"
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

                div { class: "hidden md:flex w-full justify-center",
                    PlayerScrubber {
                        has_track,
                        codec_badge: current.as_ref().and_then(|t| t.codec_badge.clone()),
                    }
                }
            }

            // RIGHT (28%): Drawer Toggles, Connect Switcher, Volume
            div { class: "flex items-center justify-end gap-1 sm:gap-1.5 lg:w-[28%] shrink-0",
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
                    class: format!("hidden sm:inline-flex {}", drawer_toggle_class(active_drawer_tab == Some(DrawerTab::Inspector))),
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
                        onkeydown: move |evt: KeyboardEvent| {
                            if evt.key() == Key::Escape && connect_open() {
                                evt.prevent_default();
                                connect_open.set(false);
                            }
                        },
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
                            tabindex: "-1",
                            onkeydown: move |evt: KeyboardEvent| {
                                if evt.key() == Key::Escape {
                                    evt.prevent_default();
                                    connect_open.set(false);
                                }
                            },
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
                                                        "w-full flex items-center justify-between gap-2 px-2.5 py-2 rounded-[8px] bg-beet-leaf-surface border border-beet-leaf-solid/35 hover:bg-beet-surface-high hover:border-beet-leaf-solid/55 hover:brightness-110 text-left cursor-pointer transition-[background-color,border-color,filter]"
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
                    PlayerVolumeSlider {
                        volume: state.volume,
                        muted: state.muted,
                        on_change: move |v| player.set_volume(v),
                    }
                }

                // Desktop Pop-Out Floating Mini-Player Toggle
                if let Some(mini_cb) = on_toggle_mini_player {
                    button {
                        r#type: "button",
                        aria_pressed: if mini_player_open { "true" } else { "false" },
                        aria_label: if mini_player_open { "Close floating Mini-Player" } else { "Open floating Mini-Player" },
                        title: if mini_player_open { "Close Mini-Player" } else { "Pop-Out Mini-Player (360×110)" },
                        class: if mini_player_open {
                            "w-8 h-8 rounded-lg inline-flex items-center justify-center bg-beet-leaf-surface border border-beet-leaf-solid/35 text-beet-leaf-text cursor-pointer transition-colors"
                        } else {
                            "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors"
                        },
                        onclick: move |_| mini_cb.call(()),
                        IconPictureInPicture2 { size: 16 }
                    }
                }
            }
            }
        }
    }
}

fn drawer_toggle_class(active: bool) -> &'static str {
    if active {
        "w-8 h-8 rounded-lg inline-flex items-center justify-center bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text hover:bg-beet-surface-high hover:border-beet-accent/50 hover:brightness-110 cursor-pointer transition-[color,background-color,border-color,filter]"
    } else {
        "w-8 h-8 rounded-lg inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_hover_pct_clamps_and_handles_invalid_inputs() {
        assert_eq!(compute_hover_pct(0.0, 200.0), Some(0.0));
        assert_eq!(compute_hover_pct(100.0, 200.0), Some(50.0));
        assert_eq!(compute_hover_pct(200.0, 200.0), Some(100.0));
        assert_eq!(compute_hover_pct(-20.0, 200.0), Some(0.0));
        assert_eq!(compute_hover_pct(260.0, 200.0), Some(100.0));

        assert_eq!(compute_hover_pct(50.0, 0.0), None);
        assert_eq!(compute_hover_pct(50.0, -10.0), None);
        assert_eq!(compute_hover_pct(f64::NAN, 200.0), None);
        assert_eq!(compute_hover_pct(50.0, f64::INFINITY), None);
    }

    #[test]
    fn clamp_tooltip_left_pct_keeps_tooltip_within_rail_bounds() {
        assert_eq!(clamp_tooltip_left_pct(0.0), 6.0);
        assert_eq!(clamp_tooltip_left_pct(5.9), 6.0);
        assert_eq!(clamp_tooltip_left_pct(42.5), 42.5);
        assert_eq!(clamp_tooltip_left_pct(95.0), 94.0);
        assert_eq!(clamp_tooltip_left_pct(100.0), 94.0);
        assert_eq!(clamp_tooltip_left_pct(f64::NAN), 6.0);
    }

    #[test]
    fn compute_progress_pct_normalizes_and_guards_edges() {
        assert_eq!(compute_progress_pct(0.0, 240.0), 0.0);
        assert_eq!(compute_progress_pct(120.0, 240.0), 50.0);
        assert_eq!(compute_progress_pct(300.0, 240.0), 100.0);
        assert_eq!(compute_progress_pct(-5.0, 240.0), 0.0);
        assert_eq!(compute_progress_pct(10.0, 0.0), 0.0);
        assert_eq!(compute_progress_pct(f64::NAN, 240.0), 0.0);
    }

    #[test]
    fn format_hover_timestamp_handles_boundaries_and_hours() {
        assert_eq!(format_hover_timestamp(200.0, 0.0), "00:00");
        assert_eq!(format_hover_timestamp(200.0, 50.0), "01:40");
        assert_eq!(format_hover_timestamp(200.0, 100.0), "03:20");
        assert_eq!(format_hover_timestamp(200.0, 150.0), "03:20");
        assert_eq!(format_hover_timestamp(200.0, -10.0), "00:00");
        assert_eq!(format_hover_timestamp(7200.0, 50.0), "1:00:00");
        assert_eq!(format_hover_timestamp(0.0, 50.0), "00:00");
        assert_eq!(format_hover_timestamp(f64::NAN, 50.0), "00:00");
    }

    #[test]
    fn compute_volume_pct_respects_mute_and_clamping() {
        assert_eq!(compute_volume_pct(0.8, false), 80);
        assert_eq!(compute_volume_pct(0.8, true), 0);
        assert_eq!(compute_volume_pct(1.5, false), 100);
        assert_eq!(compute_volume_pct(-0.2, false), 0);
        assert_eq!(compute_volume_pct(f32::NAN, false), 0);
    }
}
