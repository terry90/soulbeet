use dioxus::prelude::*;
use shared::musicbrainz::Track;

use crate::Checkbox;

#[derive(Props, Clone, PartialEq)]
pub struct Props {
    track: Track,
    is_selected: bool,
    on_toggle: EventHandler<String>,
}

#[component]
pub fn TrackItem(props: Props) -> Element {
    let track_id = props.track.id.clone();

    rsx! {
      li {
        class: "flex items-center gap-3 p-2 rounded-md cursor-pointer transition-colors border border-transparent",
        class: if props.is_selected { "bg-beet-leaf/10 border-beet-leaf/30" } else { "hover:bg-white/5 border-white/5" },
        onclick: move |_| props.on_toggle.call(track_id.clone()),
        Checkbox { is_selected: props.is_selected }

        span {
          class: "flex-grow font-mono text-sm",
          class: if props.is_selected { "text-beet-leaf" } else { "text-gray-300" },
          "{props.track.title}"
        }
        if let Some(duration) = &props.track.duration {
          span {
            class: "font-mono text-xs",
            class: if props.is_selected { "text-beet-leaf/70" } else { "text-gray-500" },
            "{duration}"
          }
        }
      }
    }
}
