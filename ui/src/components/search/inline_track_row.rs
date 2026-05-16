use api::models::folder::Folder;
use dioxus::prelude::*;
use shared::metadata::Track;

use super::download_icon::{DownloadIcon, DownloadRowState};

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub track: Track,
    pub index: usize,
    pub download_state: DownloadRowState,
    pub folders: Vec<Folder>,
    pub selected_folder_id: Option<String>,
    pub active_menu: Signal<Option<String>>,
    #[props(into)]
    pub on_download: EventHandler<()>,
    #[props(into)]
    pub on_override_download: EventHandler<Folder>,
}

#[component]
pub fn InlineTrackRow(props: Props) -> Element {
    let track = &props.track;
    let track_number = props.index + 1;

    rsx! {
        div {
            class: "flex items-center gap-3 p-2 hover:bg-white/5 rounded transition-colors",

            // Track number (1-based display)
            span {
                class: "text-xs font-mono text-gray-600 w-6 text-right shrink-0",
                "{track_number}"
            }

            // Title
            span {
                class: "text-sm font-mono text-gray-300 flex-grow truncate",
                "{track.title}"
            }

            // Duration (conditional)
            if let Some(duration) = &track.duration {
                span {
                    class: "text-xs font-mono text-gray-500 shrink-0",
                    "{duration}"
                }
            }

            // Track from find_album (MusicBrainz) lacks quality metadata. Badge hidden per D-06 caveat.

            // Per-track download
            DownloadIcon {
                item_id: track.id.clone(),
                state: props.download_state.clone(),
                folders: props.folders.clone(),
                selected_folder_id: props.selected_folder_id.clone(),
                active_menu: props.active_menu,
                on_download: props.on_download,
                on_override_download: props.on_override_download,
            }
        }
    }
}
