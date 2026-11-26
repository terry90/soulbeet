use dioxus::prelude::*;
use shared::metadata::Album;

use crate::CoverArt;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    album: Album,
}

#[component]
pub fn AlbumHeader(props: Props) -> Element {
    rsx! {
      div { class: "flex items-start gap-4 p-4 border-b border-beet-border-subtle",
        CoverArt { album: props.album.clone() }
        div { class: "flex-grow min-w-0",
          h3 { class: "text-[20px] font-bold text-beet-text-primary font-display truncate",
            "{props.album.title}"
          }
          p { class: "text-[14px] text-beet-text-secondary truncate mt-0.5", "{props.album.artist}" }
          if let Some(date) = &props.album.release_date {
            p { class: "text-[12px] text-beet-text-tertiary tabular-nums-mono mt-1", "{date}" }
          }
        }
      }
    }
}
