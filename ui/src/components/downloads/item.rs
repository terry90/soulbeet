use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;
use shared::download::{DownloadProgress, DownloadState, ImportChoice};

use super::picker::{ChoicesLoader, MatchPicker};
use super::Failure;
use crate::{Disclosure, RowAction};

/// How a row's state is shown: badge text and tooltip, plus row and badge classes.
#[derive(Debug, PartialEq)]
struct Badge {
    title: String,
    border: &'static str,
    class: &'static str,
    text: &'static str,
}

fn badge(state: &DownloadState) -> Badge {
    let (title, border, class, text) = match state {
        DownloadState::Queued => (
            "Queued".to_string(),
            "border-white/5 opacity-60",
            "border border-gray-600 text-gray-400",
            "QUEUED",
        ),
        DownloadState::InProgress => (
            "Downloading".to_string(),
            "border-beet-accent/50",
            "bg-blue-500/20 text-blue-300",
            "SLSK",
        ),
        DownloadState::Completed => (
            "Completed".to_string(),
            "border-beet-leaf/50",
            "bg-beet-leaf/20 text-beet-leaf",
            "DOWNLOADED",
        ),
        DownloadState::ImportQueued(n) => (
            waiting_text(*n),
            "border-beet-leaf/30",
            "border border-beet-leaf/40 text-beet-leaf",
            "WAIT",
        ),
        DownloadState::Importing => (
            "Importing...".to_string(),
            "border-beet-leaf/50",
            "bg-beet-leaf/20 text-beet-leaf",
            "BEETS",
        ),
        DownloadState::Imported => (
            "Imported".to_string(),
            "border-green-500/50",
            "bg-green-500/20 text-green-300",
            "LIB",
        ),
        DownloadState::ImportSkipped => (
            "Import Skipped".to_string(),
            "border-yellow-500/50",
            "bg-yellow-500/20 text-yellow-300",
            "SKIP",
        ),
        DownloadState::Failed(_) => (
            "Failed".to_string(),
            "border-red-500/50",
            "bg-red-500/20 text-red-300",
            "ERR",
        ),
        DownloadState::Cancelled => (
            "Cancelled".to_string(),
            "border-orange-500/50",
            "bg-orange-500/20 text-orange-300",
            "CANCEL",
        ),
    };
    Badge {
        title,
        border,
        class,
        text,
    }
}

/// The row's note and its text class: why it was skipped or failed, or what to
/// know about an import that went through (untagged, warnings).
fn reason(row: &DownloadProgress) -> Option<(&'static str, &str)> {
    let class = match row.state {
        DownloadState::ImportSkipped | DownloadState::Imported => "text-yellow-400",
        DownloadState::Failed(_) => "text-red-400",
        _ => return None,
    };
    row.error.as_deref().map(|note| (class, note))
}

/// Imports the row's item again the way the choice says. Resolves to Ok once
/// the server accepted it.
pub type ImportHandler =
    Callback<(String, ImportChoice), Pin<Box<dyn Future<Output = Result<(), Failure>>>>>;

fn waiting_text(ahead: u32) -> String {
    format!("Waiting for library ({ahead} ahead)")
}

/// Only rows whose import job the server kept are `retryable`, so a failed
/// download or a row with nothing left to import never offers a retry.
fn shows_retry(row: &DownloadProgress) -> bool {
    matches!(
        row.state,
        DownloadState::ImportSkipped | DownloadState::Failed(_)
    ) && row.retryable
}

/// A kept row whose last automatic import recorded matches can also import
/// as-is or with a picked match.
fn shows_choices(row: &DownloadProgress) -> bool {
    shows_retry(row) && row.can_choose_import
}

