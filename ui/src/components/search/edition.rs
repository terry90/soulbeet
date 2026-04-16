use dioxus::prelude::*;
use shared::metadata::Album;

use crate::CoverArt;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub album: Album,
    pub on_click: Callback,
}

#[component]
pub fn EditionResult(props: Props) -> Element {
    let album = &props.album;

    rsx! {
      div {
        onclick: move |_| props.on_click.call(()),
        class: "bg-white/5 border border-white/5 p-4 rounded-lg hover:border-beet-accent/50 hover:bg-white/10 transition-all duration-200 flex items-center gap-4 cursor-pointer group",

        CoverArt { album: album.clone() }

        div { class: "flex-grow flex flex-col justify-center",
          h5 { class: "text-lg font-bold text-white group-hover:text-beet-accent transition-colors",
            "{album.title}"
          }
          p { class: "text-md text-gray-400 font-mono", "{album.artist}" }
          if let Some(release_date) = album.release_date.as_ref().filter(|s| !s.is_empty()) {
            p { class: "text-sm text-gray-500 mt-1 font-mono", "{release_date}" }
          }
        }
      }
    }
}