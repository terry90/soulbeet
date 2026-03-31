use dioxus::prelude::*;
use shared::metadata::{Album, AlbumGroup};

use crate::CoverArt;
use super::album::AlbumResult;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub album_group: AlbumGroup,
    pub on_edition_click: Callback<String>,
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

    // If there's only one edition (or none), just render it as a normal AlbumResult
    if editions.len() <= 1 {
        let album = editions.pop().unwrap_or_else(|| cover_album);
        let album_id = album.id.clone();
        return rsx! {
            AlbumResult {
                album,
                on_click: move |_| props.on_edition_click.call(album_id.clone())
            }
        };
    }
    
    // Sort editions by date
    editions.sort_by(|a, b| {
        let date_a = a.release_date.as_deref().filter(|s| !s.is_empty()).unwrap_or("9999-12-31");
        let date_b = b.release_date.as_deref().filter(|s| !s.is_empty()).unwrap_or("9999-12-31");
        date_a.cmp(date_b)
    });

    let oldest_edition = editions.first();
    let oldest_id = oldest_edition
        .map(|e| e.id.clone())
        .unwrap_or_else(|| props.album_group.id.clone());
    
    rsx! {
      div {
        class: "flex flex-col bg-white/5 border border-white/5 rounded-lg overflow-hidden transition-all duration-200 hover:border-beet-accent/30",
        
        // Main Header
        div {
            onclick: move |_| {
                props.on_edition_click.call(oldest_id.clone());
            },
            class: "p-4 flex items-center gap-4 cursor-pointer group transition-colors",

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
            
            // Show edition count if not expanded
            if !expanded() && !editions.is_empty() {
                div { class: "text-xs font-mono text-gray-500 bg-white/5 px-2 py-1 rounded",
                    "{editions.len()} editions"
                }
            }
        }

        // The "Flat Arrow" bar at the bottom of the element
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
                    AlbumResult {
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
