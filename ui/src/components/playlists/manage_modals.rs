//! Playlist management modals: Create/Edit Playlist and searchable Add-to-Playlist.

use dioxus::prelude::*;

use crate::{
    friendly_error, use_auth, IconCheck, IconListMusic, IconLoader, IconPlus, IconSearch, Modal,
};

/// Lightweight playlist choice passed into [`AddToPlaylistModal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistTargetOption {
    pub id: String,
    pub name: String,
    pub song_count: u32,
}

/// Modal to create a new Navidrome playlist or edit an existing playlist's name,
/// description, and public visibility.
#[component]
pub fn CreateEditPlaylistModal(
    /// When `Some`, edits the existing playlist; when `None`, creates a new playlist.
    #[props(default)]
    editing_playlist_id: Option<String>,
    #[props(default)] initial_name: String,
    #[props(default)] initial_comment: String,
    #[props(default)] initial_public: bool,
    /// Optional initial track IDs when creating a playlist from a mix or queue.
    #[props(default)]
    initial_song_ids: Vec<String>,
    on_saved: EventHandler<api::PlaylistMutationResult>,
    on_close: EventHandler<()>,
) -> Element {
    let auth = use_auth();
    let is_edit = editing_playlist_id.is_some();

    let mut name = use_signal(move || initial_name);
    let mut comment = use_signal(move || initial_comment);
    let mut is_public = use_signal(move || initial_public);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let edit_id_for_submit = editing_playlist_id.clone();
    let song_ids_for_submit = initial_song_ids.clone();

    let submit = move || {
        let trimmed_name = name().trim().to_string();
        if trimmed_name.is_empty() {
            error.set(Some("Please enter a playlist name.".to_string()));
            return;
        }
        if busy() {
            return;
        }
        busy.set(true);
        error.set(None);
        let desc = comment().trim().to_string();
        let comment_opt = if desc.is_empty() {
            Some(String::new())
        } else {
            Some(desc)
        };
        let pub_flag = is_public();
        let edit_id = edit_id_for_submit.clone();
        let initial_ids = song_ids_for_submit.clone();

        spawn(async move {
            let res = if let Some(pid) = edit_id {
                auth.call(api::update_playlist(
                    pid,
                    Some(trimmed_name),
                    comment_opt,
                    Some(pub_flag),
                ))
                .await
            } else {
                let ids_opt = if initial_ids.is_empty() {
                    None
                } else {
                    Some(initial_ids)
                };
                auth.call(api::create_playlist(
                    trimmed_name,
                    comment_opt,
                    Some(pub_flag),
                    ids_opt,
                ))
                .await
            };

            busy.set(false);
            match res {
                Ok(saved) => on_saved.call(saved),
                Err(err) => error.set(Some(friendly_error(&err))),
            }
        });
    };

    let heading = if is_edit {
        "Edit Playlist"
    } else if !initial_song_ids.is_empty() {
        "Save as Playlist"
    } else {
        "New Playlist"
    };

    let submit_label = if busy() {
        if is_edit {
            "Saving..."
        } else {
            "Creating..."
        }
    } else if is_edit {
        "Save Changes"
    } else {
        "Create Playlist"
    };

    let can_submit = !name().trim().is_empty() && !busy();
    let mut submit_form = submit.clone();
    let mut submit_btn = submit;

    rsx! {
        Modal {
            on_close: move |_| {
                if !busy() {
                    on_close.call(());
                }
            },
            header: rsx! {
                div { class: "flex items-center gap-2.5",
                    div { class: "w-8 h-8 rounded-[8px] bg-beet-accent-surface text-beet-accent-text flex items-center justify-center shrink-0",
                        IconListMusic { size: 16 }
                    }
                    div {
                        h2 { class: "text-[16px] font-semibold text-beet-text-primary leading-tight",
                            "{heading}"
                        }
                        if !is_edit && !initial_song_ids.is_empty() {
                            p { class: "text-[12px] text-beet-text-tertiary mt-0.5 tabular-nums-mono",
                                "{initial_song_ids.len()} tracks will be added"
                            }
                        }
                    }
                }
            },
            footer: rsx! {
                div { class: "flex items-center justify-end gap-2",
                    button {
                        r#type: "button",
                        disabled: busy(),
                        class: "h-9 px-3.5 rounded-[8px] text-[13px] font-medium text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors cursor-pointer",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        r#type: "button",
                        disabled: !can_submit,
                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-[8px] bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-[color,background-color,border-color,filter] cursor-pointer",
                        onclick: move |_| submit_btn(),
                        if busy() {
                            IconLoader { size: 14 }
                        }
                        span { "{submit_label}" }
                    }
                }
            },

            form {
                class: "space-y-4",
                onsubmit: move |evt| {
                    evt.prevent_default();
                    submit_form();
                },
                if let Some(err) = error() {
                    div { class: "px-3 py-2.5 rounded-[8px] bg-beet-crimson-surface border border-beet-crimson-text/30 text-[12px] text-beet-crimson-text",
                        "{err}"
                    }
                }
                div { class: "space-y-1.5",
                    label { class: "block text-[12px] font-medium text-beet-text-secondary",
                        "Name"
                    }
                    input {
                        r#type: "text",
                        value: "{name}",
                        placeholder: "Late Night Drive",
                        maxlength: "255",
                        autofocus: true,
                        class: "w-full h-10 px-3 rounded-[9px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-[14px] text-beet-text-primary placeholder:text-beet-text-tertiary",
                        oninput: move |evt| name.set(evt.value()),
                    }
                }
                div { class: "space-y-1.5",
                    label { class: "block text-[12px] font-medium text-beet-text-secondary",
                        "Description"
                    }
                    textarea {
                        rows: 3,
                        value: "{comment}",
                        placeholder: "Optional notes about this playlist...",
                        maxlength: "2000",
                        class: "w-full p-3 rounded-[9px] bg-beet-surface-low border border-beet-border-default focus:border-beet-leaf-solid/60 focus:outline-none text-[13px] text-beet-text-primary placeholder:text-beet-text-tertiary resize-none",
                        oninput: move |evt| comment.set(evt.value()),
                    }
                }
                label { class: "flex items-center justify-between gap-3 p-3 rounded-[9px] bg-beet-surface-low border border-beet-border-subtle cursor-pointer",
                    div {
                        p { class: "text-[13px] font-medium text-beet-text-primary",
                            "Public playlist"
                        }
                        p { class: "text-[11px] text-beet-text-tertiary",
                            "Visible to other listeners on your Navidrome server."
                        }
                    }
                    input {
                        r#type: "checkbox",
                        checked: is_public(),
                        class: "w-4 h-4 accent-beet-leaf-solid rounded cursor-pointer",
                        onchange: move |evt| is_public.set(evt.checked()),
                    }
                }
            }
        }
    }
}

