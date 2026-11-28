//! Dedicated URL detail routes for Navidrome entities:
//! - `/playlist/:id` (`PlaylistDetailPage`) with full CRUD & track reordering
//! - `/album/:id` (`AlbumDetailPage`)
//! - `/artist/:id` (`ArtistDetailPage`)

use dioxus::prelude::*;
use shared::recommendation::SeedRadioRequest;
use ui::{
    format_duration, friendly_error, subsonic_cover_art_url, use_player, ConfirmModal,
    IconChevronLeft, IconCompass, IconCopy, IconDisc3, IconListMusic, IconListPlus, IconLoader,
    IconMoreHorizontal, IconPencil, IconPlay, IconRadio, IconShuffle, IconSparkles, IconTrash2,
    IconUser, PlayerTrack, PlaylistCategory, TrackRow, TrackRowState,
};

use crate::auth::use_auth;
use crate::navidrome_web::{
    fetch_album_detail, fetch_album_tracks, fetch_artist_detail, fetch_playlist_detail,
    SubsonicAlbumSummary, SubsonicArtistSummary, SubsonicPlaylistSummary,
};
use crate::web_player::use_studio;
use crate::Route;

/// Full Playlist Detail view (`/playlist/:id`) with Play, Shuffle, Rename/Edit, Duplicate,
/// Delete (for static user playlists), and per-track Move Up / Move Down / Remove / Add to playlist.
#[component]
pub fn PlaylistDetailPage(id: String) -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let nav = use_navigator();

    let mut summary = use_signal(|| None::<SubsonicPlaylistSummary>);
    let mut tracks = use_signal(Vec::<PlayerTrack>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut menu_open = use_signal(|| false);
    let mut confirm_delete_open = use_signal(|| false);
    let mut busy_mutation = use_signal(|| false);
    let mut req_id = use_signal(|| 0u64);
    let mut reload_tick = use_signal(|| 0u64);

    let id_for_load = id.clone();
    use_effect(move || {
        let _tick = reload_tick();
        let pid = id_for_load.clone();
        let nav_info = studio
            .bootstrap
            .read()
            .as_ref()
            .and_then(|b| b.navidrome.clone());
        let Some(info) = nav_info else {
            return;
        };
        let next_req = req_id.peek().wrapping_add(1);
        req_id.set(next_req);
        loading.set(true);
        error.set(None);
        spawn(async move {
            match fetch_playlist_detail(&info, &pid).await {
                Ok(Some((mut pl, list))) => {
                    if *req_id.peek() == next_req {
                        let mixes = studio.contextual_mixes.peek().clone();
                        pl.category = ui::classify_playlist_category(
                            &pl.id,
                            Some(&pl.comment),
                            pl.is_smart,
                            &mixes,
                        );
                        studio.sync_playlist_count(&pid, list.len() as u32);
                        summary.set(Some(pl));
                        tracks.set(list);
                        loading.set(false);
                    }
                }
                Ok(None) => {
                    if *req_id.peek() == next_req {
                        error.set(Some("Playlist not found.".to_string()));
                        loading.set(false);
                    }
                }
                Err(e) => {
                    if *req_id.peek() == next_req {
                        error.set(Some(e));
                        loading.set(false);
                    }
                }
            }
        });
    });

    // Reload metadata whenever global playlist edits finish (e.g. Rename modal closed).
    let mut prev_playlist_sig = use_signal(|| {
        studio
            .playlists
            .peek()
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.name.clone(), p.comment.clone(), p.public, p.song_count))
    });
    {
        let current_sig = studio
            .playlists
            .read()
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.name.clone(), p.comment.clone(), p.public, p.song_count));
        if current_sig.is_some() && current_sig != *prev_playlist_sig.peek() {
            prev_playlist_sig.set(current_sig);
            reload_tick += 1;
        }
    }

    let id_for_delete = id.clone();
    let handle_delete = move |_| {
        if busy_mutation() {
            return;
        }
        busy_mutation.set(true);
        let pid = id_for_delete.clone();
        spawn(async move {
            match auth.call(api::delete_playlist(pid)).await {
                Ok(()) => {
                    confirm_delete_open.set(false);
                    studio.refresh_playlists();
                    studio.show_toast("Playlist deleted");
                    nav.push(Route::LibraryPage {});
                }
                Err(err) => {
                    studio.show_toast(format!("Could not delete playlist: {}", friendly_error(&err)));
                }
            }
            busy_mutation.set(false);
        });
    };

    let offline_only = player.offline_only();

    let display_tracks: Vec<PlayerTrack> = tracks
        .read()
        .iter()
        .cloned()
        .map(|mut t| {
            studio.apply_curation_overrides(&mut t);
            t
        })
        .collect();

    rsx! {
        div { class: "space-y-6",
            div { class: "flex items-center justify-between gap-2",
                button {
                    r#type: "button",
                    class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                    onclick: move |_| {
                        nav.push(Route::LibraryPage {});
                    },
                    IconChevronLeft { size: 14 }
                    span { "Library" }
                }
            }

            if loading() && summary.read().is_none() {
                div { class: "py-16 flex items-center justify-center gap-2.5 text-beet-text-tertiary text-[13px]",
                    IconLoader { size: 16, class: "animate-spin text-beet-leaf-text".to_string() }
                    span { "Loading playlist..." }
                }
            } else if let Some(err) = error() {
                div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center space-y-2",
                    p { class: "text-[14px] font-semibold text-beet-text-primary",
                        "Could not load playlist"
                    }
                    p { class: "text-[13px] text-beet-text-secondary", "{err}" }
                }
            } else if let Some(pl) = summary.read().clone() {
                {
                    let is_static_editable = !pl.is_smart && !pl.category.is_soulbeet_mix();
                    let total_secs: u32 = if pl.duration > 0 {
                        pl.duration
                    } else {
                        display_tracks.iter().filter_map(|t| t.duration_secs).sum()
                    };
                    let tracks_for_play = display_tracks.clone();
                    let tracks_for_shuffle = display_tracks.clone();
                    let pl_for_edit = pl.clone();
                    let pl_for_dup = pl.clone();
                    let dup_song_ids: Vec<String> = display_tracks
                        .iter()
                        .map(|t| t.id.clone())
                        .filter(|sid| !sid.is_empty())
                        .collect();

                    let icon_wrap_cls = match pl.category {
                        PlaylistCategory::Seasonal | PlaylistCategory::Discovery => {
                            "bg-beet-accent-surface border-beet-accent/30 text-beet-accent-text"
                        }
                        PlaylistCategory::MomentMix => {
                            "bg-beet-leaf-surface border-beet-leaf-solid/30 text-beet-leaf-text"
                        }
                        PlaylistCategory::Smart | PlaylistCategory::User => {
                            "bg-beet-surface-high border-beet-border-default text-beet-text-secondary"
                        }
                    };

                    rsx! {
                        // Playlist Header
                        section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-5 sm:p-6 flex flex-col sm:flex-row items-start sm:items-end gap-5",
                            div { class: "w-28 h-28 sm:w-36 sm:h-36 rounded-[12px] border flex items-center justify-center shrink-0 {icon_wrap_cls}",
                                match pl.category {
                                    PlaylistCategory::Seasonal => rsx! { IconSparkles { size: 42 } },
                                    PlaylistCategory::Discovery => rsx! { IconCompass { size: 42 } },
                                    PlaylistCategory::MomentMix => rsx! { IconRadio { size: 42 } },
                                    PlaylistCategory::Smart | PlaylistCategory::User => rsx! { IconListMusic { size: 42 } },
                                }
                            }

                            div { class: "flex-1 min-w-0 space-y-2.5",
                                div { class: "flex flex-wrap items-center gap-2",
                                    span { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider px-2 py-0.5 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-tertiary",
                                        if let Some(tag) = pl.category.badge_label() {
                                            "{tag} PLAYLIST"
                                        } else {
                                            "PLAYLIST"
                                        }
                                    }
                                    if pl.public {
                                        span { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider px-2 py-0.5 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/25 text-beet-leaf-text",
                                            "PUBLIC"
                                        }
                                    }
                                }

                                h1 { class: "text-2xl sm:text-[30px] font-bold tracking-[-0.025em] leading-[1.15] text-beet-text-primary break-words",
                                    "{pl.name}"
                                }

                                if !pl.comment.trim().is_empty() {
                                    p { class: "text-[13px] text-beet-text-secondary max-w-2xl",
                                        "{pl.comment}"
                                    }
                                }

                                p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                    "{display_tracks.len()} TRACKS"
                                    if total_secs > 0 {
                                        " · {format_duration(total_secs)}"
                                    }
                                }

                                // Action Row: Play, Shuffle, `...` menu
                                div { class: "flex flex-wrap items-center gap-2.5 pt-1",
                                    button {
                                        r#type: "button",
                                        disabled: display_tracks.is_empty(),
                                        class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-[color,background-color,border-color,filter] cursor-pointer",
                                        onclick: move |_| {
                                            if !tracks_for_play.is_empty() {
                                                player.play_queue(tracks_for_play.clone(), 0);
                                            }
                                        },
                                        IconPlay { size: 15, filled: true, class: "ml-0.5".to_string() }
                                        span { "Play" }
                                    }

                                    button {
                                        r#type: "button",
                                        disabled: display_tracks.is_empty(),
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            if !tracks_for_shuffle.is_empty() {
                                                if !player.snapshot().shuffle {
                                                    player.toggle_shuffle();
                                                }
                                                player.play_queue(tracks_for_shuffle.clone(), 0);
                                            }
                                        },
                                        IconShuffle { size: 14 }
                                        span { "Shuffle" }
                                    }

                                    // Overflow Menu (`Rename / Edit`, `Duplicate`, `Delete Playlist`)
                                    div { class: "relative",
                                        button {
                                            r#type: "button",
                                            aria_haspopup: "menu",
                                            aria_expanded: if menu_open() { "true" } else { "false" },
                                            aria_label: "Playlist actions",
                                            title: "Playlist actions",
                                            class: "w-10 h-10 rounded-full inline-flex items-center justify-center bg-beet-surface-high border border-beet-border-default text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-highest transition-colors cursor-pointer",
                                            onclick: move |_| menu_open.toggle(),
                                            IconMoreHorizontal { size: 18 }
                                        }

                                        if menu_open() {
                                            div {
                                                class: "fixed inset-0 z-40",
                                                onclick: move |_| menu_open.set(false),
                                            }
                                            div {
                                                role: "menu",
                                                aria_label: "Playlist options",
                                                class: "absolute left-0 sm:left-auto sm:right-0 top-12 z-50 w-52 rounded-[11px] bg-beet-surface-high border border-beet-border-default shadow-2xl p-1.5 space-y-0.5",
                                                if is_static_editable {
                                                    button {
                                                        r#type: "button",
                                                        role: "menuitem",
                                                        class: "w-full flex items-center gap-2.5 px-2.5 py-2 rounded-[7px] text-[13px] text-beet-text-primary hover:bg-beet-surface-highest text-left cursor-pointer transition-colors",
                                                        onclick: move |_| {
                                                            menu_open.set(false);
                                                            studio.open_edit_playlist(
                                                                pl_for_edit.id.clone(),
                                                                pl_for_edit.name.clone(),
                                                                pl_for_edit.comment.clone(),
                                                                pl_for_edit.public,
                                                            );
                                                        },
                                                        IconPencil { size: 14, class: "text-beet-text-secondary".to_string() }
                                                        span { "Rename / Edit" }
                                                    }
                                                }
                                                button {
                                                    r#type: "button",
                                                    role: "menuitem",
                                                    disabled: busy_mutation(),
                                                    class: "w-full flex items-center gap-2.5 px-2.5 py-2 rounded-[7px] text-[13px] text-beet-text-primary hover:bg-beet-surface-highest text-left cursor-pointer transition-colors disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none",
                                                    onclick: move |_| {
                                                        menu_open.set(false);
                                                        if busy_mutation() {
                                                            return;
                                                        }
                                                        busy_mutation.set(true);
                                                        let copy_name = format!("{} (Copy)", pl_for_dup.name);
                                                        let copy_comment = (!pl_for_dup.comment.is_empty()).then(|| pl_for_dup.comment.clone());
                                                        let ids = dup_song_ids.clone();
                                                        spawn(async move {
                                                            match auth
                                                                .call(api::create_playlist(
                                                                    copy_name,
                                                                    copy_comment,
                                                                    Some(false),
                                                                    Some(ids),
                                                                ))
                                                                .await
                                                            {
                                                                Ok(created) => {
                                                                    studio.refresh_playlists();
                                                                    studio.show_toast(format!("Duplicated as \"{}\"", created.name));
                                                                    nav.push(Route::PlaylistDetailPage { id: created.id });
                                                                }
                                                                Err(err) => {
                                                                    studio.show_toast(format!("Duplicate failed: {}", friendly_error(&err)));
                                                                }
                                                            }
                                                            busy_mutation.set(false);
                                                        });
                                                    },
                                                    IconCopy { size: 14, class: "text-beet-text-secondary".to_string() }
                                                    span { "Duplicate" }
                                                }
                                                if is_static_editable {
                                                    div { class: "my-1 border-t border-beet-border-subtle" }
                                                    button {
                                                        r#type: "button",
                                                        role: "menuitem",
                                                        class: "w-full flex items-center gap-2.5 px-2.5 py-2 rounded-[7px] text-[13px] text-beet-crimson-text hover:bg-beet-crimson-surface text-left cursor-pointer transition-colors",
                                                        onclick: move |_| {
                                                            menu_open.set(false);
                                                            confirm_delete_open.set(true);
                                                        },
                                                        IconTrash2 { size: 14 }
                                                        span { "Delete Playlist" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Tracklist
                        if display_tracks.is_empty() {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "This playlist has no tracks yet. Use the ... menu on any track to add songs here."
                            }
                        } else {
                            div { class: "space-y-1", role: "list", aria_label: "Playlist tracks",
                                for (idx, track) in display_tracks.iter().enumerate() {
                                    {
                                        let all = display_tracks.clone();
                                        let total_len = display_tracks.len();
                                        let t_next = track.clone();
                                        let t_queue = track.clone();
                                        let t_pl = track.clone();
                                        let t_star = track.clone();
                                        let t_rate = track.clone();
                                        let t_inspect = track.clone();
                                        let pid_up = pl.id.clone();
                                        let pid_down = pl.id.clone();
                                        let pid_rm = pl.id.clone();
                                        let rm_song_id = track.id.clone();
                                        let rm_title = track.title.clone();

                                        let row_state = if offline_only && !track.is_offline_cached {
                                            TrackRowState::OfflineUnavailable
                                        } else {
                                            TrackRowState::Owned {
                                                is_offline_cached: track.is_offline_cached,
                                                starred: track.starred,
                                                rating: track.rating,
                                            }
                                        };

                                        let on_move_up = (is_static_editable && idx > 0).then(|| {
                                            EventHandler::new(move |()| {
                                                let mut next_list = tracks.peek().clone();
                                                if idx > 0 && idx < next_list.len() {
                                                    next_list.swap(idx, idx - 1);
                                                    let ids: Vec<String> = next_list.iter().map(|t| t.id.clone()).collect();
                                                    tracks.set(next_list);
                                                    let pid = pid_up.clone();
                                                    spawn(async move {
                                                        if let Err(err) = auth.call(api::set_playlist_tracks(pid, ids)).await {
                                                            studio.show_toast(format!("Reorder failed: {}", friendly_error(&err)));
                                                            reload_tick += 1;
                                                        }
                                                    });
                                                }
                                            })
                                        });

                                        let on_move_down = (is_static_editable && idx + 1 < total_len).then(|| {
                                            EventHandler::new(move |()| {
                                                let mut next_list = tracks.peek().clone();
                                                if idx + 1 < next_list.len() {
                                                    next_list.swap(idx, idx + 1);
                                                    let ids: Vec<String> = next_list.iter().map(|t| t.id.clone()).collect();
                                                    tracks.set(next_list);
                                                    let pid = pid_down.clone();
                                                    spawn(async move {
                                                        if let Err(err) = auth.call(api::set_playlist_tracks(pid, ids)).await {
                                                            studio.show_toast(format!("Reorder failed: {}", friendly_error(&err)));
                                                            reload_tick += 1;
                                                        }
                                                    });
                                                }
                                            })
                                        });

                                        let on_remove_from_playlist = is_static_editable.then(|| {
                                            EventHandler::new(move |()| {
                                                let sid = rm_song_id.clone();
                                                let title = rm_title.clone();
                                                let pid = pid_rm.clone();
                                                let mut next_list = tracks.peek().clone();
                                                if idx < next_list.len() && next_list[idx].id == sid {
                                                    next_list.remove(idx);
                                                    tracks.set(next_list.clone());
                                                    studio.sync_playlist_count(&pid, next_list.len() as u32);
                                                }
                                                spawn(async move {
                                                    match auth.call(api::remove_playlist_tracks(pid, vec![sid])).await {
                                                        Ok(_) => {
                                                            studio.refresh_playlists();
                                                            studio.show_toast(format!("Removed \"{title}\" from playlist"));
                                                        }
                                                        Err(err) => {
                                                            studio.show_toast(format!("Remove failed: {}", friendly_error(&err)));
                                                            reload_tick += 1;
                                                        }
                                                    }
                                                });
                                            })
                                        });

                                        rsx! {
                                            TrackRow {
                                                key: "pl-det-{idx}-{track.id}",
                                                track_number: Some((idx + 1) as u32),
                                                title: track.title.clone(),
                                                artist: track.artist.clone(),
                                                album: track.album.clone(),
                                                cover_art_url: track.cover_art_url.clone(),
                                                duration_secs: track.duration_secs,
                                                codec_badge: track.codec_badge.clone(),
                                                is_playing: player.is_track_playing(&track.id),
                                                state: row_state,
                                                on_play: move |_| player.play_queue(all.clone(), idx),
                                                on_play_next: move |_| {
                                                    player.enqueue_next(t_next.clone());
                                                    studio.show_toast(format!("Playing \"{}\" next", t_next.title));
                                                },
                                                on_add_to_queue: move |_| {
                                                    player.enqueue_last(t_queue.clone());
                                                    studio.show_toast(format!("Added \"{}\" to queue", t_queue.title));
                                                },
                                                on_add_to_playlist: move |_| {
                                                    studio.open_add_to_playlist(
                                                        t_pl.title.clone(),
                                                        vec![t_pl.id.clone()],
                                                    );
                                                },
                                                on_artist_click: move |artist| studio.navigate_to_artist(nav, artist),
                                                on_album_click: move |album| studio.navigate_to_album(nav, album),
                                                on_star: move |next| studio.star_track(auth, player, t_star.clone(), next),
                                                on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                                on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
                                                on_move_up,
                                                on_move_down,
                                                on_remove_from_playlist,
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if confirm_delete_open() {
                            ConfirmModal {
                                message: format!("Delete playlist \"{}\"? Your library files will not be deleted.", pl.name),
                                confirm_label: "Delete Playlist".to_string(),
                                danger: true,
                                on_confirm: handle_delete,
                                on_cancel: move |_| confirm_delete_open.set(false),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Dedicated Album Detail route (`/album/:id`).
#[component]
pub fn AlbumDetailPage(id: String) -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let nav = use_navigator();

    let mut album = use_signal(|| None::<SubsonicAlbumSummary>);
    let mut tracks = use_signal(Vec::<PlayerTrack>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut art_failed = use_signal(|| false);
    let mut req_id = use_signal(|| 0u64);

    let id_for_load = id.clone();
    use_effect(move || {
        let aid = id_for_load.clone();
        let nav_info = studio
            .bootstrap
            .read()
            .as_ref()
            .and_then(|b| b.navidrome.clone());
        let Some(info) = nav_info else {
            return;
        };
        let next_req = req_id.peek().wrapping_add(1);
        req_id.set(next_req);
        loading.set(true);
        error.set(None);
        art_failed.set(false);
        spawn(async move {
            match fetch_album_detail(&info, &aid).await {
                Ok(Some((alb, list))) => {
                    if *req_id.peek() == next_req {
                        album.set(Some(alb));
                        tracks.set(list);
                        loading.set(false);
                    }
                }
                Ok(None) => {
                    if *req_id.peek() == next_req {
                        error.set(Some("Album not found.".to_string()));
                        loading.set(false);
                    }
                }
                Err(e) => {
                    if *req_id.peek() == next_req {
                        error.set(Some(e));
                        loading.set(false);
                    }
                }
            }
        });
    });

    let offline_only = player.offline_only();
    let display_tracks: Vec<PlayerTrack> = tracks
        .read()
        .iter()
        .cloned()
        .map(|mut t| {
            studio.apply_curation_overrides(&mut t);
            t
        })
        .collect();

    rsx! {
        div { class: "space-y-6",
            button {
                r#type: "button",
                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                onclick: move |_| {
                    nav.push(Route::LibraryPage {});
                },
                IconChevronLeft { size: 14 }
                span { "Library" }
            }

            if loading() && album.read().is_none() {
                div { class: "py-16 flex items-center justify-center gap-2.5 text-beet-text-tertiary text-[13px]",
                    IconLoader { size: 16, class: "animate-spin text-beet-leaf-text".to_string() }
                    span { "Loading album..." }
                }
            } else if let Some(err) = error() {
                div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center space-y-2",
                    p { class: "text-[14px] font-semibold text-beet-text-primary",
                        "Could not load album"
                    }
                    p { class: "text-[13px] text-beet-text-secondary", "{err}" }
                }
            } else if let Some(alb) = album.read().clone() {
                {
                    let total_secs: u32 = display_tracks.iter().filter_map(|t| t.duration_secs).sum();
                    let tracks_for_play = display_tracks.clone();
                    let tracks_for_shuffle = display_tracks.clone();
                    let tracks_for_queue = display_tracks.clone();
                    let alb_name = alb.name.clone();
                    let alb_artist = alb.artist.clone();
                    let alb_ids: Vec<String> = display_tracks
                        .iter()
                        .map(|t| t.id.clone())
                        .filter(|sid| !sid.is_empty())
                        .collect();

                    rsx! {
                        section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-5 sm:p-6 flex flex-col sm:flex-row items-start sm:items-end gap-5",
                            div { class: "w-32 h-32 sm:w-40 sm:h-40 rounded-[12px] overflow-hidden bg-beet-surface border border-beet-border-subtle flex items-center justify-center shrink-0 shadow-xl",
                                if let Some(url) = alb.cover_art_url.as_deref().filter(|_| !art_failed()) {
                                    img {
                                        src: "{url}",
                                        alt: "Cover for {alb.name}",
                                        class: "w-full h-full object-cover",
                                        onerror: move |_| art_failed.set(true),
                                    }
                                } else {
                                    IconDisc3 { size: 42, class: "text-beet-text-disabled".to_string() }
                                }
                            }

                            div { class: "flex-1 min-w-0 space-y-2.5",
                                span { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider px-2 py-0.5 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-tertiary",
                                    "ALBUM"
                                }
                                h1 { class: "text-2xl sm:text-[30px] font-bold tracking-[-0.025em] leading-[1.15] text-beet-text-primary break-words",
                                    "{alb.name}"
                                }
                                div { class: "flex flex-wrap items-center gap-2 text-[14px] text-beet-text-secondary",
                                    button {
                                        r#type: "button",
                                        class: "font-semibold text-beet-text-primary hover:text-beet-leaf-text hover:underline cursor-pointer",
                                        onclick: move |_| studio.navigate_to_artist(nav, alb_artist.clone()),
                                        "{alb.artist}"
                                    }
                                    if let Some(yr) = alb.year {
                                        span { class: "tabular-nums-mono text-[12px] text-beet-text-tertiary", "· {yr}" }
                                    }
                                    span { class: "tabular-nums-mono text-[12px] text-beet-text-tertiary",
                                        "· {display_tracks.len()} TRACKS"
                                    }
                                    if total_secs > 0 {
                                        span { class: "tabular-nums-mono text-[12px] text-beet-text-tertiary",
                                            "· {format_duration(total_secs)}"
                                        }
                                    }
                                }

                                div { class: "flex flex-wrap items-center gap-2.5 pt-1",
                                    button {
                                        r#type: "button",
                                        disabled: display_tracks.is_empty(),
                                        class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-[color,background-color,border-color,filter] cursor-pointer",
                                        onclick: move |_| {
                                            if !tracks_for_play.is_empty() {
                                                player.play_queue(tracks_for_play.clone(), 0);
                                            }
                                        },
                                        IconPlay { size: 15, filled: true, class: "ml-0.5".to_string() }
                                        span { "Play" }
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: display_tracks.is_empty(),
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            if !tracks_for_shuffle.is_empty() {
                                                if !player.snapshot().shuffle {
                                                    player.toggle_shuffle();
                                                }
                                                player.play_queue(tracks_for_shuffle.clone(), 0);
                                            }
                                        },
                                        IconShuffle { size: 14 }
                                        span { "Shuffle" }
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: display_tracks.is_empty(),
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            for t in tracks_for_queue.iter() {
                                                player.enqueue_last(t.clone());
                                            }
                                            studio.show_toast(format!("Added {} tracks to queue", tracks_for_queue.len()));
                                        },
                                        IconListMusic { size: 14 }
                                        span { "Add to Queue" }
                                    }
                                    button {
                                        r#type: "button",
                                        disabled: alb_ids.is_empty(),
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            studio.open_add_to_playlist(alb_name.clone(), alb_ids.clone());
                                        },
                                        IconListPlus { size: 14 }
                                        span { "Add to Playlist" }
                                    }
                                }
                            }
                        }

                        div { class: "space-y-1", role: "list", aria_label: "Album tracks",
                            for (idx, track) in display_tracks.iter().enumerate() {
                                {
                                    let all = display_tracks.clone();
                                    let t_next = track.clone();
                                    let t_queue = track.clone();
                                    let t_pl = track.clone();
                                    let t_star = track.clone();
                                    let t_rate = track.clone();
                                    let t_inspect = track.clone();
                                    let row_state = if offline_only && !track.is_offline_cached {
                                        TrackRowState::OfflineUnavailable
                                    } else {
                                        TrackRowState::Owned {
                                            is_offline_cached: track.is_offline_cached,
                                            starred: track.starred,
                                            rating: track.rating,
                                        }
                                    };
                                    rsx! {
                                        TrackRow {
                                            key: "alb-det-{idx}-{track.id}",
                                            track_number: Some((idx + 1) as u32),
                                            title: track.title.clone(),
                                            artist: track.artist.clone(),
                                            album: track.album.clone(),
                                            cover_art_url: track.cover_art_url.clone(),
                                            duration_secs: track.duration_secs,
                                            codec_badge: track.codec_badge.clone(),
                                            is_playing: player.is_track_playing(&track.id),
                                            state: row_state,
                                            on_play: move |_| player.play_queue(all.clone(), idx),
                                            on_play_next: move |_| {
                                                player.enqueue_next(t_next.clone());
                                                studio.show_toast(format!("Playing \"{}\" next", t_next.title));
                                            },
                                            on_add_to_queue: move |_| {
                                                player.enqueue_last(t_queue.clone());
                                                studio.show_toast(format!("Added \"{}\" to queue", t_queue.title));
                                            },
                                            on_add_to_playlist: move |_| {
                                                studio.open_add_to_playlist(t_pl.title.clone(), vec![t_pl.id.clone()]);
                                            },
                                            on_artist_click: move |artist| studio.navigate_to_artist(nav, artist),
                                            on_star: move |next| studio.star_track(auth, player, t_star.clone(), next),
                                            on_rate: move |r| studio.rate_track(auth, player, t_rate.clone(), r),
                                            on_inspect: move |_| studio.inspect_track(auth, t_inspect.clone()),
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

/// Dedicated Artist Detail route (`/artist/:id`).
#[component]
pub fn ArtistDetailPage(id: String) -> Element {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();
    let nav = use_navigator();

    let mut artist = use_signal(|| None::<SubsonicArtistSummary>);
    let mut albums = use_signal(Vec::<SubsonicAlbumSummary>::new);
    let mut loading = use_signal(|| true);
    let mut radio_busy = use_signal(|| false);
    let mut art_failed = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut req_id = use_signal(|| 0u64);

    let id_for_load = id.clone();
    use_effect(move || {
        let artist_id = id_for_load.clone();
        let nav_info = studio
            .bootstrap
            .read()
            .as_ref()
            .and_then(|b| b.navidrome.clone());
        let Some(info) = nav_info else {
            return;
        };
        let next_req = req_id.peek().wrapping_add(1);
        req_id.set(next_req);
        loading.set(true);
        error.set(None);
        art_failed.set(false);
        spawn(async move {
            match fetch_artist_detail(&info, &artist_id).await {
                Ok(Some((art, list))) => {
                    if *req_id.peek() == next_req {
                        artist.set(Some(art));
                        albums.set(list);
                        loading.set(false);
                    }
                }
                Ok(None) => {
                    if *req_id.peek() == next_req {
                        error.set(Some("Artist not found.".to_string()));
                        loading.set(false);
                    }
                }
                Err(e) => {
                    if *req_id.peek() == next_req {
                        error.set(Some(e));
                        loading.set(false);
                    }
                }
            }
        });
    });

    let play_album = use_callback(move |(album_id, album_name): (String, String)| {
        let Some(info) = studio.navidrome_info() else {
            return;
        };
        spawn(async move {
            match fetch_album_tracks(&info, &album_id).await {
                Ok(mut list) if !list.is_empty() => {
                    for t in &mut list {
                        studio.apply_curation_overrides(t);
                    }
                    player.play_queue(list, 0);
                }
                _ => {
                    studio.show_toast(format!("No playable tracks found in {album_name}"));
                }
            }
        });
    });

    rsx! {
        div { class: "space-y-6",
            button {
                r#type: "button",
                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                onclick: move |_| {
                    nav.push(Route::LibraryPage {});
                },
                IconChevronLeft { size: 14 }
                span { "Library" }
            }

            if loading() && artist.read().is_none() {
                div { class: "py-16 flex items-center justify-center gap-2.5 text-beet-text-tertiary text-[13px]",
                    IconLoader { size: 16, class: "animate-spin text-beet-leaf-text".to_string() }
                    span { "Loading artist..." }
                }
            } else if let Some(err) = error() {
                div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center space-y-2",
                    p { class: "text-[14px] font-semibold text-beet-text-primary",
                        "Could not load artist"
                    }
                    p { class: "text-[13px] text-beet-text-secondary", "{err}" }
                }
            } else if let Some(art) = artist.read().clone() {
                {
                    let art_name_radio = art.name.clone();
                    let art_name_search = art.name.clone();
                    let alb_list = albums.read().clone();

                    rsx! {
                        section { class: "rounded-[16px] bg-beet-surface-low border border-beet-border-subtle p-5 sm:p-6 flex flex-col sm:flex-row items-start sm:items-center gap-5",
                            div { class: "w-28 h-28 sm:w-32 sm:h-32 rounded-full overflow-hidden bg-beet-surface border border-beet-border-subtle flex items-center justify-center shrink-0 shadow-xl",
                                if let Some(url) = art.cover_art_url.as_deref().filter(|_| !art_failed()) {
                                    img {
                                        src: "{url}",
                                        alt: "{art.name}",
                                        class: "w-full h-full object-cover",
                                        onerror: move |_| art_failed.set(true),
                                    }
                                } else {
                                    IconUser { size: 38, class: "text-beet-text-disabled".to_string() }
                                }
                            }

                            div { class: "flex-1 min-w-0 space-y-2.5",
                                span { class: "tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider px-2 py-0.5 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-tertiary",
                                    "ARTIST"
                                }
                                h1 { class: "text-2xl sm:text-[32px] font-bold tracking-[-0.025em] leading-[1.12] text-beet-text-primary break-words",
                                    "{art.name}"
                                }
                                p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary",
                                    if alb_list.len() == 1 {
                                        "1 ALBUM IN LIBRARY"
                                    } else {
                                        "{alb_list.len()} ALBUMS IN LIBRARY"
                                    }
                                }

                                div { class: "flex flex-wrap items-center gap-2.5 pt-1",
                                    button {
                                        r#type: "button",
                                        disabled: radio_busy(),
                                        class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 disabled:opacity-45 disabled:cursor-not-allowed disabled:pointer-events-none transition-[color,background-color,border-color,filter] cursor-pointer",
                                        onclick: move |_| {
                                            if radio_busy() {
                                                return;
                                            }
                                            radio_busy.set(true);
                                            let artist_name = art_name_radio.clone();
                                            spawn(async move {
                                                let req = SeedRadioRequest {
                                                    seed_song_id: None,
                                                    seed_artist: Some(artist_name.clone()),
                                                    seed_track: None,
                                                    seed_artist_mbid: None,
                                                    limit: Some(25),
                                                };
                                                match auth.call(api::start_seed_radio(req)).await {
                                                    Ok(res) => {
                                                        let nav_info = studio.navidrome_info();
                                                        let queue: Vec<PlayerTrack> = res
                                                            .owned_tracks
                                                            .into_iter()
                                                            .map(|t| {
                                                                let cover = nav_info
                                                                    .as_ref()
                                                                    .map(|i| subsonic_cover_art_url(i, &t.song_id, 256));
                                                                let mut pt = PlayerTrack {
                                                                    id: t.song_id.clone(),
                                                                    title: t.title,
                                                                    artist: t.artist,
                                                                    album: t.album,
                                                                    cover_art_url: cover,
                                                                    cover_art_id: Some(t.song_id),
                                                                    duration_secs: t.duration,
                                                                    starred: t.starred,
                                                                    rating: t.rating.unwrap_or(0).min(5),
                                                                    ..Default::default()
                                                                };
                                                                studio.apply_curation_overrides(&mut pt);
                                                                pt
                                                            })
                                                            .collect();
                                                        if queue.is_empty() {
                                                            studio.show_toast(format!("No tracks matched {artist_name} radio."));
                                                        } else {
                                                            player.play_queue(queue, 0);
                                                            studio.show_toast(format!("Playing {artist_name} radio"));
                                                        }
                                                    }
                                                    Err(err) => {
                                                        studio.show_toast(format!("Radio failed: {}", friendly_error(&err)));
                                                    }
                                                }
                                                radio_busy.set(false);
                                            });
                                        },
                                        if radio_busy() {
                                            IconLoader { size: 15, class: "animate-spin".to_string() }
                                        } else {
                                            IconRadio { size: 15 }
                                        }
                                        span { "Start Artist Radio" }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "inline-flex items-center gap-1.5 h-10 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[13px] font-medium transition-colors cursor-pointer",
                                        onclick: move |_| {
                                            studio.search_query_override.set(Some(art_name_search.clone()));
                                            nav.push(Route::SearchPage {});
                                        },
                                        span { "Search Catalog" }
                                    }
                                }
                            }
                        }

                        if alb_list.is_empty() {
                            div { class: "rounded-[14px] bg-beet-surface border border-beet-border-subtle p-8 text-center text-[13px] text-beet-text-secondary",
                                "No albums found in your library for this artist."
                            }
                        } else {
                            section { class: "space-y-3",
                                h2 { class: "text-[17px] font-semibold text-beet-text-primary",
                                    "Albums"
                                }
                                div { class: "grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-4",
                                    for alb in alb_list {
                                        {
                                            let aid_open = alb.id.clone();
                                            let aid_play = alb.id.clone();
                                            let aname_play = alb.name.clone();
                                            rsx! {
                                                div {
                                                    key: "art-alb-{alb.id}",
                                                    role: "button",
                                                    tabindex: "0",
                                                    class: "group rounded-[12px] bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default p-3 text-left transition-colors cursor-pointer space-y-2.5",
                                                    onclick: move |_| {
                                                        nav.push(Route::AlbumDetailPage { id: aid_open.clone() });
                                                    },
                                                    div { class: "aspect-square rounded-[8px] overflow-hidden bg-beet-surface-low flex items-center justify-center relative",
                                                        if let Some(url) = alb.cover_art_url.as_deref() {
                                                            img { src: "{url}", alt: "", class: "w-full h-full object-cover", loading: "lazy" }
                                                        } else {
                                                            IconDisc3 { size: 28, class: "text-beet-text-disabled".to_string() }
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            aria_label: format!("Play {}", alb.name),
                                                            title: format!("Play {}", alb.name),
                                                            class: "absolute bottom-2 right-2 w-9 h-9 rounded-full bg-beet-leaf-solid text-beet-leaf-ink flex items-center justify-center opacity-0 group-hover:opacity-100 focus-visible:opacity-100 hover:scale-105 transition-[opacity,transform] shadow-lg cursor-pointer",
                                                            onclick: move |evt: MouseEvent| {
                                                                evt.stop_propagation();
                                                                play_album.call((aid_play.clone(), aname_play.clone()));
                                                            },
                                                            IconPlay { size: 15, filled: true, class: "ml-0.5".to_string() }
                                                        }
                                                    }
                                                    div {
                                                        p { class: "text-[14px] font-semibold text-beet-text-primary truncate", "{alb.name}" }
                                                        p { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary truncate",
                                                            if let Some(yr) = alb.year {
                                                                "{yr} · {alb.song_count} tracks"
                                                            } else {
                                                                "{alb.song_count} tracks"
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
        }
    }
}
