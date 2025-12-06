use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use shared::slskd::{AlbumResult, TrackResult};
use std::collections::HashSet;

use crate::{use_auth, Checkbox};

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub results: Vec<AlbumResult>,
    #[props(into)]
    pub on_download: EventHandler<(Vec<TrackResult>, String)>,
}

#[derive(Props, Clone, PartialEq)]
struct AlbumResultItemProps {
    album: AlbumResult,
    selected_tracks: Signal<HashSet<String>>,
    on_album_select_all: EventHandler<AlbumResult>,
    on_track_toggle: EventHandler<String>,
}

#[component]
fn AlbumResultItem(props: AlbumResultItemProps) -> Element {
    let album = props.album.clone();

    rsx! {
        div { key: "{album.album_path}", class: "bg-gray-700 p-4 rounded-md",
            div { class: "flex justify-between items-center mb-2",
                div { class: "flex-grow",
                    h4 { class: "text-md font-bold", "{album.album_title}" }
                    p { class: "text-sm text-gray-400",
                        "{album.artist.clone().unwrap_or_default()} - Quality: {album.dominant_quality}, Score: {album.score:.2}"
                    }
                }
                button {
                    class: "bg-teal-600 hover:bg-teal-700 text-white font-semibold py-1 px-3 rounded-md text-sm transition-colors duration-300",
                    onclick: move |_| props.on_album_select_all.call(album.clone()),
                    "Select All"
                }
            }
            ul { class: "space-y-1",
                for TrackResult { base , title , .. } in props.album.tracks {
                    li {
                        key: "{base.filename}",
                        class: "flex items-center gap-2 p-1 rounded-md hover:bg-gray-600 cursor-pointer",
                        onclick: move |_| props.on_track_toggle.call(base.filename.clone()),

                        Checkbox { is_selected: props.selected_tracks.read().contains(&base.filename) }

                        label { class: "cursor-pointer", "{title}" }
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
    let auth = use_auth();

    use_future(move || async move {
        if let Some(token) = auth.token() {
            if let Ok(user_folders) = api::get_user_folders(token).await {
                info!("Fetched {} user folders", user_folders.len());

                // Only select if the user has exactly one folder
                // It could be error prone to auto-select if there are multiple folders
                // (I failed multiple times because of this)
                if user_folders.len() == 1 {
                    selected_folder.set(user_folders[0].path.clone());
                }
                folders.set(user_folders);
            }
        }
    });

    let handle_album_select_all = move |album_result: AlbumResult| {
        let mut selected = selected_tracks.write();
        let all_selected = album_result
            .tracks
            .iter()
            .all(|t| selected.contains(&t.base.filename));

        if all_selected {
            for track in &album_result.tracks {
                selected.remove(&track.base.filename);
            }
        } else {
            for track in &album_result.tracks {
                selected.insert(track.base.filename.clone());
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
        let selected_filenames = selected_tracks.read();
        let tracks_to_download: Vec<TrackResult> = props
            .results
            .iter()
            .flat_map(|album_result| album_result.tracks.iter())
            .filter(|track| selected_filenames.contains(&track.base.filename))
            .cloned()
            .collect();
        props
            .on_download
            .call((tracks_to_download, selected_folder()));
    };

    rsx! {
        div { class: "bg-gray-800 text-white p-6 sm:p-8 rounded-lg shadow-xl max-w-2xl mx-auto my-10 font-sans relative",
            h3 { class: "text-2xl font-bold mb-6 text-center text-teal-400", "Download Options" }
            // if !folders.read().is_empty() {
            div { class: "mb-4",
                label { class: "block text-sm font-medium mb-1", "Select Target Folder" }
                select {
                    class: "w-full p-2 rounded bg-gray-700 border border-gray-600 focus:border-teal-500 focus:outline-none",
                    value: "{selected_folder}",
                    onchange: move |e| selected_folder.set(e.value()),
                    for folder in folders.read().iter() {
                        option { value: "{folder.path}", "{folder.name}" }
                    }
                }
            }
            // }

            div { class: "space-y-4 mb-20",
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
                    class: "bg-teal-600 hover:bg-teal-700 text-white font-bold p-4 rounded-full shadow-lg transition-transform hover:scale-105 disabled:bg-gray-600 disabled:cursor-not-allowed flex items-center justify-center",
                    disabled: selected_tracks.read().is_empty() || selected_folder.read().is_empty(),
                    onclick: handle_download,
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
