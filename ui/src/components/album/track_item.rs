use dioxus::prelude::*;
use shared::metadata::Track;

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
        class: "flex items-center gap-3 px-3 py-2 rounded-[8px] cursor-pointer transition-colors border",
        class: if props.is_selected {
          "bg-beet-leaf-surface border-beet-leaf/30"
        } else {
          "bg-transparent hover:bg-beet-surface-high border-beet-border-subtle"
        },
        onclick: move |_| props.on_toggle.call(track_id.clone()),
        Checkbox { is_selected: props.is_selected }

        span {
          class: "flex-grow text-[14px] truncate",
          class: if props.is_selected { "text-beet-leaf-text font-medium" } else { "text-beet-text-primary" },
          "{props.track.title}"
        }
        if let Some(duration) = &props.track.duration {
          span {
            class: "tabular-nums-mono text-[12px] shrink-0",
            class: if props.is_selected { "text-beet-leaf-text/80" } else { "text-beet-text-tertiary" },
            "{duration}"
          }
        }
      }
    }
}
