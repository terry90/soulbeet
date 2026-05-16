use api::models::folder::Folder;
use dioxus::prelude::*;
use shared::metadata::Track;
use std::collections::HashMap;

use super::download_icon::DownloadRowState;
use super::inline_track_row::InlineTrackRow;

const SKELETON_WIDTHS: [u8; 4] = [60, 75, 50, 65];

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub tracks: Option<Vec<Track>>,
    pub download_states: Signal<HashMap<String, DownloadRowState>>,
    pub folders: Vec<Folder>,
    pub selected_folder_id: Option<String>,
    pub active_menu: Signal<Option<String>>,
    #[props(into)]
    pub on_track_download: EventHandler<Track>,
    #[props(into)]
    pub on_track_override_download: EventHandler<(Track, Folder)>,
}

#[component]
pub fn InlineTrackPanel(props: Props) -> Element {
    let has_folders = !props.folders.is_empty();

    rsx! {
        div {
            class: "border-t border-white/10 bg-beet-dark/50 p-4",

            match &props.tracks {
                None => rsx! {
                    for width in SKELETON_WIDTHS {
                        div {
                            class: "flex items-center gap-3 p-2",
                            div { class: "w-6 h-4 bg-white/5 rounded animate-pulse" }
                            div {
                                class: "flex-grow h-4 bg-white/5 rounded animate-pulse",
                                style: "max-width: {width}%",
                            }
                            div { class: "w-10 h-4 bg-white/5 rounded animate-pulse" }
                            div { class: "w-5 h-5 bg-white/5 rounded-full animate-pulse" }
                        }
                    }
                },
                Some(tracks) if tracks.is_empty() => rsx! {
                    p {
                        class: "text-sm text-gray-500 font-mono text-center py-4",
                        "No tracks found for this album."
                    }
                },
                Some(tracks) => {
                    rsx! {
                        div {
                            class: "max-h-[400px] overflow-y-auto space-y-0.5",
                            for (idx, track) in tracks.iter().enumerate() {
                                {
                                    let dl_state = if !has_folders {
                                        DownloadRowState::Disabled
                                    } else {
                                        props.download_states.read().get(&track.id).cloned().unwrap_or_default()
                                    };
                                    let track_for_dl = track.clone();
                                    let track_for_override = track.clone();
                                    rsx! {
                                        InlineTrackRow {
                                            key: "{track.id}",
                                            track: track.clone(),
                                            index: idx,
                                            download_state: dl_state,
                                            folders: props.folders.clone(),
                                            selected_folder_id: props.selected_folder_id.clone(),
                                            active_menu: props.active_menu,
                                            on_download: move |_| {
                                                props.on_track_download.call(track_for_dl.clone());
                                            },
                                            on_override_download: move |folder: Folder| {
                                                props.on_track_override_download.call((track_for_override.clone(), folder));
                                            },
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
