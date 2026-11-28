use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use shared::download::{
    DownloadProgress, DownloadState, ImportChoice, ImportRefusal, KeptImport, MatchChoices,
};

mod diagnostics;
mod item;
mod picker;
use api::CancelDownloadRequest;
pub use diagnostics::JobDiagnosticsPanel;
use item::DownloadItem;

use crate::{friendly_error, IconDownload, IconX};

#[derive(Props, Clone, PartialEq)]
pub struct DownloadsProps {
    pub is_open: Signal<bool>,
    pub downloads: Signal<HashMap<String, DownloadProgress>>,
    /// Where focus goes back to once the drawer closes: the toggle that
    /// opened it.
    #[props(into)]
    pub return_focus_to: ReadSignal<Option<Rc<MountedData>>>,
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

    let imported_count = active_downloads
        .iter()
        .filter(|f| matches!(f.state, DownloadState::Imported))
        .count();

    let finished_count = active_downloads.len().saturating_sub(processing_count);

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
    let cancel_download = use_callback(
        move |file: DownloadProgress| -> Pin<Box<dyn Future<Output = Result<(), String>>>> {
            let req = CancelDownloadRequest {
                id: file.id.clone(),
                source: file.source.clone(),
                backend: file.backend.clone(),
            };
            Box::pin(async move {
                api::cancel_download(req)
                    .await
                    .map_err(|e| friendly_error(&e))?;
                if let Some(entry) = downloads_signal.write().get_mut(&file.item) {
                    entry.state = DownloadState::Cancelled;
                }
                Ok(())
            })
        },
    );

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

    // Keys and screen readers start in the drawer once it opens, and go
    // back to the toggle when it closes: focus left inside an inert drawer
    // falls to the top of the page.
    let mut close_button = use_signal(|| None::<Rc<MountedData>>);
    use_effect(move || {
        if (props.is_open)() {
            focus(close_button.peek().clone());
        }
    });
    let close_modal = move |_| {
        props.is_open.set(false);
        focus(props.return_focus_to.peek().clone());
    };

    let open = (*props.is_open)();
    // Closed, it turns hidden once the fade ends, so browsers neither paint
    // nor composite it, and inert right away, so clicks, keys and screen
    // readers pass it by while it fades. Opening shows it at once, so its
    // close button can take focus.
    let (modal_opacity, panel_translate, visibility) = if open {
        ("opacity-100", "translate-x-0", "visible transition-opacity")
    } else {
        (
            "opacity-0",
            "translate-x-full",
            "invisible transition-[opacity,visibility]",
        )
    };

