use std::collections::HashMap;

use dioxus::prelude::*;
use shared::download::{DownloadProgress, DownloadState};

mod item;
use item::DownloadItem;

#[derive(Props, Clone, PartialEq)]
pub struct DownloadsProps {
    pub is_open: Signal<bool>,
    pub downloads: Signal<HashMap<String, DownloadProgress>>,
}

#[component]
pub fn Downloads(mut props: DownloadsProps) -> Element {
    let mut active_downloads: Vec<DownloadProgress> = props.downloads.read().values().cloned().collect();
    active_downloads.sort_by(|a, b| {
        a.state
            .partial_cmp(&b.state)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Count specific states for the header summary
    let processing_count = active_downloads
        .iter()
        .filter(|f| {
            matches!(
                f.state,
                DownloadState::Queued
                    | DownloadState::InProgress
                    | DownloadState::Importing
                    | DownloadState::Completed // Still needs to be imported
            )
        })
        .count();

    let errored_count = active_downloads
        .iter()
        .filter(|f| {
            matches!(
                f.state,
                DownloadState::Failed(_)
                    | DownloadState::Cancelled
            )
        })
        .count();

    let clear_finished = move |_| {
        let mut map = props.downloads.write();
        map.retain(|_, file| {
            matches!(
                file.state,
                DownloadState::Queued
                    | DownloadState::InProgress
                    | DownloadState::Importing
                    | DownloadState::Completed // Downloads that are completed but not yet imported
            )
        });
    };

    let close_modal = move |_| props.is_open.set(false);

    let (modal_opacity, panel_translate, pointer_events) = if (*props.is_open)() {
        ("opacity-100", "translate-x-0", "pointer-events-auto")
    } else {
        ("opacity-0", "translate-x-full", "pointer-events-none")
    };

    rsx! {
      div { class: "fixed inset-0 z-50 flex justify-end transition-opacity duration-200 ease-out {modal_opacity} {pointer_events}",
        // Backdrop
        div {
          class: "absolute inset-0 bg-black/60 backdrop-blur-sm cursor-pointer",
          onclick: close_modal,
        }

        // Panel
        div { class: "relative w-full max-w-md bg-beet-panel border-l border-white/10 h-full shadow-2xl transform transition-transform duration-200 ease-out flex flex-col {panel_translate}",
          //Header
          div { class: "p-6 border-b border-white/10 flex justify-between items-center bg-black/20",
            div {
              h3 { class: "text-xl font-bold text-white font-display",
                "Active Transfers"
              }
              p { class: "text-xs text-beet-leaf font-mono mt-1",
                ":: {processing_count} PROCESSING // {errored_count} ERRORED"
              }
            }
            button {
              class: "text-gray-400 hover:text-white transition-colors cursor-pointer",
              onclick: close_modal,
              svg {
                class: "w-6 h-6",
                fill: "none",
                stroke: "currentColor",
                view_box: "0 0 24 24",
                path {
                  stroke_linecap: "round",
                  stroke_linejoin: "round",
                  stroke_width: "2",
                  d: "M6 18L18 6M6 6l12 12",
                }
              }
            }
          }

          // Content
          div { class: "flex-1 overflow-y-auto p-6 no-scrollbar space-y-4",
            if active_downloads.is_empty() {
              div { class: "text-center text-gray-500 py-10 font-mono text-sm",
                "No active transfers in the queue."
              }
            }

            for file in active_downloads.iter() {
              DownloadItem { file: file.clone() }
            }
          }
          // Footer
          div { class: "p-4 border-t border-white/10 bg-black/20",
            button {
              class: "w-full py-2 text-xs font-mono uppercase tracking-widest text-center border border-white/10 hover:bg-white/5 text-gray-400 hover:text-white transition-colors cursor-pointer hover:border-red-500/30",
              onclick: clear_finished,
              "CLEAR COMPLETED"
            }
          }
        }
      }
    }
}
