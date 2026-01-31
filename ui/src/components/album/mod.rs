use dioxus::prelude::*;
use shared::{
    download::DownloadQuery,
    musicbrainz::{AlbumWithTracks, Track},
};
use std::collections::HashSet;

use crate::album::{footer::AlbumFooter, track_list::TrackList};

mod footer;
mod header;
mod track_item;
mod track_list;

pub use header::AlbumHeader;

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    /// The album and its tracks to display
    pub data: AlbumWithTracks,
    /// Callback for when the user confirms their selection
    #[props(into)]
    pub on_select: EventHandler<DownloadQuery>,
}

#[component]
pub fn Album(props: Props) -> Element {
    let mut selected_tracks = use_signal(HashSet::<String>::new);
    let tracks = use_signal(|| props.data.tracks.clone());

    let all_selected =
        selected_tracks.read().len() == tracks.read().len() && !tracks.read().is_empty();

    let handle_select_all = move |_| {
        let mut selected = selected_tracks.write();
        if all_selected {
            selected.clear();
        } else {
            for track in tracks.read().iter() {
                selected.insert(track.id.clone());
            }
        }
    };

    let handle_track_toggle = move |track_id: String| {
        let mut selected = selected_tracks.write();
        if selected.contains(&track_id) {
            selected.remove(&track_id);
        } else {
            selected.insert(track_id);
        }
    };

    rsx! {
        TrackList {
            tracks,
            selected_tracks,
            on_toggle_select_all: handle_select_all,
            on_track_toggle: handle_track_toggle,
            all_selected,
        }
        AlbumFooter {
            is_selection_empty: selected_tracks.read().is_empty(),
            on_select: move |_| {
                let selected_ids = selected_tracks.read();
                let tracks: Vec<Track> = tracks
                    .read()
                    .iter()
                    .filter(|t| selected_ids.contains(&t.id))
                    .cloned()
                    .collect();
                let album = props.data.album.clone();
                let tracks = if props.data.tracks.len() == tracks.len() {
                    props.data.tracks.clone()
                } else {
                    tracks
                };
                props.on_select.call(DownloadQuery::new(tracks).album(album));
            },
        }
    }
}
