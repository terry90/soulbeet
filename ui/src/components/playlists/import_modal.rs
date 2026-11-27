use std::collections::HashSet;

use api::models::folder::Folder;
use dioxus::prelude::*;
use shared::playlist_import::{
    CommitPlaylistImportRequest, CommitPlaylistImportResponse, CommitPlaylistImportTrack,
    ImportMatchStatus, PlaylistImportPreview, PlaylistImportTarget,
};

use crate::settings_context::use_settings;
use crate::{
    friendly_error, use_auth, IconActivity, IconCheck, IconInfo, IconListPlus, IconLoader, Modal,
    TextInput,
};

#[derive(Debug, Clone, PartialEq)]
enum ImportStep {
    UrlInput,
    Preview(PlaylistImportPreview),
    Done(CommitPlaylistImportResponse),
}

fn format_duration(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[derive(Props, Clone, PartialEq)]
pub struct PlaylistImportModalProps {
    pub on_close: EventHandler,
    #[props(default)]
    pub on_imported: EventHandler<CommitPlaylistImportResponse>,
    #[props(default)]
    pub on_view_activity: Option<EventHandler>,
}

/// Two-step modal for importing a public playlist URL from Spotify, YouTube Music, Deezer, or
/// ListenBrainz into Navidrome and queuing missing tracks for download.
#[component]
pub fn PlaylistImportModal(props: PlaylistImportModalProps) -> Element {
    let auth = use_auth();
    let settings = use_settings();

    let mut step = use_signal(|| ImportStep::UrlInput);
    let mut url_input = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    // Step 2 state
    let mut playlist_name = use_signal(String::new);
    let mut selected_indices = use_signal(HashSet::<usize>::new);
    let mut folders = use_signal(Vec::<Folder>::new);
    let mut folders_loading = use_signal(|| true);
    let mut folders_error = use_signal(|| None::<String>);
    let mut selected_folder_id = use_signal(|| None::<String>);

    use_future(move || async move {
        match auth.call(api::get_user_folders()).await {
            Ok(list) => {
                if selected_folder_id.peek().is_none() {
                    let default_id = settings
                        .get()
                        .and_then(|s| s.default_download_folder_id)
                        .filter(|id| list.iter().any(|f| f.id == *id));
                    let chosen = default_id.or_else(|| list.first().map(|f| f.id.clone()));
                    selected_folder_id.set(chosen);
                }
                folders.set(list);
                folders_error.set(None);
            }
            Err(e) => {
                folders_error.set(Some(friendly_error(&e)));
            }
        }
        folders_loading.set(false);
    });

    let run_preview = use_callback(move |()| {
        let raw = url_input.read().trim().to_string();
        if raw.is_empty() || *busy.read() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn(async move {
            match auth.call(api::preview_playlist_import(raw)).await {
                Ok(preview) => {
                    playlist_name.set(preview.title.clone());
                    let all: HashSet<usize> = preview.tracks.iter().map(|t| t.index).collect();
                    selected_indices.set(all);
                    step.set(ImportStep::Preview(preview));
                }
                Err(e) => {
                    error.set(Some(friendly_error(&e)));
                }
            }
            busy.set(false);
        });
    });

    let on_imported = props.on_imported;
    let run_commit = use_callback(move |()| {
        if *busy.read() {
            return;
        }
        let ImportStep::Preview(preview) = step.read().clone() else {
            return;
        };
        let name = playlist_name.read().trim().to_string();
        if name.is_empty() {
            error.set(Some("Enter a name for the playlist.".into()));
            return;
        }
        let Some(folder_id) = selected_folder_id.read().clone() else {
            let msg = match folders_error.read().clone() {
                Some(err) => format!("Could not load library folders: {err}"),
                None => "Add a library folder in Settings before importing playlists.".into(),
            };
            error.set(Some(msg));
            return;
        };
        let chosen_set = selected_indices.read().clone();
        let tracks: Vec<CommitPlaylistImportTrack> = preview
            .tracks
            .iter()
            .filter(|t| chosen_set.contains(&t.index))
            .map(|t| CommitPlaylistImportTrack {
                artist: t.artist.clone(),
                title: t.title.clone(),
                album: t.album.clone(),
                duration_s: t.duration_s,
                recording_mbid: t.recording_mbid.clone(),
            })
            .collect();
        if tracks.is_empty() {
            error.set(Some("Select at least one track to import.".into()));
            return;
        }

        let comment = Some(format!(
            "Imported from {} ({})",
            preview.platform.display_name(),
            preview.source_url
        ));

        let req = CommitPlaylistImportRequest {
            target: PlaylistImportTarget::Create {
                name,
                comment,
                allow_duplicate_name: false,
            },
            folder_id,
            tracks,
        };

        busy.set(true);
        error.set(None);
        spawn(async move {
            match auth.call(api::commit_playlist_import(req)).await {
                Ok(resp) => {
                    on_imported.call(resp.clone());
                    step.set(ImportStep::Done(resp));
                }
                Err(e) => {
                    error.set(Some(friendly_error(&e)));
                }
            }
            busy.set(false);
        });
    });

    let current = step.read().clone();
    let header_subtitle = match &current {
        ImportStep::UrlInput => "Paste a link to bring a playlist into your library".to_string(),
        ImportStep::Preview(p) => format!(
            "{} · {} tracks found",
            p.platform.display_name(),
            p.tracks.len()
        ),
        ImportStep::Done(_) => "Playlist import started".to_string(),
    };

    let footer = match &current {
        ImportStep::UrlInput => {
            let can_preview = !url_input.read().trim().is_empty() && !busy();
            rsx! {
                div { class: "flex items-center justify-end gap-2",
                    button {
                        r#type: "button",
                        class: "h-9 px-3.5 rounded-[10px] bg-beet-surface border border-beet-border-subtle text-[13px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer",
                        onclick: move |_| props.on_close.call(()),
                        "Cancel"
                    }
                    button {
                        r#type: "button",
                        disabled: !can_preview,
                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-[10px] bg-beet-accent-solid text-white text-[13px] font-semibold transition-opacity cursor-pointer disabled:opacity-45 disabled:cursor-not-allowed",
                        onclick: move |_| run_preview.call(()),
                        if busy() {
                            IconLoader { size: 14 }
                            span { "Checking library..." }
                        } else {
                            span { "Preview" }
                        }
                    }
                }
            }
        }
        ImportStep::Preview(_) => {
            let selected_count = selected_indices.read().len();
            let can_import = selected_count > 0
                && !playlist_name.read().trim().is_empty()
                && selected_folder_id.read().is_some()
                && !busy();
            rsx! {
                div { class: "flex items-center justify-between gap-2",
                    button {
                        r#type: "button",
                        disabled: busy(),
                        class: "h-9 px-3.5 rounded-[10px] bg-beet-surface border border-beet-border-subtle text-[13px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer disabled:opacity-45",
                        onclick: move |_| {
                            error.set(None);
                            step.set(ImportStep::UrlInput);
                        },
                        "Back"
                    }
                    button {
                        r#type: "button",
                        disabled: !can_import,
                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold transition-opacity cursor-pointer disabled:opacity-45 disabled:cursor-not-allowed",
                        onclick: move |_| run_commit.call(()),
                        if busy() {
                            IconLoader { size: 14 }
                            span { "Importing..." }
                        } else {
                            IconListPlus { size: 15 }
                            span { "Import Playlist ({selected_count})" }
                        }
                    }
                }
            }
        }
        ImportStep::Done(_) => {
            let on_view = props.on_view_activity;
            rsx! {
                div { class: "flex items-center justify-end gap-2",
                    if let Some(on_view) = on_view {
                        button {
                            r#type: "button",
                            class: "inline-flex items-center gap-1.5 h-9 px-3.5 rounded-[10px] bg-beet-accent-surface border border-beet-accent/35 text-beet-accent-text text-[13px] font-medium cursor-pointer",
                            onclick: move |_| on_view.call(()),
                            IconActivity { size: 14 }
                            span { "View Activity" }
                        }
                    }
                    button {
                        r#type: "button",
                        class: "h-9 px-4 rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold cursor-pointer",
                        onclick: move |_| props.on_close.call(()),
                        "Done"
                    }
                }
            }
        }
    };

    rsx! {
        Modal {
            on_close: props.on_close,
            header: rsx! {
                div { class: "space-y-0.5",
                    h2 { class: "text-[16px] font-semibold text-beet-text-primary",
                        "Import Playlist"
                    }
                    p { class: "text-[12px] text-beet-text-tertiary", "{header_subtitle}" }
                }
            },
            footer: Some(footer),

            div { class: "space-y-4",
                if let Some(err) = error() {
                    div { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-3.5 py-2.5 text-[12px] text-beet-crimson-text break-words",
                        "{err}"
                    }
                }

                match current {
                    ImportStep::UrlInput => rsx! {
                        div { class: "space-y-3",
                            label { class: "block text-[12px] font-medium text-beet-text-secondary",
                                "Playlist URL"
                            }
                            TextInput {
                                class: "text-[13px] rounded-[10px] text-beet-text-primary placeholder:text-beet-text-disabled",
                                placeholder: "Paste a Spotify, YouTube Music, Deezer, or ListenBrainz playlist link",
                                aria_label: "Playlist URL",
                                value: url_input(),
                                oninput: move |val: String| {
                                    url_input.set(val);
                                    if error.peek().is_some() {
                                        error.set(None);
                                    }
                                },
                                on_enter: move |_| run_preview.call(()),
                            }
                            div { class: "flex flex-wrap items-center gap-1.5 pt-1",
                                for badge in ["Spotify", "YouTube Music", "Deezer", "ListenBrainz"] {
                                    span {
                                        key: "{badge}",
                                        class: "px-2 py-0.5 rounded-full bg-beet-surface border border-beet-border-subtle text-[11px] text-beet-text-tertiary",
                                        "{badge}"
                                    }
                                }
                            }
                            p { class: "text-[12px] text-beet-text-tertiary leading-relaxed",
                                "Public playlists only. Tracks already in your library are added right away and missing tracks are queued automatically."
                            }
                        }
                    },
                    ImportStep::Preview(preview) => {
                        let chosen = selected_indices.read();
                        let in_lib_count = preview
                            .tracks
                            .iter()
                            .filter(|t| chosen.contains(&t.index) && t.match_status == ImportMatchStatus::InLibrary)
                            .count();
                        let fuzzy_count = preview
                            .tracks
                            .iter()
                            .filter(|t| chosen.contains(&t.index) && t.match_status == ImportMatchStatus::Fuzzy)
                            .count();
                        let to_download_count = preview
                            .tracks
                            .iter()
                            .filter(|t| chosen.contains(&t.index) && t.match_status != ImportMatchStatus::InLibrary)
                            .count();
                        let total_tracks = preview.tracks.len();
                        let all_selected = chosen.len() == total_tracks && total_tracks > 0;
                        let all_indices: Vec<usize> = preview.tracks.iter().map(|t| t.index).collect();
                        let folder_list = folders.read().clone();
                        let current_folder = selected_folder_id().unwrap_or_default();

                        rsx! {
                            div { class: "space-y-4",
                                // Playlist name + target folder
                                div { class: "grid grid-cols-1 sm:grid-cols-2 gap-3",
                                    div { class: "space-y-1",
                                        label { class: "block text-[11px] font-medium uppercase tracking-wider text-beet-text-tertiary",
                                            "Playlist Name"
                                        }
                                        TextInput {
                                            class: "text-[13px] rounded-[8px] text-beet-text-primary",
                                            placeholder: "My Playlist",
                                            aria_label: "Playlist Name",
                                            value: playlist_name(),
                                            oninput: move |v: String| playlist_name.set(v),
                                        }
                                    }
                                    div { class: "space-y-1",
                                        label { class: "block text-[11px] font-medium uppercase tracking-wider text-beet-text-tertiary",
                                            "Target Folder"
                                        }
                                        if let Some(err) = folders_error() {
                                            p { class: "text-[12px] text-beet-crimson-text py-2 break-words",
                                                "Could not load folders: {err}"
                                            }
                                        } else if folders_loading() {
                                            p { class: "text-[12px] text-beet-text-tertiary py-2",
                                                "Loading folders..."
                                            }
                                        } else if folder_list.is_empty() {
                                            p { class: "text-[12px] text-beet-crimson-text py-2",
                                                "No folders configured"
                                            }
                                        } else {
                                            select {
                                                class: "w-full h-[38px] px-3 rounded-[8px] bg-beet-surface border border-beet-border-default text-[13px] text-beet-text-primary outline-none focus:border-beet-accent",
                                                aria_label: "Target Folder",
                                                value: "{current_folder}",
                                                onchange: move |evt| {
                                                    let val = evt.value();
                                                    selected_folder_id.set((!val.is_empty()).then_some(val));
                                                },
                                                for f in folder_list.iter() {
                                                    option { key: "{f.id}", value: "{f.id}", "{f.name}" }
                                                }
                                            }
                                        }
                                    }
                                }

                                // Summary pills
                                div { class: "flex flex-wrap items-center gap-2",
                                    span { class: "inline-flex items-center gap-1 px-2.5 py-1 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text text-[11px] font-medium tabular-nums-mono",
                                        "{in_lib_count} already in library"
                                    }
                                    span { class: "inline-flex items-center gap-1 px-2.5 py-1 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text text-[11px] font-medium tabular-nums-mono",
                                        "{to_download_count} to download"
                                    }
                                    if fuzzy_count > 0 {
                                        span { class: "inline-flex items-center gap-1 px-2.5 py-1 rounded-full bg-beet-gold/15 border border-beet-gold/30 text-beet-gold text-[11px] font-medium tabular-nums-mono",
                                            "{fuzzy_count} fuzzy match"
                                        }
                                    }
                                }

                                // Optional truncation info banner
                                if let Some(note) = preview.truncated_note.as_deref() {
                                    div { class: "flex items-start gap-2 rounded-[10px] bg-beet-surface border border-beet-border-subtle px-3 py-2 text-[12px] text-beet-text-secondary",
                                        IconInfo { size: 15, class: "text-beet-accent-text shrink-0 mt-0.5".to_string() }
                                        span { "{note}" }
                                    }
                                }

                                // Selection controls
                                div { class: "flex items-center justify-between text-[12px] text-beet-text-tertiary border-b border-beet-border-subtle pb-2",
                                    span { class: "tabular-nums-mono",
                                        "{chosen.len()} of {total_tracks} selected"
                                    }
                                    button {
                                        r#type: "button",
                                        class: "text-beet-accent-text hover:underline cursor-pointer",
                                        onclick: move |_| {
                                            if all_selected {
                                                selected_indices.write().clear();
                                            } else {
                                                selected_indices.set(all_indices.iter().copied().collect());
                                            }
                                        },
                                        if all_selected { "Deselect all" } else { "Select all" }
                                    }
                                }

                                // Scrollable track checklist
                                div { class: "max-h-72 overflow-y-auto studio-scrollbar space-y-1.5 pr-1",
                                    for track in preview.tracks.iter() {
                                        {
                                            let idx = track.index;
                                            let checked = chosen.contains(&idx);
                                            let (pill_cls, pill_text) = match track.match_status {
                                                ImportMatchStatus::InLibrary => (
                                                    "bg-beet-leaf-surface border-beet-leaf-solid/30 text-beet-leaf-text",
                                                    "In Library",
                                                ),
                                                ImportMatchStatus::Fuzzy => (
                                                    "bg-beet-gold/15 border-beet-gold/30 text-beet-gold",
                                                    "Fuzzy",
                                                ),
                                                ImportMatchStatus::Missing => (
                                                    "bg-beet-accent-surface border-beet-accent/30 text-beet-accent-text",
                                                    "Download",
                                                ),
                                            };
                                            rsx! {
                                                label {
                                                    key: "imp-track-{idx}",
                                                    class: "flex items-center gap-2.5 p-2 rounded-[8px] bg-beet-surface/70 hover:bg-beet-surface border border-beet-border-subtle/70 cursor-pointer",
                                                    input {
                                                        r#type: "checkbox",
                                                        class: "rounded accent-beet-accent shrink-0 cursor-pointer",
                                                        checked,
                                                        onchange: move |_| {
                                                            let mut set = selected_indices.write();
                                                            if set.contains(&idx) {
                                                                set.remove(&idx);
                                                            } else {
                                                                set.insert(idx);
                                                            }
                                                        },
                                                    }
                                                    div { class: "min-w-0 flex-1",
                                                        p { class: "text-[13px] font-medium text-beet-text-primary truncate",
                                                            "{track.title}"
                                                        }
                                                        p { class: "text-[11px] text-beet-text-secondary truncate",
                                                            "{track.artist}"
                                                            if let Some(secs) = track.duration_s {
                                                                span { class: "tabular-nums-mono text-beet-text-tertiary",
                                                                    " · {format_duration(secs)}"
                                                                }
                                                            }
                                                        }
                                                        if track.match_status == ImportMatchStatus::Fuzzy {
                                                            if let Some(label) = track.matched_label.as_deref() {
                                                                p { class: "text-[10px] text-beet-gold truncate",
                                                                    "Closest in library: {label}"
                                                                }
                                                            }
                                                        }
                                                    }
                                                    span { class: "px-2 py-0.5 rounded-full border text-[10px] font-semibold shrink-0 {pill_cls}",
                                                        "{pill_text}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    ImportStep::Done(resp) => rsx! {
                        div { class: "space-y-3 py-2",
                            div { class: "flex items-center gap-2.5",
                                div { class: "w-9 h-9 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text flex items-center justify-center shrink-0",
                                    IconCheck { size: 17 }
                                }
                                div { class: "min-w-0",
                                    p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                                        "{resp.playlist_name}"
                                    }
                                    p { class: "text-[12px] text-beet-text-secondary",
                                        "Created and syncing tracks in order."
                                    }
                                }
                            }
                            div { class: "flex flex-wrap items-center gap-2",
                                span { class: "px-2.5 py-1 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text text-[11px] font-medium tabular-nums-mono",
                                    "{resp.already_in_library} added from library"
                                }
                                if resp.queued_for_download > 0 {
                                    span { class: "px-2.5 py-1 rounded-full bg-beet-accent-surface border border-beet-accent/30 text-beet-accent-text text-[11px] font-medium tabular-nums-mono",
                                        "{resp.queued_for_download} queued to download"
                                    }
                                }
                                if resp.already_running > 0 {
                                    span { class: "px-2.5 py-1 rounded-full bg-beet-surface border border-beet-border-subtle text-beet-text-secondary text-[11px] font-medium tabular-nums-mono",
                                        "{resp.already_running} already downloading"
                                    }
                                }
                                if resp.refused > 0 {
                                    span { class: "px-2.5 py-1 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text text-[11px] font-medium tabular-nums-mono",
                                        "{resp.refused} could not be queued"
                                    }
                                }
                            }
                            if resp.refused > 0 {
                                p { class: "text-[12px] text-beet-crimson-text leading-relaxed",
                                    "Some tracks could not be queued (for example, because you reached the active download limit or a library lookup failed). Wait for active jobs to finish and try again to add the rest."
                                }
                            }
                            if resp.queued_for_download > 0 || resp.already_running > 0 {
                                p { class: "text-[12px] text-beet-text-tertiary leading-relaxed",
                                    "Missing tracks will appear in your playlist automatically as each download finishes. Follow progress on the Activity page."
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}
