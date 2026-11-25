use dioxus::prelude::*;
use shared::backends::{BackendMeta, Delivery};
use shared::failure::FailureCode;
use shared::system::{DownloaderHealth, HealthLevel, NavidromeStatus, SystemHealth};

use crate::{Chevron, FOCUS_RING};

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

/// What to do about an account that has no Navidrome login yet.
pub const NOT_LINKED_HINT: &str = "Not connected yet: use Connect Navidrome in Settings > Users";

/// The Navidrome chip's hint: why Navidrome is offline when the health check
/// says so, else what the user's link status says.
pub fn navidrome_hint(health: &SystemHealth, status: NavidromeStatus) -> Option<String> {
    if !health.navidrome_online {
        if let Some(failure) = &health.navidrome_failure {
            return Some(failure.message.clone());
        }
    }
    match status {
        NavidromeStatus::Connected => None,
        NavidromeStatus::InvalidCredentials => {
            Some("Your username was not found in Navidrome".to_string())
        }
        NavidromeStatus::Unknown => Some("Navidrome credentials not configured".to_string()),
        NavidromeStatus::NotLinked => Some(NOT_LINKED_HINT.to_string()),
        NavidromeStatus::Offline => None,
    }
}

/// One line of status: a status dot, a label (hidden on small screens), a
/// message and an action.
#[component]
pub fn StatusBanner(
    dot_class: &'static str,
    label: &'static str,
    message: String,
    children: Element,
) -> Element {
    rsx! {
        div { class: "mx-4 sm:mx-6 lg:mx-8 mt-2 px-3 py-2 bg-beet-panel border border-white/10 rounded flex items-center gap-3 text-xs font-mono",
            span { class: "w-1.5 h-1.5 rounded-full {dot_class} shrink-0" }
            span { class: "text-gray-500 uppercase tracking-widest shrink-0 hidden sm:inline", "{label}" }
            span { class: "text-gray-400 flex-1 min-w-0 line-clamp-2 sm:line-clamp-1 break-words", "{message}" }
            {children}
        }
    }
}

/// How a StatusBanner action looks, for an action that is a link.
pub const STATUS_ACTION_CLASS: &str =
    "shrink-0 text-gray-300 hover:text-white underline decoration-dotted cursor-pointer";

/// A StatusBanner action button.
#[component]
pub fn StatusAction(label: &'static str, onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button { class: STATUS_ACTION_CLASS, onclick: move |event| onclick.call(event), "{label}" }
    }
}

/// How much a service's state asks of the user, least first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Fine,
    Checking,
    Warning,
    Down,
}

impl Severity {
    fn dot(self) -> &'static str {
        match self {
            Severity::Fine => "bg-beet-leaf",
            Severity::Checking => "bg-gray-500 animate-pulse",
            Severity::Warning => "bg-yellow-500",
            Severity::Down => "bg-red-500",
        }
    }
}

/// One service in the status list.
struct Service {
    name: String,
    severity: Severity,
    dot: &'static str,
    label: String,
    reason: Option<String>,
}

/// A download backend's row: `{NAME} ONLINE`, `OFFLINE` or `RATE-LIMITED`, ` · LOSSY` after it
/// for a backend that delivers lossy files only. `meta` is the backend's, None for one this build
/// does not know.
fn downloader(health: &DownloaderHealth, meta: Option<&BackendMeta>) -> Service {
    let name = health.name.to_uppercase();
    let lossy = if meta.is_some_and(|m| m.delivery == Delivery::LossyOnly) {
        " · LOSSY"
    } else {
        ""
    };
    let rate_limited = health.failure.as_ref().is_some_and(|f| {
        matches!(
            f.code,
            FailureCode::RateLimited | FailureCode::SearchRateLimited
        )
    });
    let (severity, dot, state) = if health.online {
        (Severity::Fine, "bg-beet-leaf animate-pulse", "ONLINE")
    } else if rate_limited {
        (Severity::Warning, "bg-yellow-500", "RATE-LIMITED")
    } else {
        (Severity::Down, "bg-red-500", "OFFLINE")
    };
    Service {
        name: health.name.clone(),
        severity,
        dot,
        label: format!("{name} {state}{lossy}"),
        reason: health.failure.as_ref().map(|f| f.message.clone()),
    }
}

