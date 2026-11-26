//! Floating `360×110px` always-on-top frameless Mini-Player window (`dioxus-desktop`).
//!
//! Designed per `docs/design/DESIGN.md` §6.3 to sit unobtrusively over an IDE or browser:
//! - 64×64 rounded artwork + drag-window handle
//! - Title, Artist, and crisp mono codec badge (`FLAC · 24/96`, `OPUS 160`)
//! - Prev / Play-Pause / Next transport + Star toggle
//! - 1-click context-aware Discovery `Keep` / `Drop` when a staged Discovery track plays
//! - Interactive seekable progress rail (swaps to Soulbeet `seam-light` while loading/buffering)
//! - Restore Studio & Close buttons with monotonic window generation guard

use dioxus::prelude::*;
use dioxus_desktop::{Config, DesktopContext, LogicalSize, WindowBuilder};
use ui::{
    IconCheck, IconLoader, IconMusic, IconPause, IconPictureInPicture2, IconPlay, IconSkipBack,
    IconSkipForward, IconSparkles, IconStar, IconThumbsDown, IconX,
};

use crate::assets::StudioAssets;
use crate::desktop_state::{
    use_cover_art_asset_handler, MiniPlayerAction, MiniPlayerBridge, MiniPlayerSnapshot,
};

/// Spawns the `360×110px` frameless always-on-top Mini-Player secondary window.
pub async fn open_mini_player_window(bridge: MiniPlayerBridge) -> DesktopContext {
    let dom = VirtualDom::new_with_props(MiniPlayerApp, MiniPlayerProps { bridge });
    let cfg = Config::new()
        .with_window(
            WindowBuilder::new()
                .with_title("Soulbeet Mini")
                .with_inner_size(LogicalSize::new(360.0, 110.0))
                .with_min_inner_size(LogicalSize::new(360.0, 110.0))
                .with_max_inner_size(LogicalSize::new(360.0, 110.0))
                .with_resizable(false)
                .with_decorations(false)
                .with_always_on_top(true),
        )
        .with_background_color((15, 5, 24, 255));

    dioxus_desktop::window().new_window(dom, cfg).await
}

#[derive(Props, Clone)]
struct MiniPlayerProps {
    bridge: MiniPlayerBridge,
}

impl PartialEq for MiniPlayerProps {
    fn eq(&self, other: &Self) -> bool {
        self.bridge.generation == other.bridge.generation
    }
}

