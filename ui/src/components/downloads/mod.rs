use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use shared::download::{DownloadProgress, DownloadState, ImportChoice, KeptImport, MatchChoices};

mod item;
mod picker;
use api::CancelDownloadRequest;
use item::DownloadItem;

use crate::friendly_error;

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

    let mut fail =
        move |item: &str, error: &ServerFnError, from_picker: bool| match downloads_signal
            .write()
            .get_mut(item)
        {
            Some(entry) => show_failure(entry, error, from_picker),
            None => Failure::OnRow,
        };
    let import_again = use_callback(
        move |(item, choice): (String, ImportChoice)| -> Pin<Box<dyn Future<Output = Result<(), Failure>>>> {
            Box::pin(async move {
                let from_picker = sent_by_picker(&choice);
                api::retry_import(item.clone(), choice)
                    .await
                    .map_err(|e| fail(&item, &e, from_picker))
            })
        },
    );
    let load_choices = use_callback(
        move |item: String| -> Pin<Box<dyn Future<Output = Result<MatchChoices, Failure>>>> {
            Box::pin(async move {
                api::import_choices(item.clone())
                    .await
                    .map_err(|e| fail(&item, &e, true))
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
                on_import: import_again,
                on_load_choices: load_choices,
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

/// Where a failed request from a row shows why.
#[derive(Debug, Clone, PartialEq)]
enum Failure {
    /// On the row's reason line.
    OnRow,
    /// In the picker that sent it, which stays.
    InPicker(String),
}

/// Picks and match loads come from the picker; Retry and as-is from the row.
fn sent_by_picker(choice: &ImportChoice) -> bool {
    matches!(
        choice,
        ImportChoice::Candidate { .. } | ImportChoice::MusicBrainz(_)
    )
}

/// What a refusal says the server still keeps for the row. None when the
/// request was not refused, or the refusal does not say.
fn kept_after(error: &ServerFnError) -> Option<KeptImport> {
    match error {
        ServerFnError::ServerError {
            code: api::REFUSAL_CODE,
            details: Some(details),
            ..
        } => serde_json::from_value(details.clone()).ok(),
        _ => None,
    }
}

/// Takes away what the server says it no longer keeps for the row, and
/// puts why on the reason line, unless the picker sent the request and
/// stays, which then shows it so the skip reason stays readable.
fn show_failure(row: &mut DownloadProgress, error: &ServerFnError, from_picker: bool) -> Failure {
    match kept_after(error) {
        Some(KeptImport::Gone) => {
            row.retryable = false;
            row.can_choose_import = false;
        }
        Some(KeptImport::WithoutMatches) => row.can_choose_import = false,
        Some(KeptImport::WithMatches) | None => {}
    }
    let message = friendly_error(error);
    if from_picker && row.retryable && row.can_choose_import {
        return Failure::InPicker(message);
    }
    row.error = Some(message);
    Failure::OnRow
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
    use dioxus::fullstack::RequestError;

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

    fn kept_skip() -> DownloadProgress {
        DownloadProgress {
            error: Some("No close match (2 candidates)".into()),
            retryable: true,
            can_choose_import: true,
            ..row("a", DownloadState::ImportSkipped).1
        }
    }

    /// A refusal as the client gets it: built by the server's helper and
    /// sent through JSON.
    fn refused(message: &str, kept: KeptImport) -> ServerFnError {
        let sent = serde_json::to_value(api::refusal_error_with(message, kept)).unwrap();
        serde_json::from_value(sent).unwrap()
    }

    fn every_choice() -> [ImportChoice; 4] {
        [
            ImportChoice::Auto,
            ImportChoice::AsIs,
            ImportChoice::Candidate {
                data_source: "MusicBrainz".into(),
                id: "r1".into(),
            },
            ImportChoice::MusicBrainz("https://musicbrainz.org/release/x".into()),
        ]
    }

    fn after(choice: &ImportChoice, error: &ServerFnError) -> (DownloadProgress, Failure) {
        let mut row = kept_skip();
        let failure = show_failure(&mut row, error, sent_by_picker(choice));
        (row, failure)
    }

    #[test]
    fn the_server_says_what_a_refusal_keeps() {
        let kept = KeptImport::WithoutMatches;
        assert_eq!(kept_after(&refused("x", kept)), Some(kept));
        let plain = api::refusal_error("Folder not found");
        assert_eq!(kept_after(&plain), None);
    }

    #[test]
    fn a_refusal_that_dropped_the_job_takes_every_action_away() {
        for message in ["Nothing to retry", "The kept files are gone"] {
            for choice in every_choice() {
                let (row, failure) = after(&choice, &refused(message, KeptImport::Gone));
                assert_eq!(failure, Failure::OnRow, "{choice:?}");
                assert_eq!(row.error.as_deref(), Some(message));
                assert!(!row.retryable && !row.can_choose_import, "{choice:?}");
            }
        }
    }

    #[test]
    fn a_refusal_without_matches_keeps_only_retry() {
        let message = "This import has no matches to pick from";
        for choice in every_choice() {
            let (row, failure) = after(&choice, &refused(message, KeptImport::WithoutMatches));
            assert_eq!(failure, Failure::OnRow, "{choice:?}");
            assert_eq!(row.error.as_deref(), Some(message));
            assert!(row.retryable && !row.can_choose_import, "{choice:?}");
        }
        // A match load refused the same way takes the picker too.
        let mut row = kept_skip();
        show_failure(
            &mut row,
            &refused(message, KeptImport::WithoutMatches),
            true,
        );
        assert!(row.retryable && !row.can_choose_import);
    }

    #[test]
    fn a_refused_pick_keeps_every_action_and_shows_why_in_the_picker() {
        let text = "Track imports need a MusicBrainz recording, not a release";
        for choice in every_choice().into_iter().filter(sent_by_picker) {
            let (row, failure) = after(&choice, &refused(text, KeptImport::WithMatches));
            assert_eq!(failure, Failure::InPicker(text.into()), "{choice:?}");
            assert_eq!(row.error.as_deref(), Some("No close match (2 candidates)"));
            assert!(row.retryable && row.can_choose_import);
        }
    }

    #[test]
    fn an_unreachable_server_changes_no_action() {
        let error = ServerFnError::Request(RequestError::Request("connection refused".into()));
        let text = "Could not reach the server. Check your connection.";
        let (row, failure) = after(&ImportChoice::Auto, &error);
        assert_eq!(failure, Failure::OnRow);
        assert_eq!(row.error.as_deref(), Some(text));
        assert!(row.retryable && row.can_choose_import);

        // A failed pick or match load keeps the skip reason.
        let mut row = kept_skip();
        assert_eq!(
            show_failure(&mut row, &error, true),
            Failure::InPicker(text.into())
        );
        assert_eq!(row, kept_skip());
    }
}
