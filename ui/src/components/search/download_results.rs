use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use shared::download::{DownloadableGroup, DownloadableItem};
use std::collections::HashSet;

use crate::{use_auth, Checkbox};

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub results: Vec<DownloadableGroup>,
    pub is_searching: bool,
    pub is_downloading: Signal<bool>,
    #[props(into)]
    pub on_download: EventHandler<(Vec<DownloadableItem>, String)>,
    #[props(into)]
    pub on_back: EventHandler<()>,
}

#[derive(Props, Clone, PartialEq)]
struct AlbumResultItemProps {
    album: DownloadableGroup,
    selected_tracks: Signal<HashSet<String>>,
    on_album_select_all: EventHandler<DownloadableGroup>,
    on_track_toggle: EventHandler<String>,
}

#[derive(Props, Clone, PartialEq)]
struct TrackItemProps {
    track: DownloadableItem,
    is_selected: bool,
    on_toggle: EventHandler<String>,
}

fn get_track_id(track: &DownloadableItem) -> String {
    format!("{}{}", track.id, track.source)
}

#[component]
fn TrackItem(props: TrackItemProps) -> Element {
    let unique_id = get_track_id(&props.track);

    rsx! {
        li {
            key: "{unique_id}",
            class: "flex items-center gap-2 p-1 rounded-md hover:bg-white/10 cursor-pointer",
            onclick: move |_| props.on_toggle.call(unique_id.clone()),

            Checkbox { is_selected: props.is_selected }

            label { class: "cursor-pointer text-gray-300 font-mono text-sm", "{props.track.title}" }
        }
    }
}

#[component]
fn AlbumResultItem(props: AlbumResultItemProps) -> Element {
    let album = props.album.clone();

    rsx! {
        div {
            key: "{album.group_id}",
            class: "bg-white/5 border border-white/5 p-4 rounded-md",
            div { class: "flex justify-between items-center mb-2",
                div { class: "flex-grow",
                    h4 { class: "text-md font-bold text-beet-leaf", "{album.title}" }
                    p { class: "text-sm text-gray-400 font-mono",
                        "{album.artist.clone().unwrap_or_default()} - Quality: {album.quality}, Score: {album.score:.2}"
                    }
                }
                button {
                    class: "font-mono uppercase text-[10px] whitespace-nowrap tracking-widest px-3 py-1 border border-beet-leaf/30 text-beet-leaf hover:bg-beet-leaf hover:text-beet-dark transition-colors cursor-pointer rounded",
                    onclick: move |_| props.on_album_select_all.call(album.clone()),
                    "Select All"
                }
            }
            ul { class: "space-y-1",
                for track in props.album.items {
                    TrackItem {
                        is_selected: props.selected_tracks.read().contains(&get_track_id(&track)),
                        track,
                        on_toggle: props.on_track_toggle,
                    }
                }

            }
        }
    }
}

