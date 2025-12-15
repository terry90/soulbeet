use dioxus::prelude::*;
use shared::musicbrainz::Album;

use crate::CoverArt;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub album: Album,
    pub on_click: EventHandler<String>,
}

#[component]
pub fn AlbumResult(props: Props) -> Element {
    let album_id = props.album.id.clone();
    let album = &props.album;

    let cover_art_url = format!("https://coverartarchive.org/release/{}/front-250", album_id);
    let alt_text = format!("Album cover for {}", album.title);

    rsx! {
      div {
        onclick: move |_| props.on_click.call(album_id.clone()),
        class: "bg-white/5 border border-white/5 p-4 rounded-lg hover:border-beet-accent/50 hover:bg-white/10 transition-all duration-200 flex items-center gap-4 cursor-pointer group",

        CoverArt { src: cover_art_url, alt: alt_text }

        div { class: "flex-grow flex flex-col justify-center",
          h5 { class: "text-lg font-bold text-white group-hover:text-beet-accent transition-colors",
            "{album.title}"
          }
          p { class: "text-md text-gray-400 font-mono", "{album.artist}" }
          if let Some(release_date) = &album.release_date {
            p { class: "text-sm text-gray-500 mt-1 font-mono", "{release_date}" }
          }
        }
      }
    }
}
