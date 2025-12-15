use dioxus::prelude::*;

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum SearchType {
    Album,
    Track,
}

#[component]
pub fn SearchTypeToggle(search_type: Signal<SearchType>) -> Element {
    let active_class = "text-white bg-white/10 shadow-sm";
    let inactive_class = "text-gray-500 hover:text-gray-300 hover:bg-white/5";

    let album_class = if search_type() == SearchType::Album {
        active_class
    } else {
        inactive_class
    };
    let track_class = if search_type() == SearchType::Track {
        active_class
    } else {
        inactive_class
    };

    rsx! {
      div { class: "flex items-center bg-black/20 rounded p-1 mr-2",
        button {
          class: "px-3 py-1 text-xs font-bold rounded transition-all duration-200 {album_class}",
          onclick: move |_| search_type.set(SearchType::Album),
          "ALBUM"
        }
        button {
          class: "px-3 py-1 text-xs font-bold rounded transition-all duration-200 {track_class}",
          onclick: move |_| search_type.set(SearchType::Track),
          "TRACK"
        }
      }
    }
}