#[component]
pub fn DownloadItem(
    file: DownloadProgress,
    on_cancel: EventHandler<DownloadProgress>,
    on_import: ImportHandler,
    on_load_choices: ChoicesLoader,
) -> Element {
    // Set while an import request is in flight, and kept after an accepted one
    // until the row changes state, so a second click cannot race the first.
    let mut busy = use_signal(|| false);
    // Why the picker's last load or pick failed, shown in the picker.
    let mut picker_notice = use_signal(|| None::<String>);
    use_effect(use_reactive(&file.state, move |_| {
        busy.set(false);
        picker_notice.set(None);
    }));
    let item = file.item.clone();
    let import = use_callback(move |choice: ImportChoice| {
        if busy() {
            return;
        }
        busy.set(true);
        picker_notice.set(None);
        let item = item.clone();
        spawn(async move {
            if let Err(failure) = on_import.call((item, choice)).await {
                busy.set(false);
                if let Failure::InPicker(message) = failure {
                    picker_notice.set(Some(message));
                }
            }
        });
    });

    let state = &file.state;
    let note = reason(&file);
    let is_cancellable = matches!(state, DownloadState::Queued | DownloadState::InProgress);
    let Badge {
        title: status_text,
        border: border_class,
        class: badge_class,
        text: badge_text,
    } = badge(state);

    let percent = file.percent as i32;

    // Clean up filename for display (remove path)
    let filename_str = file.item.replace('\\', "/");
    let path = std::path::Path::new(&filename_str);
    let components: Vec<_> = path.components().collect();

    let display_name = match components.len() {
        0 => "Unknown".to_string(),
        _ => components[components.len() - 1]
            .as_os_str()
            .to_string_lossy()
            .into_owned(),
    };

    rsx! {
      div { class: "bg-white/5 border {border_class} p-4 rounded-lg hover:border-beet-accent/50 transition-colors group",
        div { class: "flex justify-between items-start mb-2",
          div {
            class: "text-sm font-bold text-white truncate w-3/4 pr-2",
            title: "{file.item}",
            "{display_name}"
          }
          div { class: "flex items-center gap-1.5 shrink-0",
            span {
              class: "text-[10px] font-mono {badge_class} px-1.5 py-0.5 rounded uppercase cursor-help",
              title: "{status_text}",
              "{badge_text}"
            }
            if is_cancellable {
              { let file_clone = file.clone();
              rsx! {
                button {
                  class: "text-[10px] font-mono px-1.5 py-0.5 rounded uppercase border border-transparent text-gray-600 opacity-0 group-hover:opacity-100 hover:border-red-500/40 hover:text-red-400 transition-all cursor-pointer",
                  title: "Cancel download",
                  onclick: move |evt: Event<MouseData>| {
                      evt.stop_propagation();
                      on_cancel.call(file_clone.clone());
                  },
                  "\u{00d7}"
                }
              }}
            }
          }
        }
        div { class: "flex justify-between text-xs text-gray-400 font-mono mb-1",
          span {
            if matches!(state, DownloadState::InProgress) && file.speed > 0.0 {
              "{format_size(file.speed as u64)}/s"
            } else {
              "{format_size(file.size)}"
            }
          }
          span { "{percent}%" }
        }
        // Progress Bar
        if matches!(state, DownloadState::InProgress | DownloadState::Importing) {
          div { class: "h-2 w-full bg-gray-800 rounded-full overflow-hidden relative",
            div {
              class: "h-full bg-beet-accent absolute top-0 left-0 transition-all duration-300",
              style: "width: {percent}%",
            }
            // Striped animation overlay (using inline SVG for pattern)
            div {
              class: "h-full w-full absolute top-0 left-0 opacity-30",
              style: "background-image: repeating-linear-gradient(45deg, transparent, transparent 5px, rgba(255,255,255,0.5) 5px, rgba(255,255,255,0.5) 10px);",
            }
          }
        } else if matches!(state, DownloadState::Queued) {
          div { class: "h-1 w-full bg-gray-800 rounded-full mt-2" }
        }
        if let DownloadState::ImportQueued(n) = state {
          div { class: "text-xs text-gray-300 font-mono mt-2", "{waiting_text(*n)}" }
        }
        // The only element holding the reason: e2e matches it by text.
        if let Some((note_class, note)) = note {
          div { class: "text-xs {note_class} mt-1 break-words", "{note}" }
        }
        if let (true, Some(kept)) = (file.retryable, &file.local_path) {
          div { class: "text-xs text-gray-500 font-mono mt-1 break-all", "Kept in {kept}" }
        }
        if let (Some(_), Some(tail)) = (note, &file.import_output) {
          Disclosure { class: "mt-2", label: "beets output",
            pre { class: "mt-1 max-h-48 overflow-y-auto text-xs font-mono text-gray-400 whitespace-pre-wrap break-all", "{tail}" }
          }
        }
        if shows_retry(&file) {
          div { class: "mt-2 flex flex-wrap gap-2",
            RowAction { busy: busy(), onclick: move |_| import.call(ImportChoice::Auto), "Retry import" }
            if shows_choices(&file) {
              RowAction { busy: busy(), onclick: move |_| import.call(ImportChoice::AsIs), "Import as-is" }
            }
          }
        }
        if shows_choices(&file) {
          MatchPicker {
            item: file.item.clone(),
            busy: busy(),
            on_load: on_load_choices,
            on_choose: import,
            notice: picker_notice,
          }
        }
        if matches!(state, DownloadState::Importing) {
          div { class: "flex items-center gap-2 text-xs text-gray-300 font-mono mt-2",
            svg {
              class: "w-3 h-3 animate-spin",
              fill: "none",
              view_box: "0 0 24 24",
              circle {
                class: "opacity-25",
                cx: "12",
                cy: "12",
                r: "10",
                stroke: "currentColor",
                stroke_width: "4",
              }
              path {
                class: "opacity-75",
                fill: "currentColor",
                d: "M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z",
              }
            }
            "Moving and tagging..."
          }
        }
      }
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: DownloadState, kept: Option<&str>) -> DownloadProgress {
        DownloadProgress {
            retryable: kept.is_some(),
            ..DownloadProgress::queued("1".into(), "peer".into(), "a\\b\\01.flac".into(), 10)
                .with_state(state)
                .with_import_details(kept.map(str::to_string), None)
        }
    }

    #[test]
    fn wait_badge_and_text() {
        let b = badge(&DownloadState::ImportQueued(2));
        assert_eq!(b.text, "WAIT");
        assert_eq!(b.title, "Waiting for library (2 ahead)");
    }

    #[test]
    fn reason_covers_failures_and_notable_imports() {
        let with_error = |state, error: Option<&str>| DownloadProgress {
            error: error.map(str::to_string),
            ..row(state, None)
        };
        let untagged = with_error(DownloadState::Imported, Some("Imported as-is (untagged)"));
        assert_eq!(
            reason(&untagged),
            Some(("text-yellow-400", "Imported as-is (untagged)"))
        );
        assert_eq!(reason(&with_error(DownloadState::Imported, None)), None);
        let skipped = with_error(DownloadState::ImportSkipped, Some("No close match"));
        assert_eq!(
            reason(&skipped),
            Some(("text-yellow-400", "No close match"))
        );
        let failed = with_error(
            DownloadState::Failed("x".into()),
            Some("beets exited with code 1"),
        );
        assert_eq!(
            reason(&failed),
            Some(("text-red-400", "beets exited with code 1"))
        );
        assert_eq!(
            reason(&with_error(DownloadState::InProgress, Some("stale"))),
            None
        );
    }

    #[test]
    fn retry_shows_only_for_kept_import_failures() {
        assert!(shows_retry(&row(
            DownloadState::ImportSkipped,
            Some("/dl/X")
        )));
        assert!(shows_retry(&row(
            DownloadState::Failed("beets exited with code 1".into()),
            Some("/dl/X")
        )));
        assert!(!shows_retry(&row(DownloadState::Failed("x".into()), None)));
        assert!(!shows_retry(&row(DownloadState::Imported, Some("/dl/X"))));
        let nothing_kept = DownloadProgress {
            retryable: false,
            ..row(DownloadState::ImportSkipped, Some("/dl/X"))
        };
        assert!(!shows_retry(&nothing_kept));
    }

    #[test]
    fn choices_show_only_on_kept_rows_with_matches() {
        let with_choices = |state| DownloadProgress {
            can_choose_import: true,
            ..row(state, Some("/dl/X"))
        };
        assert!(shows_choices(&with_choices(DownloadState::ImportSkipped)));
        // A failed pick keeps the matches.
        assert!(shows_choices(&with_choices(DownloadState::Failed(
            "MusicBrainz has no recording x".into()
        ))));
        assert!(!shows_choices(&with_choices(DownloadState::Importing)));
        assert!(!shows_choices(&row(
            DownloadState::ImportSkipped,
            Some("/dl/X")
        )));
        let nothing_kept = DownloadProgress {
            retryable: false,
            ..with_choices(DownloadState::ImportSkipped)
        };
        assert!(!shows_choices(&nothing_kept));
    }
}
