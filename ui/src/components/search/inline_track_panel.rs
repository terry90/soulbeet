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
    pub has_folder: bool,
    #[props(into)]
    pub on_track_download: EventHandler<Track>,
}

#[component]
pub fn InlineTrackPanel(props: Props) -> Element {
    let has_folder = props.has_folder;

    rsx! {
        div {
            class: "border-t border-beet-border-subtle bg-beet-surface-low rounded-b-[12px] p-3",

            match &props.tracks {
                None => rsx! {
                    for width in SKELETON_WIDTHS {
                        div {
                            class: "flex items-center gap-3 p-2",
                            div { class: "w-6 h-4 bg-beet-surface-high rounded animate-pulse" }
                            div {
                                class: "flex-grow h-4 bg-beet-surface-high rounded animate-pulse",
                                style: "max-width: {width}%",
                            }
                            div { class: "w-10 h-4 bg-beet-surface-high rounded animate-pulse" }
                            div { class: "w-5 h-5 bg-beet-surface-high rounded-full animate-pulse" }
                        }
                    }
                },
                Some(tracks) if tracks.is_empty() => rsx! {
                    p {
                        class: "text-[13px] text-beet-text-tertiary text-center py-4",
                        "No tracks found for this album."
                    }
                },
                Some(tracks) => {
                    rsx! {
                        div {
                            class: "max-h-[400px] overflow-y-auto studio-scrollbar space-y-0.5",
                            for (idx, track) in tracks.iter().enumerate() {
                                {
                                    let dl_state = if !has_folder {
                                        DownloadRowState::Disabled
                                    } else {
                                        props.download_states.read().get(&track.id).cloned().unwrap_or_default()
                                    };
                                    let track_for_dl = track.clone();
                                    rsx! {
                                        InlineTrackRow {
                                            key: "{track.id}",
                                            track: track.clone(),
                                            index: idx,
                                            download_state: dl_state,
                                            on_download: move |_| {
                                                props.on_track_download.call(track_for_dl.clone());
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
