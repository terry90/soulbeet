//! Soulbeet Desktop Studio: native offline-first Linux & Windows music player.
//!
//! Combines `dioxus-desktop`, in-process `client-core` (local SQLite mirror + FTS5 + offline
//! LRU cache + mutation outbox + Connect WS), native `libmpv` (gapless + ReplayGain + live
//! audio telemetry), `souvlaki` (Linux MPRIS2 / Windows SMTC), and the floating `360×110px`
//! always-on-top Mini-Player window.

mod assets;
mod audio_mpv;
mod desktop_state;
mod media_controls;
mod mini_player;
mod views;

use dioxus::prelude::*;
use dioxus_desktop::{Config, LogicalSize, WindowBuilder};
use tracing_subscriber::EnvFilter;
use ui::{
    classify_local_playlist_category, use_player, CommandPalette, DrawerTab, IconListMusic,
    IconPlus, Modal, MultiListenerGuardModal, PaletteCategory, PaletteCommand, PlayerTrack,
    StudioMobileNav, StudioNavSection, StudioPlayerBar, StudioRightDrawer, StudioSidebar,
    StudioTopBar, UndoSnackbar,
};

use crate::assets::StudioAssets;
use crate::audio_mpv::ensure_c_numeric_locale;
use crate::desktop_state::{
    fail_closed_guard_preview, use_desktop_studio, DesktopStudioProvider,
};
use crate::views::{
    ActivityView, DiscoverView, HomeView, LibraryView, SearchView, SettingsView, SetupView,
};

