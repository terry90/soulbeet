use std::collections::HashSet;

use dioxus::prelude::*;
use shared::musicbrainz::Track;

use crate::album::track_item::TrackItem;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    tracks: Signal<Vec<Track>>,
    selected_tracks: Signal<HashSet<String>>,
    on_toggle_select_all: EventHandler,
    on_track_toggle: EventHandler<String>,
    all_selected: bool,
}

#[component]
pub fn TrackList(props: Props) -> Element {
    rsx! {
      ul { class: "list-none p-4 space-y-2 overflow-y-auto",
        li {
          class: "flex items-center gap-3 p-2 rounded-md cursor-pointer hover:bg-gray-700",
          onclick: move |_| props.on_toggle_select_all.call(()),
          div { class: if props.all_selected { "w-5 h-5 border-2 rounded flex items-center justify-center border-teal-400 bg-teal-500" } else { "w-5 h-5 border-2 rounded flex items-center justify-center border-gray-500" },
            if props.all_selected {
              "✓"
            }
          }
          span { class: "font-bold", "Select / Deselect All" }
        }
        for track in props.tracks.read().iter() {
          TrackItem {
            key: "{track.id}",
            track: track.clone(),
            is_selected: props.selected_tracks.read().contains(&track.id),
            on_toggle: props.on_track_toggle.clone(),
          }
        }
      }
    }
}
