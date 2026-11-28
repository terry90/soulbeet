use dioxus::prelude::*;
use shared::metadata::{Album, Track};
use std::collections::HashMap;

use super::download_icon::{DownloadIcon, DownloadRowState};
use super::inline_track_panel::InlineTrackPanel;
use crate::CoverArt;

/// The year the album first came out and its release types, as
/// "2003 · Album · Live"; None when it has neither.
fn album_facts(album: &Album) -> Option<String> {
    let year = (album.first_release_date.as_deref()).map(|d| d.chars().take(4).collect());
    let facts: Vec<String> = year
        .into_iter()
        .chain(album.release_types.iter().flatten().cloned())
        .collect();
    (!facts.is_empty()).then(|| facts.join(" · "))
}

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub album: Album,
    pub is_expanded: bool,
    pub on_toggle: Callback,
    pub on_search_sources: Callback,
    pub download_state: DownloadRowState,
    pub has_folder: bool,
    pub tracks: Option<Vec<Track>>,
    pub download_states: Signal<HashMap<String, DownloadRowState>>,
    #[props(into)]
    pub on_download: EventHandler<()>,
    #[props(into)]
    pub on_track_download: EventHandler<Track>,
}

#[component]
pub fn AlbumResult(props: Props) -> Element {
    let album = &props.album;

    rsx! {
        div { class: "space-y-0",
            // Album row
            div {
                class: "bg-beet-surface hover:bg-beet-surface-high border transition-colors duration-150 flex items-center gap-3 p-3 group cursor-pointer",
                class: if props.is_expanded {
                    "rounded-t-[12px] border-beet-accent/40"
                } else {
                    "rounded-[12px] border-beet-border-subtle hover:border-beet-border-default"
                },
                onclick: move |_| props.on_toggle.call(()),

                // Cover art
                div {
                    class: "shrink-0",
                    CoverArt { album: album.clone() }
                }

                // Album info
                div { class: "flex-grow min-w-0",
                    p { class: "text-[15px] font-semibold text-beet-text-primary truncate group-hover:text-beet-leaf-text transition-colors",
                        "{album.title}"
                    }
                    p { class: "text-[13px] text-beet-text-secondary truncate", "{album.artist}" }
                    if let Some(facts) = album_facts(album) {
                        p { class: "text-[12px] text-beet-text-tertiary tabular-nums-mono mt-0.5", "{facts}" }
                    }
                }

                // Chevron indicator
                svg {
                    class: "w-4 h-4 transition-transform duration-200 shrink-0",
                    class: if props.is_expanded {
                        "rotate-90 text-beet-accent-text"
                    } else {
                        "text-beet-text-tertiary group-hover:text-beet-text-primary"
                    },
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    view_box: "0 0 24 24",
                    path {
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        d: "M8.25 4.5l7.5 7.5-7.5 7.5",
                    }
                }

                // Action buttons
                div { class: "flex items-center gap-1 shrink-0",
                    // Search sources button
                    button {
                        r#type: "button",
                        class: "p-2 rounded-full hover:bg-beet-surface-highest transition-colors cursor-pointer group/src",
                        aria_label: "Search sources",
                        title: "Search sources",
                        onclick: move |evt: MouseEvent| {
                            evt.stop_propagation();
                            props.on_search_sources.call(());
                        },
                        svg {
                            class: "w-4 h-4 text-beet-text-secondary group-hover/src:text-beet-text-primary transition-colors",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "1.75",
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
                        state: props.download_state.clone(),
                        on_download: props.on_download,
                    }
                }
            }

            // Expansion panel (always in DOM per NFR-02)
            div {
                class: "grid transition-[grid-template-rows] duration-300 ease-in-out",
                class: if props.is_expanded { "grid-rows-[1fr]" } else { "grid-rows-[0fr]" },
                div {
                    class: "overflow-hidden border-x border-b rounded-b-[12px]",
                    class: if props.is_expanded { "border-beet-accent/40" } else { "border-transparent" },
                    InlineTrackPanel {
                        tracks: if props.is_expanded { props.tracks.clone() } else { Some(vec![]) },
                        download_states: props.download_states,
                        has_folder: props.has_folder,
                        on_track_download: props.on_track_download,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(first_release_date: Option<&str>, release_types: &[&str]) -> Album {
        Album {
            id: "accde5f7-04da-4c66-bb8e-7bb3d39ad326".into(),
            title: "Wonder What’s Next".into(),
            artist: "Chevelle".into(),
            release_date: None,
            mbid: None,
            cover_url: None,
            release_types: (!release_types.is_empty())
                .then(|| release_types.iter().map(|t| t.to_string()).collect()),
            first_release_date: first_release_date.map(str::to_string),
        }
    }

    #[test]
    fn an_album_row_shows_its_year_and_types() {
        let facts = |date, types| album_facts(&album(date, types));
        assert_eq!(
            facts(Some("2002-08-27"), &["Album"]).as_deref(),
            Some("2002 · Album")
        );
        assert_eq!(
            facts(Some("2003"), &["Album", "Live"]).as_deref(),
            Some("2003 · Album · Live")
        );
        assert_eq!(
            facts(Some("2011-10-14"), &["Single"]).as_deref(),
            Some("2011 · Single")
        );
        assert_eq!(
            facts(None, &["Album", "Live"]).as_deref(),
            Some("Album · Live")
        );
        assert_eq!(facts(Some("1999"), &[]).as_deref(), Some("1999"));
        assert_eq!(facts(None, &[]), None);
    }
}
