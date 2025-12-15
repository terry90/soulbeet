use dioxus::prelude::*;
use shared::musicbrainz::Track;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub track: Track,
    pub on_album_click: EventHandler<String>,
}

#[component]
pub fn TrackResult(props: Props) -> Element {
    let track = props.track.clone();

    rsx! {
      div { class: "bg-white/5 border border-white/5 p-4 rounded-lg hover:border-beet-accent/50 hover:bg-white/10 transition-all duration-200 group",

        div { class: "flex justify-between items-center",

          div {
            h5 { class: "text-lg font-bold text-white group-hover:text-beet-accent transition-colors",
              "{track.title}"
            }
            p { class: "text-md text-gray-400 font-mono", "{track.artist}" }

            if let (Some(album_title), Some(album_id)) = (&track.album_title, &track.album_id) {
              {
                  let album_id = album_id.clone();
                  rsx! {
                    p {
                      class: "text-sm text-gray-500 italic cursor-pointer hover:text-beet-leaf transition-colors mt-1",
                      onclick: move |_| props.on_album_click.call(album_id.clone()),
                      "from \"{album_title}\""
                    }
                  }
              }
            }
          }

          if let Some(duration) = &track.duration {
            p { class: "text-sm font-mono text-gray-500 whitespace-nowrap pl-4",
              "{duration}"
            }
          }
        }
      }
    }
}
