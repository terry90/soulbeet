use std::collections::HashMap;

use dioxus::prelude::*;
use shared::slskd::{DownloadState, FileEntry};

#[derive(Props, Clone, PartialEq)]
pub struct DownloadsProps {
    pub is_open: Signal<bool>,
    pub downloads: Signal<HashMap<String, FileEntry>>,
}

#[component]
pub fn Downloads(mut props: DownloadsProps) -> Element {
    let mut active_downloads: Vec<FileEntry> = props.downloads.read().values().cloned().collect();
    active_downloads.sort_by(|a, b| b.enqueued_at.cmp(&a.enqueued_at));

    if !(*props.is_open)() {
        return rsx! {};
    }

    let clear_finished = move |_| {
        let mut map = props.downloads.write();
        map.retain(|_, file| {
            let state = file
                .state
                .first()
                .cloned()
                .unwrap_or(DownloadState::Unknown("Unknown".into()));

            matches!(
                state,
                DownloadState::Queued
                    | DownloadState::InProgress
                    | DownloadState::Importing
                    | DownloadState::Completed // Downloads that are completed but not yet imported
            )
        });
    };

    rsx! {
      div { class: "fixed top-20 right-6 z-50 flex flex-col items-end gap-2",
        div { class: "bg-gray-800 border border-gray-700 rounded-lg shadow-xl w-96 max-h-[32rem] overflow-hidden flex flex-col mb-2",
          div { class: "p-4 border-b border-gray-700 bg-gray-800/50 backdrop-blur sticky top-0 flex justify-between items-center",
            h3 { class: "font-semibold text-white", "Downloads" }
            div { class: "flex items-center gap-3",
              span { class: "text-xs text-gray-400 bg-gray-700 px-2 py-1 rounded-full",
                "{active_downloads.len()} items"
              }
              button {
                class: "text-xs hover:text-red-200 hover:bg-red-500/20 px-2 py-1 rounded transition-colors font-bold tracking-wider",
                onclick: clear_finished,
                "Clear finished"
              }
            }
          }
          div { class: "overflow-y-auto p-2 space-y-2 flex-1",
            if active_downloads.is_empty() {
              div { class: "text-center text-gray-500 py-8", "No active downloads" }
            }
            for file in active_downloads.iter() {
              DownloadItem { file: file.clone() }
            }
          }
        }
      }
    }
}

#[component]
fn DownloadItem(file: FileEntry) -> Element {
    let state = file
        .state
        .first()
        .cloned()
        .unwrap_or(DownloadState::Unknown("Unknown".into()));

    let status_color = match state {
        DownloadState::Queued => "text-yellow-400",
        DownloadState::InProgress => "text-blue-400",
        DownloadState::Completed | DownloadState::Imported => "text-green-400",
        DownloadState::Aborted
        | DownloadState::Cancelled
        | DownloadState::Errored
        | DownloadState::ImportFailed => "text-red-400",
        DownloadState::Importing => "text-purple-400",
        DownloadState::ImportSkipped => "text-orange-400",
        _ => "text-gray-400",
    };

    let status_text = match &state {
        DownloadState::Queued => "Queued".to_string(),
        DownloadState::InProgress => "Downloading".to_string(),
        DownloadState::Completed => "Completed".to_string(),
        DownloadState::Aborted => "Aborted".to_string(),
        DownloadState::Cancelled => "Cancelled".to_string(),
        DownloadState::Errored => "Errored".to_string(),
        DownloadState::Importing => "Importing".to_string(),
        DownloadState::Imported => "Imported".to_string(),
        DownloadState::ImportSkipped => "Import Skipped".to_string(),
        DownloadState::ImportFailed => "Import Failed".to_string(),
        DownloadState::Unknown(s) => s.clone(),
    };
    let percent = file.percent_complete as i32;

    // Clean up filename for display (remove path)
    let filename_str = file.filename.replace('\\', "/");
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
      div { class: "bg-gray-700/50 rounded-md p-3 text-sm border border-gray-600 hover:border-gray-500 transition-colors",
        div { class: "flex justify-between items-start gap-2 mb-2",
          div { class: "flex-1 min-w-0",
            div {
              class: "font-medium text-gray-200 truncate",
              title: "{file.filename}",
              "{display_name}"
            }
            div { class: "text-xs text-gray-400 mt-0.5 flex items-center gap-2",
              span { "{file.username}" }
              span { class: "w-1 h-1 bg-gray-500 rounded-full" }
              span { "{format_size(file.size)}" }
            }
          }
          span {
            class: "text-xs font-medium px-2 py-0.5 rounded bg-gray-800 {status_color}",
            title: "{file.state_description}",
            "{status_text}"
          }
        }
        if matches!(state, DownloadState::Errored) {
          div { class: "text-xs text-red-400 mt-1 break-words", "{file.state_description}" }
        }
        if matches!(state, DownloadState::InProgress) {
          div { class: "w-full bg-gray-800 rounded-full h-1.5 mt-2 overflow-hidden",
            div {
              class: "bg-teal-500 h-full rounded-full transition-all duration-500",
              style: "width: {percent}%",
            }
          }
          div { class: "flex justify-between text-xs text-gray-400 mt-1",
            span { "{percent}%" }
            if let Some(speed) = calculate_speed(&file) {
              span { "{speed}" }
            }
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

fn calculate_speed(file: &FileEntry) -> Option<String> {
    if file.average_speed > 0.0 {
        Some(format!("{}/s", format_size(file.average_speed as u64)))
    } else {
        None
    }
}
