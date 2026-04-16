use api::models::folder::Folder;
use dioxus::prelude::*;
use shared::metadata::{Album, AlbumGroup, compare_musicbrainz_dates};

use super::download_icon::{DownloadIcon, DownloadRowState};
use crate::CoverArt;
use crate::search::edition::EditionResult;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub album_group: AlbumGroup,
    pub on_edition_click: Callback<String>,
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
pub fn AlbumGroupResult(props: Props) -> Element {
    let mut expanded = use_signal(|| false);
    let mut editions = props.album_group.editions.clone();


    // Construct a dummy Album for CoverArt
    let cover_album = Album {
        id: props.album_group.id.clone(),
        title: props.album_group.title.clone(),
        artist: props.album_group.artist.clone(),
        release_date: props.album_group.release_date.clone(),
        mbid: props.album_group.mbid.clone(),
        cover_url: props.album_group.cover_url.clone(),
    };

    // If there's only one edition (or none), just render it as a normal EditionResult
    if editions.len() <= 1 {
        let album = editions.pop().unwrap_or_else(|| cover_album);
        let album_id = album.id.clone();
        return rsx! {
            EditionResult {
                album,
                on_click: move |_| props.on_edition_click.call(album_id.clone())
            }
        };
    }
    
    // Sort editions by date
    editions.sort_by(|a, b| compare_musicbrainz_dates(&a.release_date, &b.release_date));

    // SAFETY: We know there's at least 2 editions here, so unwrap is fine
    let oldest_edition = editions.first().unwrap();
    let oldest_id = oldest_edition.id.clone();
    
    rsx! {
      div {
        class: "flex flex-col bg-white/5 border border-white/5 rounded-lg overflow-hidden transition-all duration-200 hover:border-beet-accent/30",
        
        // Main Header, looks basically like AlbumResult
        div {
            class: "p-4 flex items-center gap-4 group transition-colors",

            div {
              class: "flex-grow flex items-center gap-4 cursor-pointer",
              onclick: move |_| {
                  props.on_edition_click.call(oldest_id.clone());
              },
              CoverArt { album: cover_album }

              div { class: "flex-grow flex flex-col justify-center",
                  div { class: "flex items-center gap-2",
                    h5 { class: "text-lg font-bold text-white group-hover:text-beet-accent transition-colors",
                        "{props.album_group.title}"
                    }
                  }
                  p { class: "text-md text-gray-400 font-mono", "{props.album_group.artist}" }
                  if let Some(release_date) = props.album_group.release_date.as_ref().filter(|s| !s.is_empty()) {
                      p { class: "text-sm text-gray-500 mt-1 font-mono", "{release_date}" }
                  }
              }
            }
            
            // Action buttons
            div { class: "flex items-center gap-1 shrink-0",
                // Show edition count if not expanded
                if !expanded() && !editions.is_empty() {
                    div { class: "text-xs font-mono text-gray-500 bg-white/5 px-2 py-1 rounded mr-2",
                        "{editions.len()} editions"
                    }
                }

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
                    item_id: props.album_group.id.clone(),
                    state: props.download_state.clone(),
                    folders: props.folders.clone(),
                    selected_folder_id: props.selected_folder_id.clone(),
                    active_menu: props.active_menu,
                    on_download: props.on_download,
                    on_override_download: props.on_override_download,
                }
            }
        }

        // Bar with arrow at bottom of the list card item to toggle editions
        if !editions.is_empty() {
            div {
                onclick: move |evt| {
                    evt.stop_propagation();
                    expanded.toggle();
                },
                class: "h-8 flex justify-center items-center hover:bg-beet-accent/10 cursor-pointer border-t border-white/5 transition-all duration-300 group",
                svg {
                    class: "w-6 h-6 text-gray-500 group-hover:text-beet-accent transform transition-transform duration-300",
                    fill: "none",
                    stroke: "currentColor",
                    view_box: "0 0 24 24",
                    if expanded() {
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M22 14l-10-6-10 6",
                        }
                    } else {
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M22 10l-10 6-10-6",
                        }
                    }
                }
            }
        }

        // Expanded editions
        if expanded() {
            div {
                class: "p-4 pt-2 space-y-3",
                div { class: "text-[10px] text-gray-500 font-mono uppercase tracking-widest mb-2 flex items-center gap-2",
                  span { "Available Editions" }
                }
                for edition in editions {
                    EditionResult {
                        key: "{edition.id.clone()}",
                        album: edition.clone(),
                        on_click: move |_| props.on_edition_click.call(edition.id.clone()),
                    }
                }
            }
        }
      }
    }
}