/// Each download backend, beets and Navidrome as the status list shows them.
fn services(health: &SystemHealth, navidrome_status: NavidromeStatus) -> Vec<Service> {
    let mut services: Vec<Service> = (health.downloaders.iter())
        .map(|d| downloader(d, shared::backends::meta(&d.id)))
        .collect();

    let (beets_dot, beets_chip) = importer_chip(health.importer.level);
    let beets = Service {
        name: "Beets".to_string(),
        severity: match health.importer.level {
            HealthLevel::Ready => Severity::Fine,
            HealthLevel::Checking => Severity::Checking,
            HealthLevel::Degraded => Severity::Warning,
            HealthLevel::Broken => Severity::Down,
        },
        dot: beets_dot,
        label: format!("BEETS {beets_chip}"),
        reason: health
            .importer
            .issues
            .first()
            .map(|issue| issue.title.clone()),
    };

    let nav_hint = navidrome_hint(health, navidrome_status);
    let user_not_linked = matches!(
        navidrome_status,
        NavidromeStatus::InvalidCredentials | NavidromeStatus::Unknown | NavidromeStatus::NotLinked
    );
    let navidrome = Service {
        name: "Navidrome".to_string(),
        severity: if !health.navidrome_online {
            Severity::Down
        } else if nav_hint.is_some() {
            Severity::Warning
        } else {
            Severity::Fine
        },
        dot: if health.navidrome_online {
            "bg-blue-400 animate-pulse"
        } else {
            "bg-red-500"
        },
        label: if health.navidrome_online {
            "NAVIDROME ONLINE"
        } else if user_not_linked {
            "NAVIDROME NOT LINKED"
        } else {
            "NAVIDROME OFFLINE"
        }
        .to_string(),
        reason: nav_hint,
    };

    services.extend([beets, navidrome]);
    services
}

/// The one line for all of `services`: the worst state, and what needs
/// attention, or what is still being checked.
fn summary(services: &[Service]) -> (Severity, String) {
    let worst = services
        .iter()
        .map(|service| service.severity)
        .max()
        .unwrap_or(Severity::Fine);
    let names = |at: Severity| {
        services
            .iter()
            .filter(|service| service.severity >= at)
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>()
    };
    let line = match worst {
        Severity::Fine => "All services online".to_string(),
        Severity::Checking => format!("Checking {}", names(Severity::Checking).join(", ")),
        Severity::Warning | Severity::Down => {
            let names = names(Severity::Warning);
            let verb = if names.len() == 1 { "needs" } else { "need" };
            format!("{} {verb} attention", names.join(", "))
        }
    };
    (worst, line)
}

