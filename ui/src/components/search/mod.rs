pub mod album;
pub mod context;
pub mod track;

pub use context::SearchReset;

use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use shared::download::DownloadQuery;
use shared::musicbrainz::{AlbumWithTracks, SearchResult};
use shared::slskd::{
    AlbumResult as SlskdAlbumResult, SearchState, TrackResult as SlskdTrackResult,
};
use shared::system::SystemHealth;

use track::TrackResult;

use crate::search::album::AlbumResult;
use crate::{use_auth, Album, AlbumHeader, Button, Modal, SystemStatus};

mod download_results;
use download_results::DownloadResults;

mod search_type_toggle;
use search_type_toggle::{SearchType, SearchTypeToggle};

#[component]
pub fn Search() -> Element {
    let auth = use_auth();
    let mut response = use_signal::<Option<Vec<SearchResult>>>(|| None);
    let mut search = use_signal(String::new);
    let mut artist = use_signal::<Option<String>>(|| None);
    let mut search_type = use_signal(|| SearchType::Album);
    let mut loading = use_signal(|| false);
    let mut viewing_album = use_signal::<Option<AlbumWithTracks>>(|| None);
    let mut download_options = use_signal::<Option<Vec<SlskdAlbumResult>>>(|| None);
    let search_reset = try_use_context::<SearchReset>();

    let mut system_status = use_signal(SystemHealth::default);

    use_future(move || async move {
        loop {
            if let Ok(health) = auth.call(api::get_system_health()).await {
                system_status.set(health);
            }
            gloo_timers::future::TimeoutFuture::new(10000).await;
        }
    });

    use_effect(move || {
        if let Some(reset) = search_reset {
            if reset.0() > 0 {
                response.set(None);
                search.set(String::new());
                artist.set(None);
                search_type.set(SearchType::Album);
                viewing_album.set(None);
                download_options.set(None);
            }
        }
    });

    if !auth.is_logged_in() {
        info!("User not logged in");
        return rsx! {};
    }

    let download = move |query: DownloadQuery| async move {
        loading.set(true);
        viewing_album.set(None);
        download_options.set(Some(vec![]));

        let search_id = match auth.call(api::start_download_search(query)).await {
            Ok(id) => id,
            Err(e) => {
                warn!("Failed to start download search: {:?}", e);
                loading.set(false);
                return;
            }
        };

        loop {
            match auth
                .call(api::poll_download_search(search_id.clone()))
                .await
            {
                Ok(response) => {
                    download_options.with_mut(|current| {
                        if let Some(list) = current {
                            for new_album in response.results {
                                if let Some(pos) = list.iter().position(|x| {
                                    x.username == new_album.username
                                        && x.album_path == new_album.album_path
                                }) {
                                    // Safeguard against incomplete albums
                                    list[pos] = new_album;
                                } else {
                                    list.push(new_album);
                                }
                            }

                            // Resort new results by score
                            list.sort_by(|a, b| {
                                b.score
                                    .partial_cmp(&a.score)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        }
                    });

                    if response.state != SearchState::InProgress {
                        break;
                    }
                }
                Err(e) => {
                    info!("Failed to poll search: {:?}", e);
                    break;
                }
            }
        }
        loading.set(false);
    };

    let download_tracks = move |(tracks, folder): (Vec<SlskdTrackResult>, String)| async move {
        loading.set(true);
        download_options.set(None);
        if let Ok(_res) = auth.call(api::download(tracks, folder)).await {
            info!("Downloads started");
        }
        loading.set(false);
    };

    let perform_search = move || async move {
        loading.set(true);

        let query_data = api::SearchQuery {
            artist: artist(),
            query: search(),
        };

        let result = match search_type() {
            SearchType::Album => auth.call(api::search_album(query_data)).await,
            SearchType::Track => auth.call(api::search_track(query_data)).await,
        };

        if let Ok(data) = result {
            response.set(Some(data));
        }
        loading.set(false);
    };

    let view_full_album = move |album_id: String| async move {
        loading.set(true);

        if let Ok(album_data) = auth.call(api::find_album(album_id.clone())).await {
            viewing_album.set(Some(album_data));
        } else {
            info!("Failed to fetch album details for {}", album_id);
        }
        loading.set(false);
    };

    rsx! {
      if let Some(data) = viewing_album.read().clone() {
        Modal {
          on_close: move |_| viewing_album.set(None),
          header: rsx! {
            AlbumHeader { album: data.album.clone() }
          },
          Album {
            data,
            on_select: move |data: DownloadQuery| {
                spawn(download(data));
            },
          }
        }
      }

      // bg decorations
      div { class: "fixed top-1/4 -left-10 w-64 h-64 bg-beet-accent/10 rounded-full blur-[150px] pointer-events-none" }
      div { class: "fixed bottom-1/4 -right-10 w-64 h-64 bg-beet-leaf/10 rounded-full blur-[150px] pointer-events-none" }

      div { class: "w-full max-w-3xl space-y-8 z-10 mx-auto flex flex-col items-center mt-20",

        // Title Area
        div { class: "text-center space-y-2",
          h2 { class: "text-4xl md:text-6xl font-bold tracking-tight",
            span { class: "text-white", "Harvest" }
            " "
            span { class: "text-beet-leaf font-light italic", "Music." }
          }
          p { class: "text-gray-400 font-mono text-sm",
            "Search & Download // Manage Your Library"
          }
        }

        // Search bar
        div { class: "w-full relative group",

          div { class: "absolute -inset-1 bg-gradient-to-r from-beet-accent to-beet-leaf rounded-lg blur opacity-25 group-hover:opacity-50 transition duration-1000 group-hover:duration-200" }
          div { class: "relative flex items-center bg-beet-dark border border-white/10 rounded-lg p-2 shadow-2xl",
            div { class: "pl-4 pr-2 text-gray-500",
              svg {
                class: "w-6 h-6",
                fill: "none",
                stroke: "currentColor",
                view_box: "0 0 24 24",
                path {
                  stroke_linecap: "round",
                  stroke_linejoin: "round",
                  stroke_width: "2",
                  d: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
                }
              }
            }
            input {
              "type": "text",
              value: "{search}",
              class: "w-2/3 bg-transparent border-none focus:ring-0 text-white text-sm placeholder-gray-600 font-mono h-10 focus:outline-none",
              placeholder: "Search artist, album or track...",
              oninput: move |event| search.set(event.value()),
              onkeydown: move |event| {
                  if event.key() == Key::Enter {
                      spawn(perform_search());
                  }
              },
            }

            div { class: "h-8 w-px bg-white/10 mx-2" }

            input {
              "type": "text",
              value: "{artist.read().clone().unwrap_or_default()}",
              class: "w-1/3 bg-transparent border-none focus:ring-0 text-gray-400 text-sm placeholder-gray-700 font-mono h-10 focus:outline-none",
              placeholder: "Artist (opt)",
              oninput: move |event| {
                  let val = event.value();
                  if val.is_empty() { artist.set(None) } else { artist.set(Some(val)) }
              },
              onkeydown: move |event| {
                  if event.key() == Key::Enter {
                      spawn(perform_search());
                  }
              },
            }

            SearchTypeToggle { search_type }

            Button {
              class: "rounded ml-2 whitespace-nowrap",
              disabled: loading() || search.read().is_empty(),
              onclick: move |_| {
                  spawn(perform_search());
              },
              "SEARCH"
            }
          }
        }

        SystemStatus { health: system_status.read().clone() }

        // Results
        if let Some(results) = download_options.read().clone() {
          DownloadResults {
            results,
            is_searching: loading(),
            on_download: move |data| {
                spawn(download_tracks(data));
            },
          }
        } else if loading() {
          div { class: "flex flex-col justify-center items-center py-10",
            div { class: "animate-spin rounded-full h-16 w-16 border-t-4 border-b-4 border-beet-accent" }
          }
        } else {
          match *response.read() {
              Some(ref items) if !items.is_empty() => rsx! {
                div { class: "w-full bg-beet-panel/50 border border-white/5 rounded-xl p-6 backdrop-blur-sm mt-8",
                  h5 { class: "text-xl font-display font-bold mb-4 border-b border-white/10 pb-2 text-white",
                    "Search Results"
                  }
                  ul { class: "list-none p-0 space-y-4",
                    for item in items.iter() {
                      match item {
                          SearchResult::Track(ref track) => rsx! {
                            li { key: "{track.id}",
                              TrackResult {
                                on_album_click: move |id| {
                                    spawn(view_full_album(id));
                                },
                                track: track.clone(),
                              }
                            }
                          },
                          SearchResult::Album(album) => rsx! {
                            li { key: "{album.id}",
                              AlbumResult {
                                on_click: move |id| {
                                    spawn(view_full_album(id));
                                },
                                album: album.clone(),
                              }
                            }
                          },
                      }
                    }
                  }
                }
              },
              Some(_) => rsx! {
                div { class: "text-center text-gray-500 py-10 font-mono", "No signals found in the ether." }
              },
              None => rsx! {},
          }
        }
      }
    }
}
