use dioxus::prelude::*;
use shared::musicbrainz::Album;

use crate::CoverArt;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    album: Album,
}

#[component]
pub fn AlbumHeader(props: Props) -> Element {
    rsx! {
      div { class: "flex items-start gap-4 p-4 border-b border-white/10",
        CoverArt {
          src: format!("https://coverartarchive.org/release/{}/front-250", props.album.id),
          alt: format!("Cover for {}", props.album.title),
        }
        div { class: "flex-grow",
          h3 { class: "text-2xl font-bold text-beet-accent font-display",
            "{props.album.title}"
          }
          p { class: "text-lg text-white font-mono", "{props.album.artist}" }
          if let Some(date) = &props.album.release_date {
            p { class: "text-sm text-gray-400 font-mono", "{date}" }
          }
        }
      }
    }
}