fn main() {
    ensure_c_numeric_locale();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("desktop=info,client_core=info,warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let cfg = Config::new()
        .with_window(
            WindowBuilder::new()
                .with_title("Soulbeet Studio")
                .with_inner_size(LogicalSize::new(1360.0, 860.0))
                .with_min_inner_size(LogicalSize::new(960.0, 640.0)),
        )
        .with_background_color((15, 5, 24, 255));

    dioxus::LaunchBuilder::desktop().with_cfg(cfg).launch(App);
}

#[component]
fn App() -> Element {
    rsx! {
        StudioAssets {}
        DesktopStudioProvider {
            DesktopRoot {}
        }
    }
}

/// Top-level gate component: strictly obeys Dioxus 0.7 Rules of Hooks by isolating all Studio
/// hooks inside [`DesktopShell`] rather than after an early return.
#[component]
fn DesktopRoot() -> Element {
    let studio = use_desktop_studio();
    let is_configured = studio.config.read().is_configured();

    if !is_configured {
        rsx! { SetupView {} }
    } else {
        rsx! { DesktopShell {} }
    }
}

#[component]
fn DesktopShell() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let cfg = studio.config.read().clone();

    let active_section = (studio.active_section)();
    let active_drawer = (studio.active_drawer)();
    let offline_only = player.offline_only();
    let sync_st = studio.sync_status.read().clone();
    let sidebar_playlists = studio.sidebar_playlists();
    let connect_devices = studio.connect_devices.read().clone();
    let mini_open = (studio.mini_player_open)();
    let pending_drop_active = studio.pending_drops.read().active().cloned();
    let status_toast = studio.status_toast.read().clone();
    let guard_modal_track = studio.guard_modal_track.read().clone();
    let guard_preview = studio.guard_preview.read().clone();
    let guard_loading = (studio.guard_loading)();
    let inspected_track = studio.inspected_track.read().clone();
    let lyrics = studio.lyrics.read().clone();
    let lyrics_loading = (studio.lyrics_loading)();
    let lyrics_error = studio.lyrics_error.read().clone();

    let palette_commands = build_palette_commands(offline_only, mini_open, cfg.replaygain);

    // Clean up window keyboard listener when DesktopShell unmounts (e.g. on Disconnect).
    use_drop(move || {
        document::eval(
            r#"
            if (window.__soulbeetKeyHandler) {
                window.removeEventListener("keydown", window.__soulbeetKeyHandler);
                window.__soulbeetKeyHandler = null;
            }
            "#,
        );
    });

    // Global desktop keyboard shortcut bridge: always replaces any prior listener cleanly so
    // shortcuts continue working across Disconnect -> Reconnect transitions.
    use_future(move || {
        let mut st = studio;
        async move {
            let mut eval = document::eval(
                r#"
                if (window.__soulbeetKeyHandler) {
                    window.removeEventListener("keydown", window.__soulbeetKeyHandler);
                }
                window.__soulbeetKeyHandler = (evt) => {
                    const active = document.activeElement;
                    const tag = active ? active.tagName : "BODY";
                    const role = active ? active.getAttribute("role") : null;
                    const editable = !!(active && active.isContentEditable);
                    const dialogOpen = !!document.querySelector('dialog[open], [role="dialog"]:not([inert])');
                    const ctrlOrMeta = !!(evt.ctrlKey || evt.metaKey);
                    const key = evt.key || "";

                    const handled = (ctrlOrMeta && key.toLowerCase() === "k")
                        || key === "Escape"
                        || (!ctrlOrMeta && !evt.altKey && (
                            key === " " || key === "ArrowLeft" || key === "ArrowRight"
                            || "sSpPxXlLqQiIrRmM12345".includes(key)
                        ));
                    if (!handled) return;

                    const upperTag = (tag || "").toUpperCase();
                    const isEscape = key === "Escape";
                    const isCmdK = ctrlOrMeta && key.toLowerCase() === "k";
                    if (!isEscape && !isCmdK) {
                        if (dialogOpen || editable || upperTag === "INPUT" || upperTag === "TEXTAREA" || upperTag === "SELECT") {
                            return;
                        }
                        if (key === " " && (upperTag === "BUTTON" || upperTag === "A" || (role && role.toLowerCase() === "button"))) {
                            return;
                        }
                    }

                    if (key === " " || key === "ArrowLeft" || key === "ArrowRight" || isCmdK) {
                        evt.preventDefault();
                    }
                    dioxus.send({
                        key,
                        ctrl: ctrlOrMeta,
                        shift: !!evt.shiftKey,
                        alt: !!evt.altKey,
                        tag,
                        role,
                        editable,
                        dialog_open: dialogOpen,
                    });
                };
                window.addEventListener("keydown", window.__soulbeetKeyHandler);
                "#,
            );

            while let Ok(val) = eval.recv::<serde_json::Value>().await {
                let key = val.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let ctrl = val.get("ctrl").and_then(|v| v.as_bool()).unwrap_or(false);
                let shift = val.get("shift").and_then(|v| v.as_bool()).unwrap_or(false);
                let alt = val.get("alt").and_then(|v| v.as_bool()).unwrap_or(false);
                let tag = val.get("tag").and_then(|v| v.as_str()).unwrap_or("BODY");
                let role = val.get("role").and_then(|v| v.as_str());
                let editable = val.get("editable").and_then(|v| v.as_bool()).unwrap_or(false);
                let dialog_open = val
                    .get("dialog_open")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let cmdk_open = *st.cmdk_open.peek();

                if ui::should_ignore_shortcut_target(
                    tag,
                    role,
                    editable,
                    dialog_open,
                    cmdk_open,
                    key,
                    ctrl,
                ) {
                    continue;
                }

                if ctrl && key.eq_ignore_ascii_case("k") {
                    st.cmdk_open.toggle();
                    continue;
                }
                if key == "Escape" {
                    if *st.cmdk_open.peek() {
                        st.cmdk_open.set(false);
                    } else if st.guard_modal_track.peek().is_some() {
                        st.guard_modal_track.set(None);
                    } else if !dialog_open && st.active_drawer.peek().is_some() {
                        st.active_drawer.set(None);
                    }
                    continue;
                }
                if ctrl || alt {
                    continue;
                }

                match key {
                    " " => player.toggle_play_pause(),
                    "ArrowLeft" => {
                        if shift {
                            player.previous();
                        } else {
                            let cur = *player.position_signal().peek();
                            player.seek((cur - 5.0).max(0.0));
                        }
                    }
                    "ArrowRight" => {
                        if shift {
                            player.next();
                        } else {
                            let cur = *player.position_signal().peek();
                            player.seek(cur + 5.0);
                        }
                    }
                    "m" | "M" => player.toggle_mute(),
                    "l" | "L" => st.toggle_drawer(DrawerTab::Lyrics),
                    "q" | "Q" => st.toggle_drawer(DrawerTab::Queue),
                    "i" | "I" => st.toggle_drawer(DrawerTab::Inspector),
                    "s" | "S" => {
                        if let Some(track) = player.state_signal().peek().current_track.clone() {
                            let next = !track.starred;
                            st.star_track(player, track, next);
                        }
                    }
                    "p" | "P" => {
                        if let Some(track) = player.state_signal().peek().current_track.clone() {
                            if track.is_discovery {
                                st.keep_track(player, track);
                            }
                        }
                    }
                    "x" | "X" => {
                        if let Some(track) = player.state_signal().peek().current_track.clone() {
                            st.drop_track(player, track);
                        }
                    }
                    "r" | "R" => {
                        if let Some(track) = player.state_signal().peek().current_track.clone() {
                            st.start_local_radio(player, track);
                        }
                    }
                    "1" | "2" | "3" | "4" | "5" => {
                        if let (Some(track), Ok(rating)) = (
                            player.state_signal().peek().current_track.clone(),
                            key.parse::<u8>(),
                        ) {
                            st.rate_track(player, track, rating);
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    rsx! {
        div {
            "data-oled": if cfg.oled_mode { "true" } else { "false" },
            class: "h-screen w-screen bg-beet-canvas text-beet-text-primary flex flex-col overflow-hidden",

            // Main 3-Pane Studio Area
            div { class: "flex-1 flex min-h-0 overflow-hidden",
                // Left 240px Sidebar Rail
                StudioSidebar {
                    active: active_section,
                    on_navigate: move |sec| studio.active_section.set(sec),
                    offline_only,
                    on_toggle_offline: move |next| player.set_offline_only(next),
                    show_offline_toggle: true,
                    active_transfers_count: sync_st.pending_outbox_count,
                    playlists: sidebar_playlists,
                    on_select_playlist: move |pid| {
                        studio.open_playlist_detail(pid);
                    },
                    username: Some(cfg.device_name.clone()),
                    on_logout: move |_| studio.disconnect_server(player),
                }

                // Center Stage (Sticky Top Bar + Scrolling View)
                div { class: "flex-1 flex flex-col min-w-0 min-h-0 overflow-hidden",
                    StudioTopBar {
                        active: active_section,
                        offline_only,
                        on_toggle_offline: move |next| player.set_offline_only(next),
                        show_offline_toggle: true,
                        on_open_palette: move |_| studio.cmdk_open.set(true),
                        on_open_downloads: move |_| studio.active_section.set(StudioNavSection::Activity),
                        on_logout: Some(EventHandler::new(move |_| studio.disconnect_server(player))),
                        active_transfers_count: sync_st.pending_outbox_count,
                        culture_badge: Some(format!("RG {}", cfg.replaygain.label().to_uppercase())),
                    }

                    main { class: "flex-1 overflow-y-auto studio-scrollbar",
                        match active_section {
                            StudioNavSection::Home => rsx! { HomeView {} },
                            StudioNavSection::Discover => rsx! { DiscoverView {} },
                            StudioNavSection::Search => rsx! { SearchView {} },
                            StudioNavSection::Library => rsx! { LibraryView {} },
                            StudioNavSection::Activity => rsx! { ActivityView {} },
                            StudioNavSection::Settings => rsx! { SettingsView {} },
                        }
                    }
                }

                // Collapsible 320px Right Contextual Drawer (Queue / Lyrics / Inspector)
                StudioRightDrawer {
                    active_tab: active_drawer,
                    on_select_tab: move |tab| studio.active_drawer.set(Some(tab)),
                    on_close: move |_| studio.active_drawer.set(None),
                    inspected_track,
                    lyrics,
                    lyrics_loading,
                    lyrics_error,
                    on_fetch_lyrics: move |_| {
                        if let Some(track) = player.current_track() {
                            studio.fetch_lyrics(track.id, Some(track.artist), Some(track.title));
                        }
                    },
                    guard_preview: guard_preview.clone(),
                    guard_loading,
                    on_refresh_guard: move |song_id| studio.audit_delete_guard(song_id),
                    on_open_guard_modal: move |track: ui::PlayerTrack| {
                        let id = track.id.clone();
                        studio.guard_preview.set(Some(fail_closed_guard_preview(
                            id.clone(),
                            "Inspecting household listener protection…".into(),
                        )));
                        studio.guard_modal_track.set(Some(track));
                        studio.audit_delete_guard(id);
                    },
                    on_keep: move |track| studio.keep_track(player, track),
                    on_drop: move |track| studio.drop_track(player, track),
                    on_rate: move |(track, rating)| studio.rate_track(player, track, rating),
                    on_start_radio: move |track| studio.start_local_radio(player, track),
                }
            }

            // Persistent 84px Studio Bottom Player Bar (+ Desktop Mini-Player Pop-Out)
            StudioPlayerBar {
                active_drawer_tab: active_drawer,
                on_toggle_drawer: move |tab| studio.toggle_drawer(tab),
                connect_devices,
                local_device_id: cfg.device_id.clone(),
                acquiring_in_flight: sync_st.syncing,
                on_keep: move |track| studio.keep_track(player, track),
                on_drop: move |track| studio.drop_track(player, track),
                on_star: move |(track, starred)| studio.star_track(player, track, starred),
                on_rate: move |(track, rating)| studio.rate_track(player, track, rating),
                on_start_radio: move |track| studio.start_local_radio(player, track),
                on_claim_active_device: Some(EventHandler::new(move |_| studio.claim_connect_active())),
                on_transfer_to_device: Some(EventHandler::new(move |dev_id| {
                    studio.transfer_playback_to(player, dev_id);
                })),
                mini_player_open: mini_open,
                on_toggle_mini_player: Some(EventHandler::new(move |_| {
                    studio.toggle_mini_player(player);
                })),
            }

            // Compact Mobile Navigation on narrow windows (< 1024px, below Player Bar)
            StudioMobileNav {
                active: active_section,
                on_navigate: move |sec| studio.active_section.set(sec),
            }

            // Floating 5-second Undo Snackbar for Pending Discovery Drops (or brief status toast)
            if let Some(pending) = pending_drop_active {
                {
                    let key_id = pending.key.clone();
                    let msg = pending.snackbar_message();
                    rsx! {
                        div { class: "fixed bottom-[152px] lg:bottom-24 left-1/2 -translate-x-1/2 z-50 px-4 w-full max-w-md",
                            UndoSnackbar {
                                message: msg,
                                duration_ms: 5000,
                                key_id: Some(key_id.clone()),
                                on_undo: move |_| studio.undo_active_drop(),
                                on_dismiss: move |_| studio.expire_active_drop(key_id.clone()),
                            }
                        }
                    }
                }
            } else if let Some(msg) = status_toast {
                div { class: "fixed bottom-[152px] lg:bottom-24 left-1/2 -translate-x-1/2 z-50 px-4 w-full max-w-md",
                    UndoSnackbar {
                        message: msg,
                        action_label: "OK".to_string(),
                        duration_ms: 3500,
                        on_undo: move |_| studio.status_toast.set(None),
                        on_dismiss: move |_| studio.status_toast.set(None),
                    }
                }
            }

            // Shared Add-to-Playlist Picker Modal
            if let Some(track) = studio.add_to_playlist_track.read().clone() {
                AddToPlaylistModal {
                    track,
                    on_close: move |_| studio.add_to_playlist_track.set(None),
                }
            }

            // Multi-Listener Delete Guard Sheet for permanent library removals (FAIL-CLOSED while loading/error!)
            if let Some(track) = guard_modal_track {
                {
                    let track_confirm = track.clone();
                    let preview = guard_preview
                        .filter(|p| p.song_id == track.id)
                        .unwrap_or_else(|| {
                            fail_closed_guard_preview(
                                track.id.clone(),
                                "Verifying household listener protection…".into(),
                            )
                        });
                    let preview_confirm = preview.clone();
                    rsx! {
                        MultiListenerGuardModal {
                            preview,
                            track_title: track.title.clone(),
                            track_artist: Some(track.artist.clone()),
                            busy: guard_loading,
                            on_confirm: move |_| {
                                studio.confirm_library_remove(
                                    player,
                                    track_confirm.clone(),
                                    preview_confirm.clone(),
                                );
                            },
                            on_cancel: move |_| studio.guard_modal_track.set(None),
                        }
                    }
                }
            }

            // Accessible Ctrl+K Command Palette
            CommandPalette {
                is_open: (studio.cmdk_open)(),
                commands: palette_commands,
                on_select: move |cmd_id: String| {
                    match cmd_id.as_str() {
                        "nav.home" => studio.active_section.set(StudioNavSection::Home),
                        "nav.discover" => studio.active_section.set(StudioNavSection::Discover),
                        "nav.search" => studio.active_section.set(StudioNavSection::Search),
                        "nav.library" => studio.active_section.set(StudioNavSection::Library),
                        "nav.activity" => studio.active_section.set(StudioNavSection::Activity),
                        "nav.settings" => studio.active_section.set(StudioNavSection::Settings),
                        "play.toggle" => player.toggle_play_pause(),
                        "play.next" => player.next(),
                        "play.prev" => player.previous(),
                        "play.mute" => player.toggle_mute(),
                        "play.mini" => studio.toggle_mini_player(player),
                        "play.replaygain" => {
                            let next = studio.config.peek().replaygain.cycle();
                            studio.set_replaygain(next);
                        }
                        "mode.offline" => player.toggle_offline_only(),
                        "sync.now" => studio.sync_now(false),
                        "drawer.queue" => studio.toggle_drawer(DrawerTab::Queue),
                        "drawer.lyrics" => studio.toggle_drawer(DrawerTab::Lyrics),
                        "drawer.inspector" => studio.toggle_drawer(DrawerTab::Inspector),
                        _ => {}
                    }
                },
                on_search_query: Some(EventHandler::new(move |q: String| {
                    studio.search_seed_query.set(q);
                    studio.active_section.set(StudioNavSection::Search);
                })),
                on_close: move |_| studio.cmdk_open.set(false),
            }
        }
    }
}

fn build_palette_commands(
    offline_only: bool,
    mini_open: bool,
    rg: audio_mpv::ReplayGainMode,
) -> Vec<PaletteCommand> {
    vec![
        PaletteCommand {
            id: "nav.home".into(),
            label: "Go to Home".into(),
            subtitle: Some("Recent albums & starred favorites".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["jump".into(), "start".into()],
        },
        PaletteCommand {
            id: "nav.discover".into(),
            label: "Go to Discover & Triage".into(),
            subtitle: Some("Pending discovery queue & seasonal packs".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["radar".into(), "seasonal".into(), "triage".into()],
        },
        PaletteCommand {
            id: "nav.search".into(),
            label: "Go to Search".into(),
            subtitle: Some("0ms local SQLite FTS5 & MusicBrainz".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["find".into(), "catalog".into()],
        },
        PaletteCommand {
            id: "nav.library".into(),
            label: "Go to Library & Offline Cache".into(),
            subtitle: Some("Playlists, albums, and offline pinning".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["cache".into(), "pin".into()],
        },
        PaletteCommand {
            id: "nav.activity".into(),
            label: "Go to Activity & Outbox".into(),
            subtitle: Some("Offline mutation outbox & Connect status".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["outbox".into(), "sync".into()],
        },
        PaletteCommand {
            id: "nav.settings".into(),
            label: "Go to Settings".into(),
            subtitle: Some("libmpv ReplayGain, OLED mode & server config".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["audio".into(), "mpv".into(), "oled".into()],
        },
        PaletteCommand {
            id: "play.toggle".into(),
            label: "Play / Pause".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("Space".into()),
            keywords: vec!["resume".into()],
        },
        PaletteCommand {
            id: "play.next".into(),
            label: "Next Track".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("Shift+→".into()),
            keywords: vec!["skip".into()],
        },
        PaletteCommand {
            id: "play.prev".into(),
            label: "Previous Track".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("Shift+←".into()),
            keywords: vec!["back".into()],
        },
        PaletteCommand {
            id: "play.mini".into(),
            label: if mini_open {
                "Close Floating Mini-Player".into()
            } else {
                "Switch to Floating Mini-Player (360×110)".into()
            },
            subtitle: Some("Always-on-top compact desktop player".into()),
            category: PaletteCategory::Playback,
            shortcut: None,
            keywords: vec!["pip".into(), "float".into(), "compact".into()],
        },
        PaletteCommand {
            id: "play.replaygain".into(),
            label: format!("Cycle ReplayGain (Current: {})", rg.label()),
            subtitle: Some("Track → Album → Off".into()),
            category: PaletteCategory::Playback,
            shortcut: None,
            keywords: vec!["loudness".into(), "mpv".into()],
        },
        PaletteCommand {
            id: "mode.offline".into(),
            label: if offline_only {
                "Disable Offline Only Mode".into()
            } else {
                "Enable Offline Only Mode".into()
            },
            subtitle: Some("Play strictly from local cached audio".into()),
            category: PaletteCategory::Playback,
            shortcut: None,
            keywords: vec!["airplane".into(), "cache".into()],
        },
        PaletteCommand {
            id: "sync.now".into(),
            label: "Sync Library & Flush Outbox".into(),
            subtitle: Some("Delta sync Navidrome & Soulbeet discovery".into()),
            category: PaletteCategory::Curation,
            shortcut: None,
            keywords: vec!["refresh".into(), "delta".into()],
        },
        PaletteCommand {
            id: "drawer.queue".into(),
            label: "Toggle Up Next Queue".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("Q".into()),
            keywords: vec![],
        },
        PaletteCommand {
            id: "drawer.lyrics".into(),
            label: "Toggle Synced Lyrics".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("L".into()),
            keywords: vec![],
        },
        PaletteCommand {
            id: "drawer.inspector".into(),
            label: "Toggle Track Inspector".into(),
            subtitle: Some("libmpv signal chain & multi-listener guard".into()),
            category: PaletteCategory::Curation,
            shortcut: Some("I".into()),
            keywords: vec!["codec".into(), "flac".into()],
        },
    ]
}

#[component]
fn AddToPlaylistModal(track: PlayerTrack, on_close: EventHandler<()>) -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let offline_only = player.offline_only();
    let has_server = studio
        .config
        .read()
        .soulbeet_base_url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty());
    let mut filter_text = use_signal(String::new);
    let mut new_name = use_signal(String::new);
    let mutating = (studio.playlist_mutating)();
    let can_edit = !offline_only && has_server && !mutating;

    let editable_playlists: Vec<_> = studio
        .playlists
        .read()
        .iter()
        .filter(|item| {
            let cat = classify_local_playlist_category(
                &item.playlist.kind,
                item.playlist.comment.as_deref(),
                item.playlist.is_smart,
            );
            !item.playlist.is_smart && !cat.is_soulbeet_mix()
        })
        .cloned()
        .collect();

    let query = filter_text().trim().to_ascii_lowercase();
    let filtered_playlists: Vec<_> = if query.is_empty() {
        editable_playlists.clone()
    } else {
        editable_playlists
            .iter()
            .filter(|p| p.playlist.name.to_ascii_lowercase().contains(&query))
            .cloned()
            .collect()
    };

    let track_id_create = track.id.clone();

    rsx! {
        Modal {
            on_close: move |_| on_close.call(()),
            header: rsx! {
                div { class: "min-w-0",
                    h2 { class: "text-base font-semibold text-beet-text-primary truncate",
                        "Add to Playlist"
                    }
                    p { class: "text-[12px] text-beet-text-secondary truncate mt-0.5",
                        "{track.title} · {track.artist}"
                    }
                }
            },
            div { class: "space-y-4",
                if !has_server {
                    div { class: "p-3 rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 text-[12px] text-beet-crimson-text",
                        "Connect a Soulbeet server in Settings to edit playlists."
                    }
                } else if offline_only {
                    div { class: "p-3 rounded-[10px] bg-beet-surface-high border border-beet-border-default text-[12px] text-beet-text-secondary",
                        "Disable Offline Only mode to add tracks to playlists."
                    }
                }

                // Quick create new playlist with this track
                div { class: "space-y-2",
                    p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                        "Quick Create"
                    }
                    div { class: "flex items-center gap-2",
                        input {
                            r#type: "text",
                            value: "{new_name}",
                            placeholder: "New playlist name...",
                            disabled: !can_edit,
                            class: "flex-1 h-9 px-3 rounded-[8px] bg-beet-surface border border-beet-border-default text-[13px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none disabled:opacity-50",
                            oninput: move |evt| new_name.set(evt.value()),
                        }
                        button {
                            r#type: "button",
                            disabled: !can_edit || new_name().trim().is_empty(),
                            class: "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-[8px] bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold hover:brightness-105 disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none cursor-pointer shrink-0",
                            onclick: move |_| {
                                let name = new_name().trim().to_string();
                                if !name.is_empty() {
                                    studio.create_playlist(
                                        name,
                                        None,
                                        Some(false),
                                        Some(vec![track_id_create.clone()]),
                                    );
                                    on_close.call(());
                                }
                            },
                            IconPlus { size: 13 }
                            span { "Create & Add" }
                        }
                    }
                }

                // Existing editable playlists
                div { class: "space-y-2",
                    div { class: "flex items-center justify-between gap-2",
                        p { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                            "Your Playlists ({editable_playlists.len()})"
                        }
                    }

                    if editable_playlists.len() > 5 {
                        input {
                            r#type: "text",
                            value: "{filter_text}",
                            placeholder: "Filter playlists...",
                            class: "w-full h-8 px-3 rounded-[8px] bg-beet-surface border border-beet-border-subtle text-[12px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none",
                            oninput: move |evt| filter_text.set(evt.value()),
                        }
                    }

                    if filtered_playlists.is_empty() {
                        p { class: "text-[12px] text-beet-text-tertiary py-3 text-center",
                            if editable_playlists.is_empty() {
                                "No editable playlists yet. Create one above."
                            } else {
                                "No playlists match your filter."
                            }
                        }
                    } else {
                        div { class: "space-y-1.5 max-h-64 overflow-y-auto studio-scrollbar",
                            for item in filtered_playlists.iter() {
                                {
                                    let pid = item.playlist.id.clone();
                                    let pname = item.playlist.name.clone();
                                    let tid = track.id.clone();
                                    rsx! {
                                        button {
                                            key: "pick-pl-{item.playlist.id}",
                                            r#type: "button",
                                            disabled: !can_edit,
                                            class: "w-full flex items-center justify-between gap-3 px-3 py-2 rounded-[10px] bg-beet-surface border border-beet-border-subtle hover:border-beet-leaf-solid/40 hover:bg-beet-surface-high text-left disabled:opacity-40 disabled:cursor-not-allowed disabled:pointer-events-none cursor-pointer transition-colors",
                                            onclick: move |_| {
                                                studio.add_tracks_to_playlist(
                                                    pid.clone(),
                                                    pname.clone(),
                                                    vec![tid.clone()],
                                                );
                                                on_close.call(());
                                            },
                                            div { class: "flex items-center gap-2.5 min-w-0",
                                                IconListMusic { size: 14, class: "text-beet-text-tertiary shrink-0".to_string() }
                                                span { class: "text-[13px] font-medium text-beet-text-primary truncate",
                                                    "{item.playlist.name}"
                                                }
                                            }
                                            span { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary shrink-0",
                                                "{item.playlist.song_count} TRACKS"
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
}
