//! 3-pane adaptive Refined Midnight Studio shell (`StudioShell`) replacing the legacy navbar.

use dioxus::prelude::*;
use dioxus::router::Navigator;
use shared::changelog::{changelog, running_version};
use shared::client::{ConnectClientMessage, ConnectCommand};
use shared::download::DownloadableItem;
use shared::recommendation::ContextualMixKind;
use shared::system::NavidromeStatus;
use ui::{
    format_mix_title, friendly_error, use_player, CommandPalette, Downloads, DrawerTab,
    IconLoader, IconPlay, Modal, MultiListenerGuardModal, PaletteCategory, PaletteCommand,
    SearchPrefill, SearchReset, SidebarPlaylistItem, StatusAction, StatusBanner, StudioMobileNav,
    StudioNavSection, StudioPlayerBar, StudioRightDrawer, StudioSidebar, StudioTopBar,
    UndoSnackbar, WhatsNew,
};

use crate::auth::use_auth;
use crate::shortcuts::use_studio_shortcuts;
use crate::views::{SettingsTab, SettingsTabRequest, SsoLinkNotice};
use crate::web_player::{
    build_connect_snapshot, use_studio, ManualChoiceModalState, WebStudioProvider,
};
use crate::Route;

/// Outer layout component wired into Dioxus Router for all authenticated routes.
#[component]
pub fn StudioShell() -> Element {
    rsx! {
        WebStudioProvider {
            StudioShellContent {}
        }
    }
}