    rsx! {
      div {
        class: "fixed inset-0 z-50 flex justify-end duration-200 ease-out {modal_opacity} {visibility}",
        "inert": (!open).then_some(true),
        // Backdrop
        div {
          class: "absolute inset-0 bg-beet-sunken/75 backdrop-blur-xs cursor-pointer",
          onclick: close_modal,
        }

        // Panel
        div {
          "data-testid": "downloads-drawer-panel",
          class: "relative w-full max-w-md bg-beet-surface-low border-l border-beet-border-subtle h-full transform transition-transform duration-200 ease-out flex flex-col {panel_translate}",
          // Header
          div { class: "px-5 py-4 border-b border-beet-border-subtle flex justify-between items-start gap-3 shrink-0",
            div { class: "min-w-0",
              h3 { class: "text-[17px] font-semibold tracking-tight text-beet-text-primary leading-snug",
                "Active Transfers"
              }
              div { class: "flex flex-wrap items-center gap-1.5 mt-2",
                span {
                  class: if processing_count > 0 {
                      "inline-flex items-center gap-1.5 px-2 py-0.5 rounded-full tabular-nums-mono text-[11px] font-medium bg-beet-leaf-surface text-beet-leaf-glow border border-beet-leaf-solid/25"
                  } else {
                      "inline-flex items-center gap-1.5 px-2 py-0.5 rounded-full tabular-nums-mono text-[11px] font-medium bg-beet-surface text-beet-text-tertiary border border-beet-border-subtle"
                  },
                  if processing_count > 0 {
                    span { class: "w-1.5 h-1.5 rounded-full bg-beet-leaf-glow shrink-0" }
                  }
                  "{processing_count} ACTIVE"
                }
                if errored_count > 0 {
                  span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full tabular-nums-mono text-[11px] font-medium bg-beet-crimson-surface text-beet-crimson-text border border-beet-crimson-text/25",
                    "{errored_count} FAILED"
                  }
                }
                if imported_count > 0 {
                  span { class: "inline-flex items-center gap-1 px-2 py-0.5 rounded-full tabular-nums-mono text-[11px] font-medium bg-beet-surface text-beet-text-secondary border border-beet-border-subtle",
                    "{imported_count} IMPORTED"
                  }
                }
              }
            }
            button {
              r#type: "button",
              class: "w-8 h-8 rounded-[10px] inline-flex items-center justify-center text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high transition-colors cursor-pointer shrink-0",
              aria_label: "Close downloads",
              onmounted: move |e| close_button.set(Some(e.data())),
              onclick: close_modal,
              IconX { size: 16 }
            }
          }

          // Content
          div { class: "flex-1 overflow-y-auto studio-scrollbar px-4 py-4 space-y-3",
            if active_downloads.is_empty() {
              div { class: "flex flex-col items-center justify-center text-center py-14 px-6 rounded-[12px] bg-beet-surface border border-beet-border-subtle",
                div { class: "w-11 h-11 rounded-full bg-beet-surface-high border border-beet-border-subtle flex items-center justify-center text-beet-text-tertiary mb-3",
                  IconDownload { size: 18 }
                }
                p { class: "text-[13px] font-medium text-beet-text-secondary",
                  "No active transfers in the queue."
                }
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
          div { class: "px-4 py-3 border-t border-beet-border-subtle shrink-0",
            button {
              r#type: "button",
              disabled: finished_count == 0,
              class: "w-full h-9 px-3 rounded-[10px] bg-beet-surface border border-beet-border-subtle text-beet-text-secondary text-[12px] font-medium transition-colors cursor-pointer enabled:hover:bg-beet-surface-high enabled:hover:border-beet-border-default enabled:hover:text-beet-text-primary disabled:opacity-40 disabled:cursor-not-allowed",
              onclick: clear_finished,
              "Clear completed"
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

/// Moves keyboard focus to `element`, once it is mounted.
fn focus(element: Option<Rc<MountedData>>) {
    if let Some(element) = element {
        spawn(async move {
            let _ = element.set_focus(true).await;
        });
    }
}

/// Picks, tagged as-is, and match loads come from the picker; Retry and bare as-is from the row.
fn sent_by_picker(choice: &ImportChoice) -> bool {
    matches!(
        choice,
        ImportChoice::Candidate { .. }
            | ImportChoice::MusicBrainz(_)
            | ImportChoice::AsIsWithTags(_)
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
        } => serde_json::from_value::<ImportRefusal>(details.clone())
            .ok()
            .map(|refusal| refusal.kept),
        _ => None,
    }
}

/// Takes away what the server says it no longer keeps for the row, and
/// puts why on the reason line, unless the picker sent the request and
/// stays, which then shows it so the skip reason stays readable.
fn show_failure(row: &mut DownloadProgress, error: &ServerFnError, from_picker: bool) -> Failure {
    match kept_after(error) {
        Some(KeptImport::Gone) => row.retryable = false,
        Some(KeptImport::Kept) | None => {}
    }
    let message = friendly_error(error);
    if from_picker && row.retryable {
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
    use shared::failure::{FailureCode, RetryAdvice, Stage};

    fn row(item: &str, state: DownloadState) -> (String, DownloadProgress) {
        let p =
            DownloadProgress::queued(item.into(), "peer".into(), item.into(), 1).with_state(state);
        (item.to_string(), p)
    }

    /// The drawer's outer element as the server renders it.
    fn drawer(open: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            |open: bool| {
                let is_open = use_signal(|| open);
                let downloads = use_signal(HashMap::new);
                let return_focus_to = use_signal(|| None);
                rsx! { Downloads { is_open, downloads, return_focus_to } }
            },
            open,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        html[..html.find('>').unwrap()].to_string()
    }

    #[test]
    fn a_closed_drawer_is_hidden_and_inert_once_its_fade_ends() {
        let closed = drawer(false);
        assert!(closed.contains(" inert=true"), "{closed}");
        assert!(closed.contains(" invisible"), "{closed}");
        assert!(
            closed.contains("transition-[opacity,visibility]"),
            "{closed}"
        );
        let open = drawer(true);
        assert!(!open.contains("inert"), "{open}");
        assert!(!open.contains("invisible"), "{open}");
        // Shown at once, so its close button can take focus.
        assert!(open.contains(" transition-opacity"), "{open}");
        assert!(!open.contains("visibility"), "{open}");
    }

    thread_local! {
        static TOGGLE_FOCUSED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    /// The navbar toggle, counting the times it is asked to take focus.
    struct Toggle;

    impl RenderedElementBacking for Toggle {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn set_focus(
            &self,
            focus: bool,
        ) -> Pin<Box<dyn Future<Output = dioxus::html::MountedResult<()>>>> {
            if focus {
                TOGGLE_FOCUSED.with(|f| f.set(f.get() + 1));
            }
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn closing_from_inside_gives_focus_back_to_the_toggle() {
        let mut dom = VirtualDom::new(|| {
            let is_open = use_signal(|| true);
            let downloads = use_signal(HashMap::new);
            let return_focus_to = use_signal(|| Some(Rc::new(MountedData::new(Toggle))));
            rsx! { Downloads { is_open, downloads, return_focus_to } }
        });
        // The backdrop, the close button, then the footer's clear button.
        let clicks = crate::listeners(&mut dom, "click");
        assert_eq!(clicks.len(), 3);
        for (closer, focused) in [(clicks[1], 1), (clicks[0], 2)] {
            crate::click(&mut dom, closer);
            assert_eq!(TOGGLE_FOCUSED.with(|f| f.get()), focused);
            let html = dioxus_ssr::render(&dom);
            assert!(html.contains(" inert=true"), "{html}");
        }
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
            ..row("a", DownloadState::ImportSkipped).1
        }
    }

    /// A refusal as the client gets it: built by the server's helper and
    /// sent through JSON.
    fn refused(message: &str, kept: KeptImport) -> ServerFnError {
        let refusal = ImportRefusal {
            failure: shared::failure::Failure::new(
                FailureCode::ImportChoiceInvalid,
                Stage::Input,
                RetryAdvice::NeedsDecision,
                message,
            ),
            kept,
        };
        let sent = serde_json::to_value(api::import_refusal_error(refusal)).unwrap();
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
        let kept = KeptImport::Kept;
        assert_eq!(kept_after(&refused("x", kept)), Some(kept));
        let plain = api::failure_error(shared::failure::Failure::new(
            FailureCode::UnknownHandle,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Folder not found",
        ));
        assert_eq!(kept_after(&plain), None);
        let ServerFnError::ServerError {
            message, details, ..
        } = refused("Nothing to retry", KeptImport::Gone)
        else {
            panic!("not a server error")
        };
        let back: ImportRefusal = serde_json::from_value(details.unwrap()).unwrap();
        assert_eq!(
            (message.as_str(), back.failure.message.as_str(), back.kept),
            ("Nothing to retry", "Nothing to retry", KeptImport::Gone)
        );
    }

    #[test]
    fn a_refusal_that_dropped_the_job_takes_every_action_away() {
        for message in ["Nothing to retry", "The kept files are gone"] {
            for choice in every_choice() {
                let (row, failure) = after(&choice, &refused(message, KeptImport::Gone));
                assert_eq!(failure, Failure::OnRow, "{choice:?}");
                assert_eq!(row.error.as_deref(), Some(message));
                assert!(!row.retryable, "{choice:?}");
            }
        }
    }

    #[test]
    fn a_refusal_of_a_kept_import_without_matches_shows_in_the_picker_that_sent_it() {
        let message = "This import has no matches to pick from";
        for choice in every_choice() {
            let (row, failure) = after(&choice, &refused(message, KeptImport::Kept));
            let expected = match sent_by_picker(&choice) {
                true => Failure::InPicker(message.into()),
                false => Failure::OnRow,
            };
            assert_eq!(failure, expected, "{choice:?}");
            assert!(row.retryable, "{choice:?}");
        }
    }

    #[test]
    fn a_refused_pick_keeps_every_action_and_shows_why_in_the_picker() {
        let text = "Track imports need a MusicBrainz recording, not a release";
        for choice in every_choice().into_iter().filter(sent_by_picker) {
            let (row, failure) = after(&choice, &refused(text, KeptImport::Kept));
            assert_eq!(failure, Failure::InPicker(text.into()), "{choice:?}");
            assert_eq!(row.error.as_deref(), Some("No close match (2 candidates)"));
            assert!(row.retryable);
        }
    }

    #[test]
    fn an_unreachable_server_changes_no_action() {
        let error = ServerFnError::Request(RequestError::Request("connection refused".into()));
        let text = "Could not reach the server. Check your connection.";
        let (row, failure) = after(&ImportChoice::Auto, &error);
        assert_eq!(failure, Failure::OnRow);
        assert_eq!(row.error.as_deref(), Some(text));
        assert!(row.retryable);

        // A failed pick or match load keeps the skip reason.
        let mut row = kept_skip();
        assert_eq!(
            show_failure(&mut row, &error, true),
            Failure::InPicker(text.into())
        );
        assert_eq!(row, kept_skip());
    }
}
