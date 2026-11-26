//! Desktop Studio Activity (Outbox & Sync Log) and Settings (libmpv ReplayGain, OLED, Cache,
//! Server Connection) views.

use dioxus::prelude::*;
use ui::{use_player, IconActivity, IconLogOut, IconPictureInPicture2, IconRefreshCw};

use crate::audio_mpv::ReplayGainMode;
use crate::desktop_state::use_desktop_studio;

#[component]
pub fn ActivityView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();
    let sync_st = studio.sync_status.read().clone();
    let core_opt = studio.core.read().clone();
    let rev = (studio.library_revision)();

    let pending_rows = use_resource(move || {
        let core = core_opt.clone();
        let _rev = rev;
        async move {
            match core {
                Some(c) => c.outbox().pending().await.unwrap_or_default(),
                None => Vec::new(),
            }
        }
    });

    let rows = pending_rows.read().clone().unwrap_or_default();

    rsx! {
        div { class: "p-6 space-y-6 max-w-4xl mx-auto",
            div { class: "flex items-center justify-between gap-3",
                div {
                    h2 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary",
                        "Activity & Outbox"
                    }
                    p { class: "text-[13px] text-beet-text-secondary mt-0.5",
                        "Offline scrobbles, stars, ratings, keeps, and drops replay in strict chronological order."
                    }
                }
                button {
                    r#type: "button",
                    disabled: offline_only,
                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-surface border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary disabled:opacity-40 text-[12px] font-medium cursor-pointer",
                    onclick: move |_| studio.drain_outbox_bg(),
                    IconRefreshCw { size: 14 }
                    span { "Flush Outbox" }
                }
            }

            // Status Summary Cards
            div { class: "grid grid-cols-1 sm:grid-cols-3 gap-3",
                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4",
                    p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary", "Pending Outbox" }
                    p { class: "text-[24px] font-bold text-beet-text-primary mt-1 tabular-nums-mono", "{sync_st.pending_outbox_count}" }
                }
                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4",
                    p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary", "Dead-Lettered" }
                    p { class: "text-[24px] font-bold text-beet-text-primary mt-1 tabular-nums-mono", "{sync_st.dead_letter_count}" }
                }
                div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle p-4",
                    p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary", "Connect Hub" }
                    p {
                        class: if sync_st.connect_online {
                            "text-[18px] font-bold text-beet-leaf-text mt-1.5"
                        } else {
                            "text-[18px] font-bold text-beet-text-tertiary mt-1.5"
                        },
                        if sync_st.connect_online { "Connected" } else { "Offline / Local" }
                    }
                }
            }

            if let Some(ref err) = sync_st.last_error {
                div { class: "p-3 rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 text-[13px] text-beet-crimson-text",
                    "{err}"
                }
            }

            // Pending Outbox Queue List
            div { class: "rounded-[12px] bg-beet-surface border border-beet-border-subtle overflow-hidden",
                div { class: "px-4 py-3 border-b border-beet-border-subtle flex items-center gap-2",
                    IconActivity { size: 15, class: "text-beet-leaf-text".to_string() }
                    span { class: "text-[13px] font-semibold text-beet-text-primary", "Queued Offline Mutations" }
                }
                if rows.is_empty() {
                    div { class: "p-6 text-center text-[13px] text-beet-text-tertiary",
                        "Outbox is clean — all scrobbles and curation edits are synced."
                    }
                } else {
                    div { class: "divide-y divide-beet-border-subtle",
                        for row in rows.iter() {
                            div {
                                key: "ob-{row.seq}",
                                class: "px-4 py-2.5 flex items-center justify-between gap-3 text-[12px]",
                                div { class: "min-w-0",
                                    span { class: "px-1.5 py-0.5 rounded bg-beet-accent-surface text-beet-accent-text tabular-nums-mono text-[10px] font-semibold uppercase mr-2",
                                        "{row.kind}"
                                    }
                                    span { class: "tabular-nums-mono text-beet-text-primary", "{row.song_id}" }
                                    if let Some(ref err) = row.last_error {
                                        p { class: "text-[11px] text-beet-crimson-text truncate mt-0.5", "{err}" }
                                    }
                                }
                                span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                                    "attempts: {row.attempts}"
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
pub fn SettingsView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let cfg = studio.config.read().clone();
    let telemetry = studio.telemetry.read().clone();
    let mpv_err = studio.mpv_error.read().clone();
    let mpris_active = studio
        .media_controls
        .read()
        .lock()
        .map(|m| m.is_active())
        .unwrap_or(false);
    let (db_path, cache_dir, _) = cfg.resolved_paths();

    rsx! {
        div { class: "p-6 space-y-6 max-w-4xl mx-auto",
            h2 { class: "text-[22px] font-bold tracking-[-0.02em] text-beet-text-primary",
                "Desktop Studio Settings"
            }

            // Native libmpv Audio Engine Card
            section { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
                div { class: "flex items-center justify-between gap-3",
                    div {
                        h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                            "Native Audio Engine (libmpv)"
                        }
                        p { class: "text-[12px] text-beet-text-secondary mt-0.5",
                            "Gapless playlist prefetching, direct lossless decoding, and hardware ReplayGain."
                        }
                    }
                    div { class: "flex items-center gap-2",
                        span { class: "px-2.5 py-1 rounded-full bg-beet-surface-high border border-beet-border-subtle text-beet-text-secondary tabular-nums-mono text-[10px] font-semibold",
                            if mpris_active { "OS MEDIA KEYS ON" } else { "OS MEDIA KEYS OFF" }
                        }
                        span { class: "px-2.5 py-1 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/35 text-beet-leaf-text tabular-nums-mono text-[11px] font-semibold",
                            if mpv_err.is_some() { "ERROR" } else { "GAPLESS READY" }
                        }
                    }
                }

                if let Some(ref err) = mpv_err {
                    p { class: "text-[12px] text-beet-crimson-text", "{err}" }
                }

                if let Some(ref t) = telemetry {
                    div { class: "p-3 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle tabular-nums-mono text-[12px] text-beet-text-secondary",
                        "Signal Chain: {t.format_signal_chain()}"
                    }
                }

                // ReplayGain Segmented Control
                div { class: "flex flex-wrap items-center justify-between gap-3 pt-1",
                    div {
                        p { class: "text-[14px] font-medium text-beet-text-primary", "ReplayGain Normalization" }
                        p { class: "text-[12px] text-beet-text-tertiary", "Applies embedded ReplayGain tags cleanly inside libmpv." }
                    }
                    div { class: "inline-flex items-center gap-1 p-1 rounded-full bg-beet-sunken border border-beet-border-subtle",
                        for mode in [ReplayGainMode::Track, ReplayGainMode::Album, ReplayGainMode::Off] {
                            {
                                let active = cfg.replaygain == mode;
                                rsx! {
                                    button {
                                        key: "{mode.label()}",
                                        r#type: "button",
                                        class: if active {
                                            "h-8 px-3.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/40 text-beet-leaf-text text-[12px] font-semibold cursor-pointer"
                                        } else {
                                            "h-8 px-3.5 rounded-full text-beet-text-secondary hover:text-beet-text-primary text-[12px] font-medium cursor-pointer"
                                        },
                                        onclick: move |_| studio.set_replaygain(mode),
                                        "{mode.label()}"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Appearance & Mini-Player Card
            section { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 space-y-4",
                h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                    "Appearance & Windows"
                }

                div { class: "flex items-center justify-between gap-3",
                    div {
                        p { class: "text-[14px] font-medium text-beet-text-primary", "True-Black OLED Mode" }
                        p { class: "text-[12px] text-beet-text-tertiary", "Switches the deep midnight-violet canvas to #000000." }
                    }
                    button {
                        r#type: "button",
                        aria_pressed: if cfg.oled_mode { "true" } else { "false" },
                        class: if cfg.oled_mode {
                            "h-8 px-4 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold cursor-pointer"
                        } else {
                            "h-8 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary text-[12px] font-medium cursor-pointer"
                        },
                        onclick: move |_| studio.set_oled_mode(!cfg.oled_mode, player),
                        if cfg.oled_mode { "OLED On" } else { "Midnight Default" }
                    }
                }

                div { class: "flex items-center justify-between gap-3 pt-2 border-t border-beet-border-subtle",
                    div {
                        p { class: "text-[14px] font-medium text-beet-text-primary", "Floating Always-On-Top Mini-Player" }
                        p { class: "text-[12px] text-beet-text-tertiary", "Compact 360×110px frameless player with 1-click Discovery Keep / Drop." }
                    }
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-leaf-solid/40 text-[12px] font-medium cursor-pointer",
                        onclick: move |_| studio.toggle_mini_player(player),
                        IconPictureInPicture2 { size: 14 }
                        span {
                            if (studio.mini_player_open)() {
                                "Close Mini-Player"
                            } else {
                                "Launch Mini-Player"
                            }
                        }
                    }
                }
            }

            // Storage & Server Card
            section { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-5 space-y-3",
                h3 { class: "text-[16px] font-semibold text-beet-text-primary",
                    "Local Storage & Connection"
                }
                dl { class: "space-y-1.5 text-[12px]",
                    div { class: "flex items-center justify-between gap-3",
                        dt { class: "text-beet-text-tertiary", "Soulbeet Server" }
                        dd { class: "tabular-nums-mono text-beet-text-primary truncate",
                            {cfg.soulbeet_base_url.as_deref().unwrap_or("Not set (Direct Navidrome)")}
                        }
                    }
                    div { class: "flex items-center justify-between gap-3",
                        dt { class: "text-beet-text-tertiary", "SQLite Mirror" }
                        dd { class: "tabular-nums-mono text-beet-text-secondary truncate max-w-md",
                            "{db_path.display()}"
                        }
                    }
                    div { class: "flex items-center justify-between gap-3",
                        dt { class: "text-beet-text-tertiary", "Audio Cache Dir" }
                        dd { class: "tabular-nums-mono text-beet-text-secondary truncate max-w-md",
                            "{cache_dir.display()}"
                        }
                    }
                }
                div { class: "pt-3 flex items-center justify-end",
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text hover:bg-beet-crimson-solid hover:text-white text-[12px] font-semibold cursor-pointer transition-colors",
                        onclick: move |_| studio.disconnect_server(player),
                        IconLogOut { size: 14 }
                        span { "Disconnect & Reconfigure" }
                    }
                }
            }
        }
    }
}
