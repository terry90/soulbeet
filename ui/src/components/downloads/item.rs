use dioxus::prelude::*;
use shared::download::{DownloadProgress, DownloadState};

#[component]
pub fn DownloadItem(file: DownloadProgress) -> Element {
    let state = &file.state;

    let (status_text, border_class, badge_class, badge_text) = match state {
        DownloadState::Queued => (
            "Queued",
            "border-white/5 opacity-60",
            "border border-gray-600 text-gray-400",
            "QUEUED",
        ),
        DownloadState::InProgress => (
            "Downloading",
            "border-beet-accent/50",
            "bg-blue-500/20 text-blue-300",
            "SLSK",
        ),
        DownloadState::Completed => (
            "Completed",
            "border-beet-leaf/50",
            "bg-beet-leaf/20 text-beet-leaf",
            "DOWNLOADED",
        ),
        DownloadState::Importing => (
            "Importing...",
            "border-beet-leaf/50",
            "bg-beet-leaf/20 text-beet-leaf",
            "BEETS",
        ),
        DownloadState::Imported => (
            "Imported",
            "border-green-500/50",
            "bg-green-500/20 text-green-300",
            "LIB",
        ),
        DownloadState::ImportSkipped => (
            "Import Skipped",
            "border-yellow-500/50",
            "bg-yellow-500/20 text-yellow-300",
            "SKIP",
        ),
        DownloadState::Failed(_) => (
            "Failed",
            "border-red-500/50",
            "bg-red-500/20 text-red-300",
            "ERR",
        ),
        DownloadState::Cancelled => (
            "Cancelled",
            "border-orange-500/50",
            "bg-orange-500/20 text-orange-300",
            "CANCEL",
        ),
    };

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

    let error_msg = file.error.clone();

    rsx! {
      div { class: "bg-white/5 border {border_class} p-4 rounded-lg hover:border-beet-accent/50 transition-colors group",
        div { class: "flex justify-between items-start mb-2",
          div {
            class: "text-sm font-bold text-white truncate w-3/4 pr-2",
            title: "{file.item}",
            "{display_name}"
          }
          span {
            class: "text-[10px] font-mono {badge_class} px-1.5 py-0.5 rounded uppercase cursor-help",
            title: "{status_text}",
            "{badge_text}"
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
        } else if matches!(state, DownloadState::Failed(_)) {
          if let Some(err) = &error_msg {
            div { class: "text-xs text-red-400 mt-1 break-words", "{err}" }
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
