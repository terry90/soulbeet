use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;
use shared::download::{DownloadProgress, DownloadState, ImportChoice, IndexState};

use super::diagnostics::JobDiagnosticsPanel;
use super::picker::{ChoicesLoader, MatchPicker};
use super::MatchPickerFailure as Failure;
use crate::{Disclosure, IconLoader, IconX, LossyChip, RowAction};

/// How a row's state is shown: badge text and tooltip, plus row and badge classes.
#[derive(Debug, PartialEq)]
struct Badge {
    title: String,
    border: &'static str,
    class: &'static str,
    text: &'static str,
}

fn active_backend_badge(backend: Option<&str>) -> &'static str {
    match backend {
        None | Some("slskd") => "SLSK",
        Some("archive") => "ARCHIVE",
        Some("ytdlp") => "YTM",
        Some("gdstudio") => "NETEASE",
        Some("airdcpp") => "DC++",
        Some(_) => "DL",
    }
}

fn badge(state: &DownloadState, backend: Option<&str>) -> Badge {
    let (title, border, class, text) = match state {
        DownloadState::Queued => (
            "Queued".to_string(),
            "border-beet-border-subtle opacity-75",
            "bg-beet-surface-high text-beet-text-tertiary border border-beet-border-subtle",
            "QUEUED",
        ),
        DownloadState::InProgress => (
            "Downloading".to_string(),
            "border-beet-leaf-solid/30",
            "bg-beet-accent-surface text-beet-accent-text border border-beet-accent-solid/30",
            active_backend_badge(backend),
        ),
        DownloadState::Completed => (
            "Completed".to_string(),
            "border-beet-leaf-solid/30",
            "bg-beet-leaf-surface text-beet-leaf-text border border-beet-leaf-solid/25",
            "DOWNLOADED",
        ),
        DownloadState::ImportQueued(n) => (
            waiting_text(*n),
            "border-beet-border-default",
            "bg-beet-leaf-surface text-beet-leaf-text border border-beet-leaf-solid/25",
            "WAIT",
        ),
        DownloadState::Importing => (
            "Importing...".to_string(),
            "border-beet-leaf-solid/30",
            "bg-beet-leaf-surface text-beet-leaf-text border border-beet-leaf-solid/25",
            "BEETS",
        ),
        DownloadState::Imported => (
            "Imported".to_string(),
            "border-beet-leaf-solid/30",
            "bg-beet-leaf-surface text-beet-leaf-glow border border-beet-leaf-solid/25",
            "LIB",
        ),
        DownloadState::ImportSkipped => (
            "Import Skipped".to_string(),
            "border-beet-gold/30",
            "bg-beet-gold-surface text-beet-gold border border-beet-gold/30",
            "SKIP",
        ),
        DownloadState::Failed(_) => (
            "Failed".to_string(),
            "border-beet-crimson-text/35",
            "bg-beet-crimson-surface text-beet-crimson-text border border-beet-crimson-text/30",
            "ERR",
        ),
        DownloadState::Cancelled => (
            "Cancelled".to_string(),
            "border-beet-border-subtle",
            "bg-beet-surface-high text-beet-text-tertiary border border-beet-border-subtle",
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
        DownloadState::ImportSkipped | DownloadState::Imported => "text-beet-gold",
        DownloadState::Failed(_) => "text-beet-crimson-text",
        _ => return None,
    };
    row.error.as_deref().map(|note| (class, note))
}

/// The row's note line: its reason, else why the last cancel was refused.
fn note<'a>(
    row: &'a DownloadProgress,
    cancel_refusal: Option<&'a str>,
) -> Option<(&'static str, &'a str)> {
    reason(row).or_else(|| cancel_refusal.map(|refusal| ("text-beet-crimson-text", refusal)))
}

/// An imported row's line on Navidrome and its text class: whether it lists the file yet, or why
/// not.
fn index_line(row: &DownloadProgress) -> Option<(&'static str, &str)> {
    if row.state != DownloadState::Imported {
        return None;
    }
    Some(match row.index.as_ref()? {
        IndexState::Waiting => ("text-beet-text-secondary", "Waiting for Navidrome"),
        IndexState::Listed => ("text-beet-leaf-text", "In Navidrome"),
        IndexState::Failed(failure) => ("text-beet-gold", failure.message.as_str()),
    })
}

