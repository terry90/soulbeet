use dioxus::prelude::*;
use shared::metadata::Track;

use super::download_icon::{DownloadIcon, DownloadRowState};

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub track: Track,
    pub index: usize,
    pub download_state: DownloadRowState,
    #[props(into)]
    pub on_download: EventHandler<()>,
}

#[component]
pub fn InlineTrackRow(props: Props) -> Element {
    let track = &props.track;
    let track_number = props.index + 1;

    rsx! {
        div {
            class: "flex items-center gap-3 px-2.5 py-2 hover:bg-beet-surface-high rounded-[8px] transition-colors",

            // Track number (1-based display)
            span {
                class: "text-[12px] tabular-nums-mono text-beet-text-tertiary w-6 text-right shrink-0",
                "{track_number}"
            }

            // Title
            span {
                class: "text-[14px] text-beet-text-primary flex-grow truncate",
                "{track.title}"
            }

            // Duration (conditional)
            if let Some(duration) = &track.duration {
                span {
                    class: "text-[12px] tabular-nums-mono text-beet-text-tertiary shrink-0",
                    "{duration}"
                }
            }

            // Per-track download
            DownloadIcon {
                state: props.download_state.clone(),
                on_download: props.on_download,
            }
        }
    }
}