#[component]
fn StudioShellContent() -> Element {
    let mut auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let nav = use_navigator();
    let route = use_route::<Route>();

    // Install global keyboard shortcuts (Space, Arrows, S, 1-5, P, X, L, Q, I, R, M, Ctrl+K)
    use_studio_shortcuts();

    // Contexts expected by existing Search & Settings views
    let search_reset = use_signal(|| 0);
    let search_prefill = use_signal(|| None::<(String, String)>);
    let settings_tab_request = use_signal(|| None::<SettingsTab>);
    let sso_link_notice = use_signal(|| None::<api::auth::SsoLinkResult>);
    use_context_provider(|| SearchReset(search_reset));
    use_context_provider(|| SearchPrefill(search_prefill));
    use_context_provider(|| SettingsTabRequest(settings_tab_request));
    use_context_provider(|| SsoLinkNotice(sso_link_notice));

    // Changelog / "What's new" check (preserved from ui::Layout)
    let seen = use_resource(move || async move { auth.call(api::get_changelog_seen()).await });
    let mut auto_closed = use_signal(|| false);
    let mut showing_all_whats_new = use_signal(|| false);
    let unseen_notes = match &*seen.read() {
        Some(Ok(version)) if !auto_closed() => changelog()
            .unseen(*version, running_version())
            .into_iter()
            .cloned()
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    let mut logout_error = use_signal(|| None::<String>);
    let do_logout = move || {
        spawn(async move {
            // Flush any staged pending-discovery drop before ending the session.
            studio.flush_pending_drops_now(auth).await;
            let failed = auth.logout().await.err();
            logout_error.set(failed.map(|e| {
                format!(
                    "Sign-out failed, you are still signed in. {}",
                    friendly_error(&e)
                )
            }));
        });
    };

    let active_section = route_to_section(&route);
    let offline_only = player.offline_only();
    let active_transfers = studio.active_transfers_count();
    let acquiring_in_flight = studio.has_active_acquisitions();

    let pinned_seasonal = studio
        .contextual_mixes
        .read()
        .iter()
        .find(|m| m.kind == ContextualMixKind::Seasonal)
        .cloned();

    let sidebar_playlists: Vec<SidebarPlaylistItem> = studio
        .playlists
        .read()
        .iter()
        .map(|p| SidebarPlaylistItem {
            id: p.id.clone(),
            name: p.name.clone(),
            song_count: p.song_count,
            is_smart: p.is_smart,
            is_offline_pinned: false,
        })
        .collect();

    let culture_badge = {
        let prof = studio.culture_profile.read();
        prof.effective_country().map(|cc| {
            let pct = (prof.local_culture_ratio.clamp(0.0, 1.0) * 100.0).round() as u8;
            format!("{} · {pct}% LOCAL", cc.to_ascii_uppercase())
        })
    };

    // Build dynamic Command Palette items
    let palette_commands = build_palette_commands(&studio, &player);

    let active_drop = studio.pending_drops.read().active().cloned();
    let current_guard_modal = studio.guard_modal.read().clone();
    let current_manual_modal = studio.manual_choice_modal.read().clone();
    let current_toast = studio.toast_notice.read().clone();

    rsx! {
        div { class: "h-screen w-screen overflow-hidden flex flex-col bg-beet-canvas text-beet-text-primary",
            // Main 3-Pane Body (Left Rail + Center Stage + Right Drawer)
            div { class: "flex-1 min-h-0 flex overflow-hidden",
                StudioSidebar {
                    active: active_section,
                    on_navigate: move |section| navigate_to_section(nav, section),
                    offline_only,
                    on_toggle_offline: move |next| player.set_offline_only(next),
                    active_transfers_count: active_transfers,
                    pinned_seasonal,
                    on_open_seasonal: move |_slug| {
                        nav.push(Route::DiscoverPage {});
                    },
                    playlists: sidebar_playlists,
                    on_select_playlist: move |pid| {
                        studio.selected_playlist_id.set(Some(pid));
                        nav.push(Route::LibraryPage {});
                    },
                    username: auth.username(),
                    on_whats_new: move |_| showing_all_whats_new.set(true),
                    on_logout: move |_| do_logout(),
                }

                // Center Stage Column
                div { class: "flex-1 min-w-0 flex flex-col overflow-hidden bg-beet-canvas",
                    StudioTopBar {
                        active: active_section,
                        offline_only,
                        on_toggle_offline: move |next| player.set_offline_only(next),
                        on_open_palette: move |_| studio.cmdk_open.set(true),
                        on_open_downloads: move |_| studio.downloads_drawer_open.toggle(),
                        on_downloads_toggle_mounted: move |data| {
                            studio.downloads_toggle_ref.set(Some(data));
                        },
                        on_logout: move |_| do_logout(),
                        active_transfers_count: active_transfers,
                        culture_badge,
                    }

                    // Status & System Health Banners
                    if (studio.outdated)() {
                        StatusBanner {
                            dot_class: "bg-beet-accent",
                            label: "UPDATE",
                            message: "Soulbeet was updated. Reload this page.".to_string(),
                            StatusAction {
                                label: "Reload",
                                onclick: |_| {
                                    #[cfg(feature = "web")]
                                    if let Some(window) = web_sys::window() {
                                        let _ = window.location().reload();
                                    }
                                },
                            }
                        }
                    }
                    ImporterBanner {}
                    NavidromeBanner {}
                    if let Some(message) = logout_error() {
                        StatusBanner {
                            dot_class: "bg-beet-accent",
                            label: "SIGN-OUT",
                            message,
                            StatusAction {
                                label: "Try again",
                                onclick: move |_| do_logout(),
                            }
                        }
                    }

                    // Scrollable Main View Content
                    main { class: "flex-1 min-h-0 overflow-y-auto studio-scrollbar px-4 sm:px-6 lg:px-8 py-6",
                        div { class: "max-w-6xl mx-auto w-full pb-10",
                            Outlet::<Route> {}
                        }
                    }
                }

                // Collapsible Right Contextual Drawer (Queue / Lyrics / Inspector)
                StudioRightDrawer {
                    active_tab: (studio.drawer_tab)(),
                    on_select_tab: move |tab| studio.drawer_tab.set(Some(tab)),
                    on_close: move |_| studio.drawer_tab.set(None),
                    inspected_track: (studio.inspected_track)(),
                    lyrics: (studio.lyrics)(),
                    lyrics_loading: (studio.lyrics_loading)(),
                    lyrics_error: (studio.lyrics_error)(),
                    on_fetch_lyrics: move |_| {
                        if let Some(track) = player.current_track() {
                            studio.fetch_lyrics_for(auth, track);
                        }
                    },
                    guard_preview: (studio.inspector_guard_preview)(),
                    guard_loading: (studio.inspector_guard_loading)(),
                    on_refresh_guard: move |song_id| studio.fetch_guard_preview(auth, song_id),
                    on_open_guard_modal: move |track| studio.drop_track(auth, player, track),
                    on_keep: move |track| studio.keep_track(auth, player, track),
                    on_drop: move |track| studio.drop_track(auth, player, track),
                    on_start_radio: move |track| studio.start_radio(auth, player, track),
                }
            }

            // Persistent 84px Bottom Player Bar
            StudioPlayerBar {
                active_drawer_tab: (studio.drawer_tab)(),
                on_toggle_drawer: move |tab| studio.toggle_drawer(tab),
                connect_devices: (studio.connect_devices)(),
                local_device_id: (studio.local_device_id)(),
                acquiring_in_flight,
                on_keep: move |track| studio.keep_track(auth, player, track),
                on_drop: move |track| studio.drop_track(auth, player, track),
                on_star: move |(track, starred)| studio.star_track(auth, player, track, starred),
                on_rate: move |(track, rating)| studio.rate_track(auth, player, track, rating),
                on_start_radio: move |track| studio.start_radio(auth, player, track),
                on_claim_active_device: move |_| {
                    studio.send_connect(ConnectClientMessage::ClaimActive);
                },
                on_transfer_to_device: move |target_id| {
                    let from_id = studio.local_device_id.peek().clone();
                    let snapshot = build_connect_snapshot(player);
                    studio.send_connect(ConnectClientMessage::SendCommand {
                        target_device_id: target_id,
                        command: ConnectCommand::TransferPlayback {
                            from_device_id: from_id,
                            snapshot,
                        },
                    });
                    if player.is_playing() {
                        player.toggle_play_pause();
                    }
                    studio.show_toast("Playback handed off to remote device");
                },
            }

            // Mobile Bottom Navigation (< lg)
            StudioMobileNav {
                active: active_section,
                on_navigate: move |section| navigate_to_section(nav, section),
            }

            // Floating 5-Second Undo Snackbar (Discovery Drops)
            if let Some(drop_item) = active_drop {
                {
                    let drop_key = drop_item.key.clone();
                    rsx! {
                        div { class: "fixed bottom-[100px] left-1/2 -translate-x-1/2 z-50 px-4 w-full max-w-md",
                            UndoSnackbar {
                                message: drop_item.snackbar_message(),
                                action_label: "Undo".to_string(),
                                duration_ms: 5000,
                                key_id: Some(drop_key.clone()),
                                on_undo: move |_| studio.undo_pending_drop(),
                                on_dismiss: move |_| studio.dismiss_and_commit_active_drop(auth, drop_key.clone()),
                            }
                        }
                    }
                }
            }

            // Floating Subtle Toast Notice (Keep / Radio / Transfers)
            if let Some(msg) = current_toast {
                div {
                    role: "status",
                    aria_live: "polite",
                    class: "fixed top-16 right-6 z-50 px-4 py-2.5 rounded-[10px] bg-beet-surface-high border border-beet-border-default shadow-xl text-[13px] font-medium text-beet-text-primary max-w-sm",
                    "{msg}"
                }
            }

            // Multi-Listener Delete Guard Modal
            if let Some(modal_state) = current_guard_modal {
                MultiListenerGuardModal {
                    preview: modal_state.preview,
                    track_title: modal_state.track.title,
                    track_artist: Some(modal_state.track.artist),
                    busy: modal_state.busy,
                    on_confirm: move |_| studio.confirm_guard_modal(auth, player),
                    on_cancel: move |_| studio.guard_modal.set(None),
                }
            }

            // Manual Source Choice Modal (from Acquire & Play NeedsManualChoice)
            if let Some(choice_state) = current_manual_modal {
                ManualSourceChoiceModal {
                    state: choice_state,
                    on_close: move |_| studio.manual_choice_modal.set(None),
                }
            }

            // Ctrl+K Command Palette
            CommandPalette {
                is_open: (studio.cmdk_open)(),
                commands: palette_commands,
                on_select: move |cmd_id: String| {
                    if cmd_id == "auth.logout" {
                        do_logout();
                    } else {
                        execute_palette_command(&cmd_id, nav, auth, player, studio);
                    }
                },
                on_search_query: move |q| {
                    studio.search_query_override.set(Some(q));
                    nav.push(Route::SearchPage {});
                },
                on_close: move |_| studio.cmdk_open.set(false),
            }

            // Legacy / Deep Downloads & Beets Conflict Resolution Drawer
            Downloads {
                is_open: studio.downloads_drawer_open,
                downloads: studio.downloads,
                return_focus_to: studio.downloads_toggle_ref,
            }

            // Release Notes Modal (manual "What's new" or when a new version ships)
            if showing_all_whats_new() {
                WhatsNew {
                    sections: changelog().sections.clone(),
                    on_close: move |_| showing_all_whats_new.set(false),
                }
            } else if !unseen_notes.is_empty() {
                WhatsNew {
                    sections: unseen_notes,
                    on_close: move |_| {
                        auto_closed.set(true);
                        spawn(async move {
                            let _ = auth.call(api::mark_changelog_seen(running_version())).await;
                        });
                    },
                }
            }
        }
    }
}

fn route_to_section(route: &Route) -> StudioNavSection {
    match route {
        Route::HomePage {} => StudioNavSection::Home,
        Route::DiscoverPage {} => StudioNavSection::Discover,
        Route::SearchPage {} => StudioNavSection::Search,
        Route::LibraryPage {} | Route::DashboardPage {} => StudioNavSection::Library,
        Route::ActivityPage {} => StudioNavSection::Activity,
        Route::SettingsPage {} | Route::SsoLinkResultPage { .. } => StudioNavSection::Settings,
        _ => StudioNavSection::Home,
    }
}

fn navigate_to_section(nav: Navigator, section: StudioNavSection) {
    match section {
        StudioNavSection::Home => {
            nav.push(Route::HomePage {});
        }
        StudioNavSection::Discover => {
            nav.push(Route::DiscoverPage {});
        }
        StudioNavSection::Search => {
            nav.push(Route::SearchPage {});
        }
        StudioNavSection::Library => {
            nav.push(Route::LibraryPage {});
        }
        StudioNavSection::Activity => {
            nav.push(Route::ActivityPage {});
        }
        StudioNavSection::Settings => {
            nav.push(Route::SettingsPage {});
        }
    }
}

fn build_palette_commands(
    studio: &crate::web_player::WebStudioContext,
    player: &ui::PlayerContext,
) -> Vec<PaletteCommand> {
    let mut cmds = vec![
        PaletteCommand {
            id: "nav.home".into(),
            label: "Go to Home".into(),
            subtitle: Some("Quick resume, seasonal hero & mixes".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["dashboard".into(), "start".into()],
        },
        PaletteCommand {
            id: "nav.discover".into(),
            label: "Go to Discover".into(),
            subtitle: Some("Triage deck, seasonal steering & Taste DNA".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["triage".into(), "radar".into(), "seasonal".into()],
        },
        PaletteCommand {
            id: "nav.search".into(),
            label: "Go to Search".into(),
            subtitle: Some("Unified library + MusicBrainz Acquire & Play".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["find".into(), "acquire".into(), "musicbrainz".into()],
        },
        PaletteCommand {
            id: "nav.library".into(),
            label: "Go to Library".into(),
            subtitle: Some("Playlists, albums, starred & stats".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["playlists".into(), "albums".into(), "starred".into()],
        },
        PaletteCommand {
            id: "nav.activity".into(),
            label: "Go to Activity".into(),
            subtitle: Some("Downloads, Acquire & Play & archival FLAC upgrades".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["downloads".into(), "flac".into(), "upgrades".into()],
        },
        PaletteCommand {
            id: "nav.settings".into(),
            label: "Go to Settings".into(),
            subtitle: Some("Folders, Navidrome, users & config".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["config".into(), "navidrome".into()],
        },
        PaletteCommand {
            id: "auth.logout".into(),
            label: "Sign Out".into(),
            subtitle: Some("Flush pending curation and end session".into()),
            category: PaletteCategory::Navigation,
            shortcut: None,
            keywords: vec!["logout".into(), "account".into(), "exit".into()],
        },
        PaletteCommand {
            id: "play.toggle".into(),
            label: "Play / Pause".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("Space".into()),
            keywords: vec!["resume".into(), "stop".into()],
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
            id: "play.mute".into(),
            label: "Toggle Mute".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("M".into()),
            keywords: vec!["volume".into(), "silence".into()],
        },
        PaletteCommand {
            id: "toggle.offline".into(),
            label: if player.offline_only() {
                "Disable Offline Only Mode".into()
            } else {
                "Enable Offline Only Mode".into()
            },
            subtitle: Some("Filter views to locally cached tracks".into()),
            category: PaletteCategory::Playback,
            shortcut: None,
            keywords: vec!["cache".into(), "network".into()],
        },
        PaletteCommand {
            id: "drawer.lyrics".into(),
            label: "Toggle Synced Lyrics".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("L".into()),
            keywords: vec!["karaoke".into(), "words".into()],
        },
        PaletteCommand {
            id: "drawer.queue".into(),
            label: "Toggle Up Next Queue".into(),
            subtitle: None,
            category: PaletteCategory::Playback,
            shortcut: Some("Q".into()),
            keywords: vec!["playlist".into()],
        },
        PaletteCommand {
            id: "drawer.inspector".into(),
            label: "Toggle Track Inspector".into(),
            subtitle: Some("Provenance, signal chain & delete guard".into()),
            category: PaletteCategory::Playback,
            shortcut: Some("I".into()),
            keywords: vec!["info".into(), "guard".into(), "codec".into()],
        },
    ];

    if let Some(cur) = player.current_track() {
        if cur.is_discovery {
            cmds.push(PaletteCommand {
                id: "curate.keep".into(),
                label: format!("Keep “{}” in Permanent Library", cur.title),
                subtitle: Some(cur.artist.clone()),
                category: PaletteCategory::Curation,
                shortcut: Some("P".into()),
                keywords: vec!["promote".into(), "save".into(), "discovery".into()],
            });
        }
        cmds.push(PaletteCommand {
            id: "curate.star".into(),
            label: if cur.starred {
                format!("Unstar “{}”", cur.title)
            } else {
                format!("Star “{}”", cur.title)
            },
            subtitle: Some(cur.artist.clone()),
            category: PaletteCategory::Curation,
            shortcut: Some("S".into()),
            keywords: vec!["favorite".into(), "love".into()],
        });
        cmds.push(PaletteCommand {
            id: "curate.drop".into(),
            label: if cur.is_discovery {
                format!("Drop “{}” from Discovery", cur.title)
            } else {
                format!("Remove “{}” (Guard Check)", cur.title)
            },
            subtitle: Some(cur.artist.clone()),
            category: PaletteCategory::Curation,
            shortcut: Some("X".into()),
            keywords: vec!["delete".into(), "remove".into(), "skip".into()],
        });
        cmds.push(PaletteCommand {
            id: "curate.radio".into(),
            label: format!("Start Radio from “{}”", cur.title),
            subtitle: Some(cur.artist),
            category: PaletteCategory::Mixes,
            shortcut: Some("R".into()),
            keywords: vec!["seed".into(), "similar".into()],
        });
    }

    for mix in studio.contextual_mixes.read().iter() {
        cmds.push(PaletteCommand {
            id: format!("mix.{}", mix.slug),
            label: format!("Play {}", format_mix_title(mix)),
            subtitle: Some(format!("{} tracks", mix.track_count)),
            category: PaletteCategory::Mixes,
            shortcut: None,
            keywords: vec![mix.slug.clone(), "mix".into(), "seasonal".into()],
        });
    }

    cmds
}

fn execute_palette_command(
    cmd_id: &str,
    nav: Navigator,
    auth: ui::Auth,
    player: ui::PlayerContext,
    mut studio: crate::web_player::WebStudioContext,
) {
    match cmd_id {
        "nav.home" => navigate_to_section(nav, StudioNavSection::Home),
        "nav.discover" => navigate_to_section(nav, StudioNavSection::Discover),
        "nav.search" => navigate_to_section(nav, StudioNavSection::Search),
        "nav.library" => navigate_to_section(nav, StudioNavSection::Library),
        "nav.activity" => navigate_to_section(nav, StudioNavSection::Activity),
        "nav.settings" => navigate_to_section(nav, StudioNavSection::Settings),
        "play.toggle" => player.toggle_play_pause(),
        "play.next" => player.next(),
        "play.prev" => player.previous(),
        "play.mute" => player.toggle_mute(),
        "toggle.offline" => player.toggle_offline_only(),
        "drawer.lyrics" => studio.toggle_drawer(DrawerTab::Lyrics),
        "drawer.queue" => studio.toggle_drawer(DrawerTab::Queue),
        "drawer.inspector" => studio.toggle_drawer(DrawerTab::Inspector),
        "curate.keep" => {
            if let Some(track) = player.current_track() {
                studio.keep_track(auth, player, track);
            }
        }
        "curate.star" => {
            if let Some(track) = player.current_track() {
                let next = !track.starred;
                studio.star_track(auth, player, track, next);
            }
        }
        "curate.drop" => {
            if let Some(track) = player.current_track() {
                studio.drop_track(auth, player, track);
            }
        }
        "curate.radio" => {
            if let Some(track) = player.current_track() {
                studio.start_radio(auth, player, track);
            }
        }
        other => {
            if let Some(slug) = other.strip_prefix("mix.") {
                studio.play_contextual_mix(auth, player, slug.to_string());
            }
        }
    }
}

/// Modal picker opened when `AcquireAndPlayStage::NeedsManualChoice` is clicked on a track row.
#[component]
fn ManualSourceChoiceModal(
    state: ManualChoiceModalState,
    on_close: EventHandler<()>,
) -> Element {
    let auth = use_auth();
    let settings = ui::use_settings();
    let mut studio = use_studio();
    let mut queueing = use_signal(|| false);
    let mut err_msg = use_signal(|| None::<String>);

    let acquire_state_key = crate::web_player::acquire_key(&state.artist, &state.title);
    let queue_item = use_callback(move |(item, search_id): (DownloadableItem, String)| {
        queueing.set(true);
        err_msg.set(None);
        let preferred_folder_id = settings.get().and_then(|s| s.default_download_folder_id);
        let acq_key = acquire_state_key.clone();
        spawn(async move {
            let target_folder = match auth.call(api::get_user_folders()).await {
                Ok(folders) => preferred_folder_id
                    .as_deref()
                    .and_then(|pref| folders.iter().find(|f| f.id == pref))
                    .or_else(|| folders.first())
                    .map(|f| f.path.clone()),
                Err(_) => None,
            };
            let Some(target_folder) = target_folder else {
                err_msg.set(Some("No download folder configured. Set one in Settings.".to_string()));
                queueing.set(false);
                return;
            };
            let req = api::DownloadRequest {
                items: vec![item.clone()],
                target_folder,
                search_id,
            };
            match auth.call(api::download(req)).await {
                Ok(_) => {
                    studio.acquire_states.write().remove(&acq_key);
                    studio.show_toast(format!("Queued \"{}\" for download", item.title));
                    studio.manual_choice_modal.set(None);
                    studio.downloads_drawer_open.set(true);
                }
                Err(e) => {
                    err_msg.set(Some(friendly_error(&e)));
                    queueing.set(false);
                }
            }
        });
    });

    rsx! {
        Modal {
            on_close: move |_| on_close.call(()),
            header: rsx! {
                div {
                    h2 { class: "text-[17px] font-semibold text-beet-text-primary",
                        "Pick a source for “{state.title}”"
                    }
                    p { class: "text-[12px] text-beet-text-secondary mt-0.5",
                        "{state.artist} · automatic match was below confidence threshold"
                    }
                }
            },
            div { class: "space-y-3",
                if let Some(err) = err_msg() {
                    p { class: "text-[13px] text-beet-crimson-text bg-beet-crimson-surface border border-beet-crimson-text/30 rounded-[8px] px-3 py-2",
                        "{err}"
                    }
                }
                if state.groups.is_empty() {
                    p { class: "text-[13px] text-beet-text-secondary py-4 text-center",
                        "No candidate sources were returned. Try Advanced Soulseek Search."
                    }
                } else {
                    for group in state.groups.iter() {
                        div {
                            key: "{group.group_id}-{group.source}",
                            class: "rounded-[10px] bg-beet-surface-low border border-beet-border-subtle p-3 space-y-2",
                            div { class: "flex items-center justify-between gap-2",
                                div { class: "min-w-0",
                                    p { class: "text-[13px] font-semibold text-beet-text-primary truncate",
                                        "{group.title}"
                                    }
                                    p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                        "{group.source} · {group.quality} · Score {group.score:.2}"
                                    }
                                }
                            }
                            div { class: "space-y-1",
                                for item in group.items.iter().take(5) {
                                    {
                                        let item_c = item.clone();
                                        let sid = state.search_id.clone();
                                        let match_pct = format!("{:.0}", item.match_score * 100.0);
                                        rsx! {
                                            div {
                                                key: "{item.id}",
                                                class: "flex items-center justify-between gap-2 px-2.5 py-1.5 rounded-[7px] bg-beet-surface hover:bg-beet-surface-high transition-colors",
                                                div { class: "min-w-0",
                                                    p { class: "text-[12px] text-beet-text-primary truncate",
                                                        "{item.title}"
                                                    }
                                                    p { class: "tabular-nums-mono text-[10px] text-beet-text-tertiary",
                                                        "{item.quality} · Match {match_pct}%"
                                                    }
                                                }
                                                button {
                                                    r#type: "button",
                                                    disabled: queueing(),
                                                    class: "inline-flex items-center gap-1 h-7 px-3 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[11px] font-semibold hover:brightness-105 disabled:opacity-50 cursor-pointer shrink-0",
                                                    onclick: move |_| queue_item.call((item_c.clone(), sid.clone())),
                                                    if queueing() {
                                                        IconLoader { size: 12 }
                                                    } else {
                                                        IconPlay { size: 11, filled: true }
                                                    }
                                                    span { "Acquire" }
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
}

/// Preserved from legacy `web/src/main.rs`: shown while beets cannot import.
#[component]
fn ImporterBanner() -> Element {
    let health = ui::use_system_health();
    let SettingsTabRequest(mut requested_tab) = use_context();

    let importer = health.importer();
    let Some(issue) = importer.blocking_issue() else {
        return rsx! {};
    };

    rsx! {
        StatusBanner {
            dot_class: ui::importer_chip(importer.level).0,
            label: "BEETS",
            message: issue.title.clone(),
            Link {
                class: ui::STATUS_ACTION_CLASS,
                to: Route::SettingsPage {},
                onclick: move |_| requested_tab.set(Some(SettingsTab::Config)),
                "Fix in Config"
            }
        }
    }
}

/// Preserved Navidrome status banner helper (also tested in `web/src/main.rs`).
pub(crate) fn navidrome_banner(status: NavidromeStatus) -> Option<(&'static str, &'static str, bool)> {
    match status {
        NavidromeStatus::InvalidCredentials => Some((
            "bg-beet-accent",
            "Navidrome refused your password. Enter your current Navidrome password under \
             Connect Navidrome.",
            true,
        )),
        NavidromeStatus::NotLinked => Some((
            "bg-beet-accent",
            "Not connected. Connect Navidrome to use discovery and rating sync.",
            true,
        )),
        NavidromeStatus::Offline => Some((
            "bg-gray-500",
            "Unreachable. Discovery and rating sync paused.",
            false,
        )),
        NavidromeStatus::Connected | NavidromeStatus::Unknown => None,
    }
}

#[component]
fn NavidromeBanner() -> Element {
    let auth = use_auth();
    let SettingsTabRequest(mut requested_tab) = use_context();
    let mut settings = ui::use_settings();
    let health = ui::use_system_health();
    let status = auth.navidrome_status();

    let dismissed = settings
        .get()
        .map(|s| s.navidrome_banner_dismissed)
        .unwrap_or(false);

    let back_online = matches!(status, NavidromeStatus::Offline) && health.navidrome_online();
    if dismissed || back_online {
        return rsx! {};
    }

    let Some((dot_color, message, connect)) = navidrome_banner(status) else {
        return rsx! {};
    };

    let dismiss = move |_| {
        spawn(async move {
            let _ = settings
                .update(api::UpdateUserSettings {
                    navidrome_banner_dismissed: Some(true),
                    ..Default::default()
                })
                .await;
        });
    };

    rsx! {
        StatusBanner {
            dot_class: dot_color,
            label: "NAVIDROME",
            message: message.to_string(),
            if connect {
                Link {
                    class: ui::STATUS_ACTION_CLASS,
                    to: Route::SettingsPage {},
                    onclick: move |_| requested_tab.set(Some(SettingsTab::Users)),
                    "Connect"
                }
            }
            button {
                r#type: "button",
                aria_label: "Dismiss Navidrome banner",
                class: "shrink-0 text-beet-text-tertiary hover:text-beet-text-primary transition-colors cursor-pointer p-1.5",
                onclick: dismiss,
                ui::IconX { size: 14 }
            }
        }
    }
}