/// Main component responsible for displaying all download options.
#[component]
pub fn DownloadResults(props: Props) -> Element {
    let mut selected_tracks = use_signal(HashSet::<String>::new);
    let results = props.results.clone();
    let mut folders = use_signal(std::vec::Vec::new);
    let mut selected_folder = use_signal(|| "".to_string());
    let mut is_downloading = props.is_downloading;
    let auth = use_auth();

    use_future(move || async move {
        if let Ok(user_folders) = auth.call(api::get_user_folders()).await {
            info!("Fetched {} user folders", user_folders.len());

            // Only select if the user has exactly one folder
            // It could be error prone to auto-select if there are multiple folders
            // (I failed multiple times because of this)
            if user_folders.len() == 1 {
                selected_folder.set(user_folders[0].path.clone());
            }
            folders.set(user_folders);
        }
    });

    let handle_album_select_all = move |group: DownloadableGroup| {
        let mut selected = selected_tracks.write();
        let all_selected = group
            .items
            .iter()
            .all(|t| selected.contains(&get_track_id(t)));

        if all_selected {
            for item in &group.items {
                selected.remove(&get_track_id(item));
            }
        } else {
            for item in &group.items {
                selected.insert(get_track_id(item));
            }
        }
    };

    let handle_track_toggle = move |filename: String| {
        info!("Toggle track selection: {}", filename);
        let mut selected = selected_tracks.write();
        if selected.contains(&filename) {
            selected.remove(&filename);
        } else {
            selected.insert(filename);
        }
    };

    let handle_download = move |_| {
        // Prevent double-clicks by checking if already downloading
        if *is_downloading.read() {
            info!("Download already in progress, ignoring click");
            return;
        }

        let selected_ids = selected_tracks.read();

        let items_to_download: Vec<DownloadableItem> = props
            .results
            .iter()
            .flat_map(|group| group.items.iter())
            .filter(|item| selected_ids.contains(&get_track_id(item)))
            .cloned()
            .collect();

        if items_to_download.is_empty() {
            return;
        }

        // Set downloading state immediately to prevent double-clicks
        is_downloading.set(true);

        props
            .on_download
            .call((items_to_download, selected_folder()));
    };

    rsx! {
        div { class: "bg-beet-panel border border-white/10 text-white p-6 sm:p-8 rounded-lg shadow-2xl w-full max-w-2xl mx-auto my-10 font-display relative",
            div { class: "relative mb-6",
                button {
                    class: "absolute left-0 top-1/2 -translate-y-1/2 p-2 rounded-full hover:bg-white/10 transition-colors cursor-pointer",
                    onclick: move |_| props.on_back.call(()),
                    svg {
                        class: "w-5 h-5 text-gray-400",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M15 19l-7-7 7-7",
                        }
                    }
                }
                h3 { class: "text-2xl font-bold text-center text-beet-accent", "Download Options" }
            }
            div { class: "mb-4",
                label {
                    r#for: "dl_folder",
                    class: "block text-sm font-medium mb-1 text-gray-400 font-mono",
                    "Select Target Folder"
                }
                select {
                    name: "dl_folder",
                    class: "w-full p-2 rounded bg-beet-dark border border-white/10 focus:border-beet-accent focus:outline-none text-white font-mono",
                    value: "{selected_folder}",
                    onchange: move |e| selected_folder.set(e.value()),
                    option { value: "", disabled: true, "Select a folder" }
                    for folder in folders.read().iter() {
                        option { value: "{folder.path}", "{folder.name}" }
                    }
                }
            }

            div { class: "space-y-4",
                if props.is_searching {
                    div { class: "flex flex-col items-center justify-center p-4 bg-white/5 rounded-lg",
                        div { class: "animate-spin rounded-full h-8 w-8 border-t-2 border-b-2 border-beet-accent mb-2" }
                        p { class: "text-sm text-gray-300 animate-pulse text-center font-mono",
                            "Searching... The rarer your track is, the longer the search can take."
                        }
                    }
                } else if results.is_empty() {
                    div { class: "text-center text-gray-500 py-8 font-mono", "No results found" }
                }
                for album in results {
                    AlbumResultItem {
                        album,
                        selected_tracks,
                        on_album_select_all: handle_album_select_all,
                        on_track_toggle: handle_track_toggle,
                    }
                }
            }
            div { class: "fixed bottom-8 right-8",
                button {
                    class: "bg-beet-accent hover:bg-fuchsia-400 text-white font-bold p-4 rounded-full shadow-[0_0_15px_rgba(255,0,255,0.5)] transition-transform hover:scale-105 disabled:bg-gray-600 disabled:cursor-not-allowed disabled:shadow-none flex items-center justify-center cursor-pointer",
                    disabled: selected_tracks.read().is_empty() || selected_folder.read().is_empty() || *is_downloading.read(),
                    onclick: handle_download,
                    if *is_downloading.read() {
                        // Show spinner when downloading
                        div { class: "animate-spin rounded-full h-6 w-6 border-t-2 border-b-2 border-white" }
                    } else {
                        svg {
                            class: "w-6 h-6",
                            fill: "none",
                            stroke: "currentColor",
                            view_box: "0 0 24 24",
                            path {
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                stroke_width: "2",
                                d: "M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4",
                            }
                        }
                    }
                }
            }
        }
    }
}
