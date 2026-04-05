use api::models::folder::Folder;
use dioxus::prelude::*;
use shared::metadata::Track;

use super::download_icon::{DownloadIcon, DownloadRowState};

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub track: Track,
    pub on_album_click: Callback,
    pub on_search_sources: Callback,
    pub download_state: DownloadRowState,
    pub folders: Vec<Folder>,
    pub selected_folder_id: Option<String>,
    pub active_menu: Signal<Option<String>>,
    #[props(into)]
    pub on_download: EventHandler<()>,
    #[props(into)]
    pub on_override_download: EventHandler<Folder>,
}

#[component]
pub fn TrackResult(props: Props) -> Element {
    let track = props.track.clone();
    let mut cover_error = use_signal(|| false);

    let cover_url = track.release_mbid.as_ref().map(|mbid| {
        format!("https://coverartarchive.org/release/{}/front-250", mbid)
    });

    rsx! {
      div {
        class: "bg-white/5 border border-white/5 rounded-lg hover:border-white/10 transition-all duration-200 group flex items-center gap-3 p-3",

        // Cover art
        div {
          class: "w-12 h-12 flex-shrink-0 bg-beet-panel border border-white/5 rounded flex items-center justify-center overflow-hidden cursor-pointer",
          onclick: move |_| props.on_album_click.call(()),
          if let Some(url) = cover_url.filter(|_| !cover_error()) {
            img {
              src: "{url}",
              alt: "Cover",
              class: "w-full h-full object-cover",
              onerror: move |_| cover_error.set(true),
            }
          } else {
            svg {
              class: "w-5 h-5 text-white/20",
              fill: "none",
              stroke: "currentColor",
              stroke_width: "1.5",
              view_box: "0 0 24 24",
              path {
                stroke_linecap: "round",
                stroke_linejoin: "round",
                d: "M9 9l10.5-3m0 6.553v3.75a2.25 2.25 0 01-1.632 2.163l-1.32.377a1.803 1.803 0 11-.99-3.467l2.31-.66a2.25 2.25 0 001.632-2.163zm0 0V2.25L9 5.25v10.303m0 0v3.75a2.25 2.25 0 01-1.632 2.163l-1.32.377a1.803 1.803 0 01-.99-3.467l2.31-.66A2.25 2.25 0 009 15.553z",
              }
            }
          }
        }

        // Track info
        div { class: "flex-grow min-w-0",
          p { class: "text-sm font-bold text-white truncate group-hover:text-beet-accent transition-colors",
            "{track.title}"
          }
          p { class: "text-xs text-gray-400 font-mono truncate", "{track.artist}" }
          if let Some(album_title) = &track.album_title {
            p {
              class: "text-xs text-gray-500 truncate cursor-pointer hover:text-beet-leaf transition-colors",
              onclick: move |_| props.on_album_click.call(()),
              "{album_title}"
            }
          }
        }

        // Duration
        if let Some(duration) = &track.duration {
          p { class: "text-xs font-mono text-gray-600 whitespace-nowrap shrink-0",
            "{duration}"
          }
        }

        // Action buttons
        div { class: "flex items-center gap-1 shrink-0",
          // Search sources button
          button {
            class: "p-2 rounded-full hover:bg-white/10 transition-colors cursor-pointer group/src",
            title: "Search sources",
            onclick: move |evt: MouseEvent| {
              evt.stop_propagation();
              props.on_search_sources.call(());
            },
            svg {
              class: "w-4 h-4 text-gray-500 group-hover/src:text-white transition-colors",
              fill: "none",
              stroke: "currentColor",
              stroke_width: "2",
              view_box: "0 0 24 24",
              path {
                stroke_linecap: "round",
                stroke_linejoin: "round",
                d: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
              }
            }
          }

          // Auto-download button
          DownloadIcon {
            item_id: props.track.id.clone(),
            state: props.download_state.clone(),
            folders: props.folders.clone(),
            selected_folder_id: props.selected_folder_id.clone(),
            active_menu: props.active_menu,
            on_download: props.on_download,
            on_override_download: props.on_override_download,
          }
        }
      }
    }
}
