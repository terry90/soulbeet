use dioxus::prelude::*;
use shared::system::{HealthLevel, NavidromeStatus, SystemHealth};

/// The status dot class and chip text for an importer health level.
pub fn importer_chip(level: HealthLevel) -> (&'static str, &'static str) {
    let dot = match level {
        HealthLevel::Ready => "bg-beet-accent",
        HealthLevel::Degraded => "bg-yellow-500",
        HealthLevel::Broken => "bg-red-500",
        HealthLevel::Checking => "bg-gray-500 animate-pulse",
    };
    (dot, level.chip())
}

#[component]
pub fn SystemStatus(health: SystemHealth, navidrome_status: NavidromeStatus) -> Element {
    let nav_hint = match navidrome_status {
        NavidromeStatus::Connected | NavidromeStatus::MissingReportRealPath => None,
        NavidromeStatus::InvalidCredentials => Some("Your username was not found in Navidrome"),
        NavidromeStatus::Unknown => Some("Navidrome credentials not configured"),
        NavidromeStatus::Offline => None,
    };

    let (beets_dot, beets_chip) = importer_chip(health.importer.level);
    let beets_hint = health
        .importer
        .issues
        .first()
        .map(|issue| issue.title.clone())
        .unwrap_or_default();

    let user_not_linked = matches!(
        navidrome_status,
        NavidromeStatus::InvalidCredentials | NavidromeStatus::Unknown
    );

    rsx! {
      div { class: "flex justify-center gap-6 text-xs font-mono text-gray-500 flex-wrap",
        span { class: "flex items-center gap-2",
          span {
            class: format!(
                "w-2 h-2 rounded-full {}",
                if health.downloader_online { "bg-beet-leaf animate-pulse" } else { "bg-red-500" },
            ),
          }
          if health.downloader_online {
            "DOWNLOADER ONLINE"
          } else {
            "DOWNLOADER OFFLINE"
          }
        }
        span { class: "flex items-center gap-2", title: beets_hint,
          span { class: "w-2 h-2 rounded-full {beets_dot}" }
          "BEETS {beets_chip}"
        }
        span {
          class: "flex items-center gap-2",
          title: nav_hint.unwrap_or_default(),
          span {
            class: format!(
                "w-2 h-2 rounded-full {}",
                if health.navidrome_online { "bg-blue-400 animate-pulse" } else { "bg-red-500" },
            ),
          }
          if health.navidrome_online {
            "NAVIDROME ONLINE"
          } else if user_not_linked {
            "NAVIDROME NOT LINKED"
          } else {
            "NAVIDROME OFFLINE"
          }
        }
      }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importer_chip_texts() {
        assert_eq!(
            importer_chip(HealthLevel::Ready),
            ("bg-beet-accent", "READY")
        );
        assert_eq!(
            importer_chip(HealthLevel::Degraded),
            ("bg-yellow-500", "WARN")
        );
        assert_eq!(importer_chip(HealthLevel::Broken), ("bg-red-500", "BROKEN"));
        assert_eq!(
            importer_chip(HealthLevel::Checking),
            ("bg-gray-500 animate-pulse", "CHECKING")
        );
    }
}
