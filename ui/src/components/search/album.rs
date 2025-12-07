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
        class: "bg-gray-700 p-4 rounded-lg shadow-md hover:bg-gray-600 transition-colors duration-200 flex items-center gap-4",

        CoverArt { src: cover_art_url, alt: alt_text }

        div { class: "flex-grow flex flex-col justify-center",
          h5 { class: "text-lg font-bold text-indigo-300", "{album.title}" }
          p { class: "text-md text-gray-300", "{album.artist}" }
          if let Some(release_date) = &album.release_date {
            p { class: "text-sm text-gray-400 mt-1", "{release_date}" }
          }
        }
      }
    }
}