/// One line saying which services need attention, which opens to each
/// service's state and the reason for it.
#[component]
pub fn SystemStatus(health: SystemHealth, navidrome_status: NavidromeStatus) -> Element {
    let services = services(&health, navidrome_status);
    let (worst, line) = summary(&services);

    rsx! {
      details { class: "group/status mt-[10px] w-full font-mono text-[11px] text-gray-500",
        summary { class: "inline-flex items-center gap-2 min-h-[44px] max-w-full rounded cursor-pointer list-none hover:text-gray-300 {FOCUS_RING} [&::-webkit-details-marker]:hidden",
          span { class: "size-[6px] flex-none rounded-full {worst.dot()}", aria_hidden: "true" }
          span { class: "min-w-0", "{line}" }
          Chevron { class: "transition-transform duration-200 group-open/status:rotate-180" }
        }
        ul { class: "grid gap-2 mb-1 px-3 py-[10px] leading-normal list-none bg-beet-panel/60 border border-white/6 rounded-md",
          for service in services {
            li { key: "{service.name}", class: "flex items-baseline gap-[10px]",
              span {
                class: "size-[6px] flex-none rounded-full -translate-y-px {service.dot}",
                aria_hidden: "true",
              }
              span { class: "min-w-0",
                span { class: "block text-gray-300", "{service.label}" }
                if let Some(reason) = service.reason {
                  span { class: "block break-words", "{reason}" }
                }
              }
            }
          }
        }
      }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_banner_wraps_its_message_on_phones_next_to_its_action() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                StatusBanner {
                    dot_class: "bg-beet-accent",
                    label: "WHAT'S NEW",
                    message: "Could not load the release notes.".to_string(),
                    StatusAction { label: "Try again", onclick: |_| {} }
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("line-clamp-2 sm:line-clamp-1 break-words\">Could not load"),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<button class=\"{STATUS_ACTION_CLASS}\">Try again</button>"
            )),
            "{html}"
        );
    }

    fn render_status(health: SystemHealth, navidrome_status: NavidromeStatus) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(health, navidrome_status): (SystemHealth, NavidromeStatus)| {
                rsx! { SystemStatus { health, navidrome_status } }
            },
            (health, navidrome_status),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The text of the status line, the part shown while it is closed.
    fn line(html: &str) -> &str {
        let start =
            html.find("<span class=\"min-w-0\">").unwrap() + "<span class=\"min-w-0\">".len();
        &html[start..start + html[start..].find("</span>").unwrap()]
    }

    fn soulseek(failure: Option<shared::failure::Failure>) -> DownloaderHealth {
        DownloaderHealth {
            id: "slskd".into(),
            name: "Soulseek".into(),
            online: failure.is_none(),
            failure,
        }
    }

    fn ready_importer() -> shared::system::ImporterHealth {
        shared::system::ImporterHealth {
            level: HealthLevel::Ready,
            ..Default::default()
        }
    }

    #[test]
    fn the_line_names_what_needs_attention_and_the_list_says_why() {
        use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

        let health = SystemHealth {
            downloaders: vec![soulseek(Some(Failure::new(
                FailureCode::BackendUnreachable,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "slskd did not answer at http://slskd:5030",
            )))],
            importer: shared::system::ImporterHealth {
                level: HealthLevel::Broken,
                issues: vec![shared::system::HealthIssue {
                    severity: HealthLevel::Broken,
                    title: "Cannot determine the Python that runs beet".to_string(),
                    detail: String::new(),
                }],
                facts: vec![],
            },
            navidrome_online: false,
            ..SystemHealth::default()
        };
        let html = render_status(health, NavidromeStatus::NotLinked);
        assert!(html.starts_with("<details"), "{html}");
        // Closed until asked.
        assert!(
            !crate::opening_tag(&html, "group/status").contains("open"),
            "{html}"
        );
        assert_eq!(line(&html), "Soulseek, Beets, Navidrome need attention");
        for text in [
            "SOULSEEK OFFLINE",
            "slskd did not answer at http://slskd:5030",
            "BEETS BROKEN",
            "Cannot determine the Python that runs beet",
            "NAVIDROME NOT LINKED",
            NOT_LINKED_HINT,
        ] {
            let text = text.replace('>', "&#62;");
            assert!(
                html.contains(&format!(">{text}</span>")),
                "{text} in {html}"
            );
        }
    }

    #[test]
    fn one_service_in_trouble_is_named_alone() {
        let health = SystemHealth {
            downloaders: vec![soulseek(None)],
            importer: shared::system::ImporterHealth {
                level: HealthLevel::Degraded,
                ..Default::default()
            },
            navidrome_online: true,
            ..SystemHealth::default()
        };
        let html = render_status(health, NavidromeStatus::Connected);
        assert_eq!(line(&html), "Beets needs attention");
        assert!(html.contains("bg-yellow-500"), "{html}");
    }

    #[test]
    fn a_linked_account_missing_from_navidrome_needs_attention_while_it_is_online() {
        let health = SystemHealth {
            downloaders: vec![soulseek(None)],
            importer: ready_importer(),
            navidrome_online: true,
            ..SystemHealth::default()
        };
        let html = render_status(health, NavidromeStatus::InvalidCredentials);
        assert_eq!(line(&html), "Navidrome needs attention");
        assert!(html.contains(">NAVIDROME ONLINE</span>"), "{html}");
        assert!(
            html.contains(">Your username was not found in Navidrome</span>"),
            "{html}"
        );
    }

    #[test]
    fn everything_up_reads_all_services_online_and_still_lists_them() {
        let health = SystemHealth {
            downloaders: vec![soulseek(None)],
            importer: ready_importer(),
            navidrome_online: true,
            ..SystemHealth::default()
        };
        let html = render_status(health.clone(), NavidromeStatus::Connected);
        assert_eq!(line(&html), "All services online");
        for label in ["SOULSEEK ONLINE", "BEETS READY", "NAVIDROME ONLINE"] {
            assert!(
                html.contains(&format!(">{label}</span>")),
                "{label} in {html}"
            );
        }

        let checking = SystemHealth {
            importer: Default::default(),
            ..health
        };
        let html = render_status(checking, NavidromeStatus::Connected);
        assert_eq!(line(&html), "Checking Beets");
    }

    #[test]
    fn each_download_backend_has_its_row_and_one_down_names_only_itself() {
        use shared::failure::{Failure, RetryAdvice, Stage};
        let health = SystemHealth {
            downloaders: vec![
                soulseek(None),
                DownloaderHealth {
                    id: "archive".into(),
                    name: "Internet Archive".into(),
                    online: false,
                    failure: Some(Failure::new(
                        FailureCode::BackendUnreachable,
                        Stage::SourceSearch,
                        RetryAdvice::SameSourceLater,
                        "archive.org did not answer",
                    )),
                },
            ],
            importer: ready_importer(),
            navidrome_online: true,
            ..SystemHealth::default()
        };
        let html = render_status(health, NavidromeStatus::Connected);
        assert_eq!(line(&html), "Internet Archive needs attention");
        for text in [
            "SOULSEEK ONLINE",
            "INTERNET ARCHIVE OFFLINE",
            "archive.org did not answer",
        ] {
            assert!(
                html.contains(&format!(">{text}</span>")),
                "{text} in {html}"
            );
        }
    }

    #[test]
    fn a_rate_limited_backend_is_a_warning_and_a_lossy_one_says_so() {
        use shared::backends::{Switch, SLSKD};
        use shared::failure::{Failure, RetryAdvice, Stage};
        const YOUTUBE: BackendMeta = BackendMeta {
            id: "ytdlp",
            name: "YouTube Music",
            delivery: Delivery::LossyOnly,
            switch: Switch::AdminToggle,
            ..SLSKD
        };
        let health = |failure: Option<Failure>| DownloaderHealth {
            id: "ytdlp".into(),
            name: "YouTube Music".into(),
            online: failure.is_none(),
            failure,
        };
        let limited = |code| {
            Some(Failure::new(
                code,
                Stage::SourceSearch,
                RetryAdvice::SameSourceLater,
                "YouTube asks to slow down",
            ))
        };
        let online = downloader(&health(None), Some(&YOUTUBE));
        assert_eq!(online.label, "YOUTUBE MUSIC ONLINE · LOSSY");
        assert_eq!(online.severity, Severity::Fine);
        for code in [FailureCode::RateLimited, FailureCode::SearchRateLimited] {
            let row = downloader(&health(limited(code)), Some(&YOUTUBE));
            assert_eq!(row.label, "YOUTUBE MUSIC RATE-LIMITED · LOSSY");
            assert_eq!(
                (row.severity, row.dot, row.reason.as_deref()),
                (
                    Severity::Warning,
                    "bg-yellow-500",
                    Some("YouTube asks to slow down")
                )
            );
        }
        let down = downloader(
            &health(limited(FailureCode::BackendUnreachable)),
            Some(&YOUTUBE),
        );
        assert_eq!(down.label, "YOUTUBE MUSIC OFFLINE · LOSSY");
        assert_eq!(down.severity, Severity::Down);
        let plain = downloader(&soulseek(None), Some(&SLSKD));
        assert_eq!(plain.label, "SOULSEEK ONLINE");
        assert_eq!(downloader(&soulseek(None), None).label, "SOULSEEK ONLINE");
    }

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

    #[test]
    fn the_navidrome_hint_says_why_it_is_offline() {
        use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

        let message = "Navidrome appears unavailable (circuit breaker open)";
        let offline = SystemHealth {
            navidrome_failure: Some(Failure::new(
                FailureCode::NavidromeCircuitOpen,
                Stage::LibraryCheck,
                RetryAdvice::SameSourceLater,
                message,
            )),
            ..SystemHealth::default()
        };
        assert_eq!(
            navidrome_hint(&offline, NavidromeStatus::Connected).as_deref(),
            Some(message)
        );

        let online = SystemHealth {
            navidrome_online: true,
            ..offline.clone()
        };
        assert_eq!(navidrome_hint(&online, NavidromeStatus::Connected), None);

        // An account without a Navidrome login is pointed to the action that
        // connects it, whatever way it signs in.
        assert_eq!(
            navidrome_hint(&online, NavidromeStatus::NotLinked).as_deref(),
            Some(NOT_LINKED_HINT)
        );
        assert!(NOT_LINKED_HINT.contains("Connect Navidrome"));

        let unexplained = SystemHealth::default();
        assert_eq!(
            navidrome_hint(&unexplained, NavidromeStatus::Unknown).as_deref(),
            Some("Navidrome credentials not configured")
        );
    }
}