/// Searchable modal to add one or more tracks to an existing static user playlist,
/// or create a new playlist inline in one step.
#[component]
pub fn AddToPlaylistModal(
    /// Short label describing what is being added (e.g. track title or `"12 tracks"`).
    item_label: String,
    song_ids: Vec<String>,
    playlists: Vec<PlaylistTargetOption>,
    on_added: EventHandler<api::PlaylistMutationResult>,
    on_close: EventHandler<()>,
) -> Element {
    let auth = use_auth();
    let mut filter = use_signal(String::new);
    let mut show_quick_create = use_signal(|| false);
    let mut new_name = use_signal(String::new);
    let mut busy_id = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);

    let q = filter().trim().to_ascii_lowercase();
    let filtered: Vec<PlaylistTargetOption> = playlists
        .into_iter()
        .filter(|pl| q.is_empty() || pl.name.to_ascii_lowercase().contains(&q))
        .collect();

    let song_ids_for_create = song_ids.clone();
    let mut create_and_add = move || {
        let trimmed = new_name().trim().to_string();
        if trimmed.is_empty() || busy_id().is_some() {
            return;
        }
        busy_id.set(Some("__new__".to_string()));
        error.set(None);
        let ids = song_ids_for_create.clone();
        spawn(async move {
            match auth
                .call(api::create_playlist(trimmed, None, Some(false), Some(ids)))
                .await
            {
                Ok(res) => {
                    busy_id.set(None);
                    on_added.call(res);
                }
                Err(err) => {
                    busy_id.set(None);
                    error.set(Some(friendly_error(&err)));
                }
            }
        });
    };

    rsx! {
        Modal {
            on_close: move |_| {
                if busy_id().is_none() {
                    on_close.call(());
                }
            },
            header: rsx! {
                div { class: "min-w-0",
                    h2 { class: "text-[16px] font-semibold text-beet-text-primary leading-tight",
                        "Add to Playlist"
                    }
                    p { class: "text-[12px] text-beet-text-secondary truncate mt-0.5",
                        "{item_label}"
                    }
                }
            },

            div { class: "space-y-3",
                if let Some(err) = error() {
                    div { class: "px-3 py-2 rounded-[8px] bg-beet-crimson-surface border border-beet-crimson-text/30 text-[12px] text-beet-crimson-text",
                        "{err}"
                    }
                }

                // Filter input
                div { class: "flex items-center gap-2 h-9 px-3 rounded-[9px] bg-beet-surface-low border border-beet-border-default focus-within:border-beet-leaf-solid/60",
                    IconSearch { size: 14, class: "text-beet-text-tertiary".to_string() }
                    input {
                        r#type: "text",
                        value: "{filter}",
                        placeholder: "Filter playlists...",
                        class: "flex-1 bg-transparent text-[13px] text-beet-text-primary placeholder:text-beet-text-tertiary focus:outline-none",
                        oninput: move |evt| filter.set(evt.value()),
                    }
                }

                // Quick "+ New playlist" inline row
                if show_quick_create() {
                    form {
                        class: "flex items-center gap-2 p-2 rounded-[9px] bg-beet-surface-low border border-beet-border-default",
                        onsubmit: move |evt| {
                            evt.prevent_default();
                            create_and_add();
                        },
                        input {
                            r#type: "text",
                            value: "{new_name}",
                            placeholder: "New playlist name...",
                            autofocus: true,
                            class: "flex-1 min-w-0 h-8 px-2.5 rounded-[7px] bg-beet-surface border border-beet-border-subtle focus:border-beet-leaf-solid/60 focus:outline-none text-[13px] text-beet-text-primary placeholder:text-beet-text-tertiary",
                            oninput: move |evt| new_name.set(evt.value()),
                        }
                        button {
                            r#type: "submit",
                            disabled: new_name().trim().is_empty() || busy_id().is_some(),
                            class: "inline-flex items-center gap-1 h-8 px-3 rounded-[7px] bg-beet-leaf-solid text-beet-leaf-ink text-[12px] font-semibold hover:brightness-105 disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-[color,background-color,border-color,filter] cursor-pointer shrink-0",
                            if busy_id().as_deref() == Some("__new__") {
                                IconLoader { size: 12 }
                            } else {
                                IconCheck { size: 12 }
                            }
                            span { "Create" }
                        }
                    }
                } else {
                    button {
                        r#type: "button",
                        class: "w-full flex items-center gap-2.5 px-3 py-2 rounded-[9px] bg-beet-surface-low border border-dashed border-beet-border-default hover:border-beet-leaf-solid/50 hover:text-beet-leaf-text text-[13px] font-medium text-beet-text-secondary transition-colors cursor-pointer",
                        onclick: move |_| {
                            new_name.set(filter().trim().to_string());
                            show_quick_create.set(true);
                        },
                        IconPlus { size: 15 }
                        span { "New playlist" }
                    }
                }

                // Scrollable static playlists list
                div { class: "max-h-64 overflow-y-auto studio-scrollbar space-y-1",
                    if filtered.is_empty() {
                        p { class: "py-6 text-center text-[12px] text-beet-text-tertiary",
                            "No matching personal playlists. Create one above."
                        }
                    } else {
                        for pl in filtered {
                            {
                                let pid = pl.id.clone();
                                let pid_busy = busy_id().as_deref() == Some(pl.id.as_str());
                                let ids_for_row = song_ids.clone();
                                rsx! {
                                    button {
                                        key: "add-pl-{pl.id}",
                                        r#type: "button",
                                        disabled: busy_id().is_some(),
                                        class: "w-full flex items-center justify-between gap-3 px-3 py-2.5 rounded-[9px] hover:bg-beet-surface-high border border-transparent hover:border-beet-border-subtle text-left transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none",
                                        onclick: move |_| {
                                            if busy_id().is_some() {
                                                return;
                                            }
                                            busy_id.set(Some(pid.clone()));
                                            error.set(None);
                                            let target_pid = pid.clone();
                                            let ids = ids_for_row.clone();
                                            spawn(async move {
                                                match auth.call(api::add_playlist_tracks(target_pid, ids)).await {
                                                    Ok(res) => {
                                                        busy_id.set(None);
                                                        on_added.call(res);
                                                    }
                                                    Err(err) => {
                                                        busy_id.set(None);
                                                        error.set(Some(friendly_error(&err)));
                                                    }
                                                }
                                            });
                                        },
                                        span { class: "flex items-center gap-2.5 min-w-0",
                                            IconListMusic { size: 15, class: "text-beet-text-tertiary shrink-0".to_string() }
                                            span { class: "text-[13px] font-medium text-beet-text-primary truncate",
                                                "{pl.name}"
                                            }
                                        }
                                        span { class: "tabular-nums-mono text-[11px] text-beet-text-tertiary shrink-0",
                                            if pid_busy {
                                                IconLoader { size: 13 }
                                            } else {
                                                "{pl.song_count}"
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