#[component]
fn MiniPlayerApp(props: MiniPlayerProps) -> Element {
    let bridge = props.bridge.clone();
    let generation = bridge.generation;
    let core_for_art = bridge.core.clone();
    use_cover_art_asset_handler(move || core_for_art.clone());

    let mut snapshot = use_signal(|| bridge.snapshot_rx.borrow().clone());
    let action_tx = bridge.action_tx.clone();

    // Emit `Closed(generation)` strictly once on window teardown so rapid close/reopen
    // never races with a newer Mini-Player window.
    {
        let tx_drop = action_tx.clone();
        use_drop(move || {
            let _ = tx_drop.send(MiniPlayerAction::Closed(generation));
        });
    }

    // Live-subscribe to main Studio playback snapshots
    {
        let rx_source = bridge.snapshot_rx.clone();
        use_future(move || {
            let mut rx = rx_source.clone();
            async move {
                while rx.changed().await.is_ok() {
                    let latest: MiniPlayerSnapshot = rx.borrow().clone();
                    snapshot.set(latest);
                }
            }
        });
    }

    let snap = snapshot();
    let current = snap.current_track.clone();
    let is_playing = snap.status.is_playing();
    let is_busy = snap.status.is_busy();
    let pct = if snap.duration_secs.is_finite() && snap.duration_secs > 0.0 {
        ((snap.position_secs / snap.duration_secs) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let tx_prev = action_tx.clone();
    let tx_toggle = action_tx.clone();
    let tx_next = action_tx.clone();
    let tx_restore = action_tx.clone();
    let tx_star = action_tx.clone();
    let tx_keep = action_tx.clone();
    let tx_drop = action_tx.clone();
    let tx_seek = action_tx.clone();
    let max_dur = if snap.duration_secs.is_finite() && snap.duration_secs > 0.0 {
        snap.duration_secs
    } else {
        1.0
    };
    let clamped_pos = if snap.position_secs.is_finite() {
        snap.position_secs.clamp(0.0, max_dur)
    } else {
        0.0
    };

    rsx! {
        StudioAssets {}

        div {
            "data-oled": if snap.oled_mode { "true" } else { "false" },
            class: "relative w-[360px] h-[110px] bg-beet-surface-low border border-beet-border-default rounded-[10px] overflow-hidden select-none flex flex-col justify-between",

            // Top Window Drag Strip + Main Content
            div {
                class: "flex items-center gap-3 px-2.5 pt-2.5 pb-1.5 flex-1 min-h-0",
                onmousedown: move |_| dioxus_desktop::window().drag(),

                // 64x64 Rounded Album Artwork
                div { class: "w-16 h-16 rounded-[8px] overflow-hidden bg-beet-surface border border-beet-border-subtle shrink-0 flex items-center justify-center",
                    if let Some(url) = current.as_ref().and_then(|t| t.cover_art_url.as_deref()) {
                        img {
                            src: "{url}",
                            alt: "",
                            class: "w-full h-full object-cover pointer-events-none",
                        }
                    } else {
                        IconMusic { size: 22, class: "text-beet-text-disabled".to_string() }
                    }
                }

                // Right Column: Track Meta + Controls
                div { class: "flex-1 min-w-0 h-full flex flex-col justify-between",
                    // Row 1: Title + Codec Badge + Restore / Close
                    div { class: "flex items-start justify-between gap-1.5 min-w-0",
                        div { class: "min-w-0 flex-1",
                            if let Some(ref track) = current {
                                div { class: "flex items-center gap-1.5 min-w-0",
                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate leading-tight",
                                        "{track.title}"
                                    }
                                    if track.is_discovery {
                                        span { class: "inline-flex items-center gap-0.5 px-1 py-0.5 rounded bg-beet-accent-surface text-beet-accent-text tabular-nums-mono text-[9px] font-semibold shrink-0",
                                            IconSparkles { size: 9 }
                                            "DISC"
                                        }
                                    }
                                }
                                div { class: "flex items-center gap-1.5 mt-0.5 min-w-0",
                                    p { class: "text-[11px] text-beet-text-secondary truncate",
                                        "{track.artist}"
                                    }
                                    if let Some(ref badge) = track.codec_badge {
                                        span { class: "px-1 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle text-beet-text-tertiary tabular-nums-mono text-[9px] font-semibold shrink-0",
                                            "{badge}"
                                        }
                                    }
                                }
                            } else {
                                p { class: "text-[13px] font-medium text-beet-text-secondary truncate",
                                    "Soulbeet Studio"
                                }
                                p { class: "text-[11px] text-beet-text-tertiary truncate",
                                    "Ready to play"
                                }
                            }
                        }

                        // Window Restore & Close buttons (stop mousedown so they don't start drag)
                        div {
                            class: "flex items-center gap-0.5 shrink-0",
                            onmousedown: move |evt| evt.stop_propagation(),
                            button {
                                r#type: "button",
                                title: "Restore Studio window",
                                aria_label: "Restore Studio window",
                                class: "w-6 h-6 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors",
                                onclick: move |_| {
                                    let _ = tx_restore.send(MiniPlayerAction::RestoreStudio);
                                },
                                IconPictureInPicture2 { size: 12 }
                            }
                            button {
                                r#type: "button",
                                title: "Close Mini-Player",
                                aria_label: "Close Mini-Player",
                                class: "w-6 h-6 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-crimson-text hover:bg-beet-crimson-surface cursor-pointer transition-colors",
                                onclick: move |_| {
                                    // `use_drop` emits `Closed(generation)` once the window closes.
                                    dioxus_desktop::window().close();
                                },
                                IconX { size: 13 }
                            }
                        }
                    }

                    // Row 2: Transport + Context-Aware Star / Discovery Keep & Drop
                    div {
                        class: "flex items-center justify-between gap-2 pt-1",
                        onmousedown: move |evt| evt.stop_propagation(),

                        // Transport controls
                        div { class: "flex items-center gap-1",
                            button {
                                r#type: "button",
                                aria_label: "Previous track",
                                disabled: current.is_none(),
                                class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-40 cursor-pointer transition-colors",
                                onclick: move |_| {
                                    let _ = tx_prev.send(MiniPlayerAction::Previous);
                                },
                                IconSkipBack { size: 14 }
                            }
                            button {
                                r#type: "button",
                                aria_label: if is_playing { "Pause" } else { "Play" },
                                disabled: current.is_none(),
                                class: "w-8 h-8 rounded-full inline-flex items-center justify-center bg-beet-leaf-solid text-beet-leaf-ink hover:brightness-105 active:scale-95 disabled:opacity-40 cursor-pointer transition-[filter,transform]",
                                onclick: move |_| {
                                    let _ = tx_toggle.send(MiniPlayerAction::TogglePlayPause);
                                },
                                if is_busy {
                                    IconLoader { size: 14 }
                                } else if is_playing {
                                    IconPause { size: 14, filled: true }
                                } else {
                                    IconPlay { size: 14, filled: true, class: "ml-0.5".to_string() }
                                }
                            }
                            button {
                                r#type: "button",
                                aria_label: "Next track",
                                disabled: current.is_none(),
                                class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-40 cursor-pointer transition-colors",
                                onclick: move |_| {
                                    let _ = tx_next.send(MiniPlayerAction::Next);
                                },
                                IconSkipForward { size: 14 }
                            }
                        }

                        // Context-Aware Curation: Discovery Keep/Drop vs Library Star
                        if let Some(track) = current.clone() {
                            div { class: "flex items-center gap-1 shrink-0",
                                if track.is_discovery {
                                    {
                                        let tk = track.clone();
                                        let td = track.clone();
                                        rsx! {
                                            button {
                                                r#type: "button",
                                                title: "Keep in library",
                                                aria_label: "Keep discovery track",
                                                class: "inline-flex items-center gap-1 h-7 px-2 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[11px] font-semibold hover:brightness-105 cursor-pointer",
                                                onclick: move |_| {
                                                    let _ = tx_keep.send(MiniPlayerAction::Keep(tk.clone()));
                                                },
                                                IconCheck { size: 12 }
                                                "Keep"
                                            }
                                            button {
                                                r#type: "button",
                                                title: "Drop from discovery",
                                                aria_label: "Drop discovery track",
                                                class: "w-7 h-7 rounded-full inline-flex items-center justify-center bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white cursor-pointer transition-colors",
                                                onclick: move |_| {
                                                    let _ = tx_drop.send(MiniPlayerAction::Drop(td.clone()));
                                                },
                                                IconThumbsDown { size: 12 }
                                            }
                                        }
                                    }
                                } else {
                                    {
                                        let ts = track.clone();
                                        let starred = track.starred;
                                        rsx! {
                                            button {
                                                r#type: "button",
                                                aria_pressed: if starred { "true" } else { "false" },
                                                title: if starred { "Unstar" } else { "Star" },
                                                aria_label: if starred { "Unstar track" } else { "Star track" },
                                                class: if starred {
                                                    "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-gold hover:bg-beet-gold-surface cursor-pointer transition-colors"
                                                } else {
                                                    "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-gold hover:bg-beet-surface-high cursor-pointer transition-colors"
                                                },
                                                onclick: move |_| {
                                                    let _ = tx_star.send(MiniPlayerAction::Star(ts.clone(), !starred));
                                                },
                                                IconStar { size: 15, filled: starred }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Bottom Interactive Progress Rail (swaps to Soulbeet seam-light while buffering/loading)
            div {
                class: "relative h-[5px] w-full bg-beet-surface-highest shrink-0",
                onmousedown: move |evt| evt.stop_propagation(),
                if is_busy {
                    div { class: "beet-seam-light absolute inset-0", aria_hidden: "true" }
                } else {
                    div {
                        class: "h-full bg-beet-leaf-solid transition-[width] duration-100 pointer-events-none",
                        style: "width: {pct:.2}%;",
                    }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "{max_dur:.1}",
                        step: "0.5",
                        value: "{clamped_pos:.1}",
                        disabled: current.is_none() || snap.duration_secs <= 0.0,
                        aria_label: "Seek playback position",
                        class: "absolute inset-0 w-full h-full opacity-0 cursor-pointer disabled:cursor-default",
                        oninput: move |evt| {
                            if let Ok(next) = evt.value().parse::<f64>() {
                                let _ = tx_seek.send(MiniPlayerAction::Seek(next));
                            }
                        },
                    }
                }
            }
        }
    }
}
