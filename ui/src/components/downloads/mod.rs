use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use shared::download::{DownloadProgress, DownloadState};

mod item;
use api::CancelDownloadRequest;
use item::DownloadItem;

use crate::{friendly_error, is_refusal};

#[derive(Props, Clone, PartialEq)]
pub struct DownloadsProps {
    pub is_open: Signal<bool>,
    pub downloads: Signal<HashMap<String, DownloadProgress>>,
}

#[component]
pub fn Downloads(mut props: DownloadsProps) -> Element {
    let active_downloads = sorted_rows(&props.downloads.read());

    // Count specific states for the header summary
    let processing_count = active_downloads
        .iter()
        .filter(|f| {
            matches!(
                f.state,
                DownloadState::Queued
                    | DownloadState::InProgress
                    | DownloadState::ImportQueued(_)
                    | DownloadState::Importing
                    | DownloadState::Completed // Still needs to be imported
            )
        })
        .count();

    let errored_count = active_downloads
        .iter()
        .filter(|f| matches!(f.state, DownloadState::Failed(_) | DownloadState::Cancelled))
        .count();

    let clear_finished = move |_| {
        let mut cleared = Vec::new();
        props.downloads.write().retain(|item, file| {
            let keep = matches!(
                file.state,
                DownloadState::Queued
                    | DownloadState::InProgress
                    | DownloadState::ImportQueued(_)
                    | DownloadState::Importing
                    | DownloadState::Completed // Downloads that are completed but not yet imported
            );
            if !keep {
                cleared.push(item.clone());
            }
            keep
        });
        // The server keeps a retry for each row that did not import; drop those too.
        if !cleared.is_empty() {
            spawn(async move {
                if let Err(e) = api::forget_import_retries(cleared).await {
                    warn!("Could not drop import retries for cleared rows: {e}");
                }
            });
        }
    };

    let mut downloads_signal = props.downloads;
    let cancel_download = move |file: DownloadProgress| {
        let req = CancelDownloadRequest {
            id: file.id.clone(),
            source: file.source.clone(),
            item: file.item.clone(),
            backend: file.backend.clone(),
        };
        let item_key = file.item.clone();
        spawn(async move {
            if api::cancel_download(req).await.is_ok() {
                let mut map = downloads_signal.write();
                if let Some(entry) = map.get_mut(&item_key) {
                    entry.state = DownloadState::Cancelled;
                }
            }
        });
    };

    let retry_import = use_callback(
        move |file: DownloadProgress| -> Pin<Box<dyn Future<Output = bool>>> {
            Box::pin(async move {
                match api::retry_import(file.item.clone()).await {
                    Ok(()) => true,
                    Err(e) => {
                        if let Some(entry) = downloads_signal.write().get_mut(&file.item) {
                            entry.error = Some(friendly_error(&e));
                            // A refusal means the server kept nothing to retry.
                            if is_refusal(&e) {
                                entry.retryable = false;
                            }
                        }
                        false
                    }
                }
            })
        },
    );

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
              aria_label: "Close downloads",
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
              DownloadItem {
                key: "{file.item}",
                file: file.clone(),
                on_cancel: cancel_download,
                on_retry: retry_import,
              }
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

/// Rows by state, then by item so rows with the same state keep their place.
fn sorted_rows(downloads: &HashMap<String, DownloadProgress>) -> Vec<DownloadProgress> {
    let mut rows: Vec<DownloadProgress> = downloads.values().cloned().collect();
    rows.sort_by(|a, b| {
        a.state
            .partial_cmp(&b.state)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.item.cmp(&b.item))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(item: &str, state: DownloadState) -> (String, DownloadProgress) {
        let p =
            DownloadProgress::queued(item.into(), "peer".into(), item.into(), 1).with_state(state);
        (item.to_string(), p)
    }

    #[test]
    fn equal_states_sort_by_item() {
        let map: HashMap<String, DownloadProgress> = [
            row("c", DownloadState::Imported),
            row("a", DownloadState::Imported),
            row("z", DownloadState::InProgress),
            row("b", DownloadState::Imported),
            row("d", DownloadState::ImportQueued(1)),
        ]
        .into_iter()
        .collect();
        let items: Vec<String> = sorted_rows(&map).into_iter().map(|r| r.item).collect();
        assert_eq!(items, ["z", "d", "a", "b", "c"]);
    }
}