/// Cancels the row's transfer. Resolves to why the server refused, if it did.
pub type CancelHandler =
    Callback<DownloadProgress, Pin<Box<dyn Future<Output = Result<(), String>>>>>;

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

#[component]
pub fn DownloadItem(
    file: DownloadProgress,
    on_cancel: CancelHandler,
    on_import: ImportHandler,
    on_load_choices: ChoicesLoader,
) -> Element {
    // Set while an import request is in flight, and kept after an accepted one
    // until the row changes state, so a second click cannot race the first.
    let mut busy = use_signal(|| false);
    // Why the picker's last load or pick failed, shown in the picker.
    let mut picker_notice = use_signal(|| None::<String>);
    // Why the last cancel was refused, on the note line.
    let mut cancel_refusal = use_signal(|| None::<String>);
    use_effect(use_reactive(&file.state, move |_| {
        busy.set(false);
        picker_notice.set(None);
        cancel_refusal.set(None);
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
    let refusal = cancel_refusal();
    let note = note(&file, refusal.as_deref());
    let is_cancellable = matches!(state, DownloadState::Queued | DownloadState::InProgress);
    let backend_meta = file.backend.as_deref().and_then(shared::backends::meta);
    let Badge {
        title: status_text,
        border: border_class,
        class: badge_class,
        text: badge_text,
    } = badge(state, file.backend.as_deref());

    let percent = file.percent as i32;

    // Clean up filename for display (extract file name and optional parent folder)
    let filename_str = file.item.replace('\\', "/");
    let path = std::path::Path::new(&filename_str);
    let normal_parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();

    let display_name = normal_parts
        .last()
        .cloned()
        .unwrap_or_else(|| "Unknown".to_string());
    let parent_folder = if normal_parts.len() >= 2 {
        Some(normal_parts[normal_parts.len() - 2].clone())
    } else {
        None
    };

    rsx! {
      div { class: "bg-beet-surface border {border_class} rounded-[12px] p-3.5 transition-colors group hover:border-beet-border-default",
        div { class: "flex justify-between items-start gap-2 mb-2",
          div { class: "min-w-0 flex-1",
            div {
              class: "text-[13px] font-medium text-beet-text-primary truncate leading-snug",
              title: "{file.item}",
              "{display_name}"
            }
            if let Some(folder) = parent_folder {
              div { class: "text-[11px] text-beet-text-tertiary truncate mt-0.5",
                "{folder}"
              }
            }
          }
          div { class: "flex items-center gap-1.5 shrink-0",
            if let Some(meta) = backend_meta {
              LossyChip { meta }
            }
            span {
              class: "text-[10px] tabular-nums-mono font-semibold {badge_class} px-1.5 py-0.5 rounded-full uppercase cursor-help",
              title: "{status_text}",
              "{badge_text}"
            }
            if is_cancellable {
              { let file_clone = file.clone();
              rsx! {
                button {
                  r#type: "button",
                  class: "w-6 h-6 rounded-[6px] inline-flex items-center justify-center text-beet-text-tertiary opacity-0 group-hover:opacity-100 focus-visible:opacity-100 hover:bg-beet-crimson-surface hover:text-beet-crimson-text transition-all cursor-pointer",
                  title: "Cancel download",
                  aria_label: "Cancel download",
                  onclick: move |evt: Event<MouseData>| {
                      evt.stop_propagation();
                      let file = file_clone.clone();
                      spawn(async move {
                          cancel_refusal.set(on_cancel.call(file).await.err());
                      });
                  },
                  IconX { size: 13 }
                }
              }}
            }
          }
        }
        div { class: "flex items-center justify-between gap-2 tabular-nums-mono text-[11px] text-beet-text-tertiary mb-1.5",
          div { class: "flex items-center gap-1.5 min-w-0",
            span { class: "shrink-0",
              if matches!(state, DownloadState::InProgress) && file.speed > 0.0 {
                "{format_size(file.speed as u64)}/s"
              } else {
                "{format_size(file.size)}"
              }
            }
            if let Some(meta) = backend_meta {
              span { class: "px-1.5 py-0.5 rounded bg-beet-surface-high border border-beet-border-subtle text-[10px] text-beet-text-secondary truncate",
                "{meta.name}"
              }
            }
          }
          span { class: "shrink-0", "{percent}%" }
        }
        // Progress Bar
        if matches!(state, DownloadState::InProgress | DownloadState::Importing) {
          {
            let fill_class = if matches!(state, DownloadState::Importing) {
                "bg-beet-leaf-solid"
            } else {
                "bg-beet-accent-solid"
            };
            rsx! {
              div { class: "h-1 w-full bg-beet-surface-high rounded-full overflow-hidden",
                div {
                  class: "h-full {fill_class} rounded-full transition-all duration-300",
                  style: "width: {percent}%",
                }
              }
            }
          }
        } else if matches!(state, DownloadState::Queued) {
          div { class: "h-1 w-full bg-beet-surface-high rounded-full overflow-hidden" }
        }
        if let DownloadState::ImportQueued(n) = state {
          div { class: "text-[11px] text-beet-text-secondary tabular-nums-mono mt-2", "{waiting_text(*n)}" }
        }
        // The only element holding the reason: e2e matches it by text.
        if let Some((note_class, note)) = note {
          div { class: "text-[12px] {note_class} mt-1.5 break-words leading-snug", "{note}" }
        }
        if let Some((index_class, index)) = index_line(&file) {
          div { class: "text-[11px] {index_class} tabular-nums-mono mt-1.5 break-words", "{index}" }
        }
        if let (true, Some(kept)) = (file.retryable, &file.local_path) {
          div { class: "text-[11px] text-beet-text-tertiary tabular-nums-mono mt-1.5 break-all", "Kept in {kept}" }
        }
        if let (Some(_), Some(tail)) = (note, &file.import_output) {
          Disclosure { class: "mt-2", label: "beets output",
            pre { class: "mt-1.5 p-2.5 rounded-[8px] bg-beet-sunken border border-beet-border-subtle max-h-48 overflow-y-auto studio-scrollbar text-[11px] tabular-nums-mono text-beet-text-secondary whitespace-pre-wrap break-all", "{tail}" }
          }
        }
        if shows_retry(&file) {
          div { class: "mt-2.5 flex flex-wrap gap-2",
            RowAction { busy: busy(), onclick: move |_| import.call(ImportChoice::Auto), "Retry import" }
            RowAction { busy: busy(), onclick: move |_| import.call(ImportChoice::AsIs), "Import as-is" }
          }
        }
        if shows_retry(&file) {
          MatchPicker {
            item: file.item.clone(),
            busy: busy(),
            on_load: on_load_choices,
            on_choose: import,
            notice: picker_notice,
          }
        }
        if matches!(state, DownloadState::Importing) {
          div { class: "flex items-center gap-2 text-[11px] text-beet-leaf-text tabular-nums-mono mt-2",
            IconLoader { size: 12 }
            span { "Moving and tagging..." }
          }
        }
        if let Some(ref job_id) = file.job_id {
          JobDiagnosticsPanel {
            key: "{job_id}",
            job_id: job_id.clone(),
            failed: matches!(state, DownloadState::Failed(_) | DownloadState::ImportSkipped),
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
    use shared::download::MatchChoices;

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
        let b = badge(&DownloadState::ImportQueued(2), None);
        assert_eq!(b.text, "WAIT");
        assert_eq!(b.title, "Waiting for library (2 ahead)");
    }

    #[test]
    fn in_progress_badge_reflects_backend() {
        assert_eq!(badge(&DownloadState::InProgress, None).text, "SLSK");
        assert_eq!(
            badge(&DownloadState::InProgress, Some("slskd")).text,
            "SLSK"
        );
        assert_eq!(
            badge(&DownloadState::InProgress, Some("archive")).text,
            "ARCHIVE"
        );
        assert_eq!(badge(&DownloadState::InProgress, Some("ytdlp")).text, "YTM");
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
            Some(("text-beet-gold", "Imported as-is (untagged)"))
        );
        assert_eq!(reason(&with_error(DownloadState::Imported, None)), None);
        let skipped = with_error(DownloadState::ImportSkipped, Some("No close match"));
        assert_eq!(
            reason(&skipped),
            Some(("text-beet-gold", "No close match"))
        );
        let failed = with_error(
            DownloadState::Failed("x".into()),
            Some("beets exited with code 1"),
        );
        assert_eq!(
            reason(&failed),
            Some(("text-beet-crimson-text", "beets exited with code 1"))
        );
        assert_eq!(
            reason(&with_error(DownloadState::InProgress, Some("stale"))),
            None
        );
    }

    #[test]
    fn a_refused_cancel_shows_on_the_note_line() {
        let running = row(DownloadState::InProgress, None);
        assert_eq!(
            note(&running, Some("Download not found")),
            Some(("text-beet-crimson-text", "Download not found"))
        );
        assert_eq!(note(&running, None), None);
        let failed = DownloadProgress {
            error: Some("beets exited with code 1".into()),
            ..row(DownloadState::Failed("x".into()), None)
        };
        assert_eq!(
            note(&failed, Some("Download not found")),
            Some(("text-beet-crimson-text", "beets exited with code 1"))
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

    fn render(file: DownloadProgress) -> String {
        let mut dom = VirtualDom::new_with_props(
            |file: DownloadProgress| {
                let on_cancel: CancelHandler = Callback::new(|_: DownloadProgress| {
                    Box::pin(async { Ok(()) }) as Pin<Box<dyn Future<Output = Result<(), String>>>>
                });
                let on_import: ImportHandler = Callback::new(|_: (String, ImportChoice)| {
                    Box::pin(async { Ok(()) }) as Pin<Box<dyn Future<Output = Result<(), Failure>>>>
                });
                let on_load_choices: ChoicesLoader = Callback::new(|_: String| {
                    Box::pin(async { Err(Failure::OnRow) })
                        as Pin<Box<dyn Future<Output = Result<MatchChoices, Failure>>>>
                });
                rsx! { DownloadItem { file, on_cancel, on_import, on_load_choices } }
            },
            file,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn an_imported_row_says_whether_navidrome_lists_it() {
        let failure = shared::failure::Failure::new(
            shared::failure::FailureCode::IndexNotFoundAfterScan,
            shared::failure::Stage::Index,
            shared::failure::RetryAdvice::NotRetryable,
            "Navidrome scanned the library after this import and does not list the file",
        );
        for (index, text) in [
            (IndexState::Waiting, "Waiting for Navidrome"),
            (IndexState::Listed, "In Navidrome"),
            (
                IndexState::Failed(failure.clone()),
                failure.message.as_str(),
            ),
        ] {
            let html = render(DownloadProgress {
                index: Some(index),
                ..row(DownloadState::Imported, None)
            });
            assert!(html.contains(text), "{html}");
        }
        let html = render(row(DownloadState::Imported, None));
        assert!(!html.contains("Navidrome"), "{html}");
    }

    #[test]
    fn a_row_of_slskd_carries_no_lossy_chip() {
        let html = render(row(DownloadState::InProgress, None).with_backend("slskd"));
        assert!(!html.contains("LOSSY"), "{html}");
    }

    #[test]
    fn a_ytdlp_row_shows_lossy_youtube_music_through_its_import() {
        for state in [DownloadState::InProgress, DownloadState::Imported] {
            let html = render(row(state, None).with_backend("ytdlp"));
            assert!(html.contains("LOSSY · YOUTUBE MUSIC"), "{html}");
        }
    }

    #[test]
    fn a_kept_row_offers_every_import_action() {
        for state in [
            DownloadState::ImportSkipped,
            DownloadState::Failed("beets exited with code 1".into()),
        ] {
            let html = render(row(state.clone(), Some("/dl/X")));
            assert!(html.contains("Retry import"), "{html}");
            assert!(html.contains("Import as-is"), "{html}");
            assert!(html.contains("Show matches"), "{html}");

            let gone = DownloadProgress {
                retryable: false,
                ..row(state, Some("/dl/X"))
            };
            let html = render(gone);
            assert!(!html.contains("Import as-is"), "{html}");
            assert!(!html.contains("Show matches"), "{html}");
        }
    }

    #[test]
    fn diagnostics_toggle_appears_only_when_job_id_is_present() {
        let without_job = row(DownloadState::Failed("peer offline".into()), None);
        let html = render(without_job);
        assert!(!html.contains("Why?"), "{html}");
        assert!(!html.contains("Details"), "{html}");

        let failed_with_job = DownloadProgress {
            job_id: Some("job-123".into()),
            ..row(DownloadState::Failed("peer offline".into()), None)
        };
        let html = render(failed_with_job);
        assert!(html.contains("Why?"), "{html}");

        let active_with_job = DownloadProgress {
            job_id: Some("job-456".into()),
            ..row(DownloadState::InProgress, None)
        };
        let html = render(active_with_job);
        assert!(html.contains("Details"), "{html}");
    }
}
