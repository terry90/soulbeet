use dioxus::prelude::*;
use shared::system::{HealthLevel, ImporterHealth};

use crate::Disclosure;

#[derive(Props, Clone, PartialEq)]
pub(crate) struct ConnectionStatusRowProps {
    #[props(into)]
    pub label: String,
    pub dot_class: &'static str,
    #[props(into)]
    pub summary: String,
    #[props(default)]
    pub note: Option<String>,
    #[props(default)]
    pub details: Option<Element>,
}

/// One service under Settings > Config > Connections: status dot, summary, note and details.
#[component]
pub(crate) fn ConnectionStatusRow(props: ConnectionStatusRowProps) -> Element {
    rsx! {
        div {
            h3 { class: "text-sm font-semibold text-white mb-3", "{props.label}" }
            div { class: "flex items-center gap-2 mb-2",
                span { class: "w-2 h-2 rounded-full shrink-0 {props.dot_class}" }
                span { class: "text-xs font-mono text-gray-300", "{props.summary}" }
            }
            if let Some(note) = &props.note {
                p { class: "text-xs text-gray-400 font-mono", "{note}" }
            }
            if let Some(details) = props.details {
                Disclosure { class: "mt-2", label: "details", {details} }
            }
        }
    }
}

/// Dot class and summary for the Beets row. The summary is never "Connected".
pub(crate) fn beets_row(h: &ImporterHealth) -> (&'static str, String) {
    match h.level {
        HealthLevel::Checking => ("bg-gray-500", "Checking...".to_string()),
        HealthLevel::Ready => ("bg-beet-accent", "Ready".to_string()),
        HealthLevel::Degraded => ("bg-yellow-500", "Ready with warnings".to_string()),
        HealthLevel::Broken => (
            "bg-red-500",
            h.blocking_issue()
                .map(|issue| issue.title.clone())
                .unwrap_or_else(|| h.level.chip().to_string()),
        ),
    }
}

/// The importer's issues and facts, shown under the Beets row.
#[component]
pub(crate) fn ImporterDetails(health: ImporterHealth) -> Element {
    rsx! {
        div { class: "mt-2 space-y-3 text-xs font-mono",
            if !health.issues.is_empty() {
                ul { class: "space-y-2",
                    for issue in health.issues.iter() {
                        li {
                            div { class: "flex items-baseline gap-2",
                                span { class: issue_class(issue.severity), "{issue.severity.chip()}" }
                                span { class: "text-gray-300", "{issue.title}" }
                            }
                            if !issue.detail.is_empty() {
                                p { class: "text-gray-500 mt-0.5 whitespace-pre-wrap break-words", "{issue.detail}" }
                            }
                        }
                    }
                }
            }
            if !health.facts.is_empty() {
                dl { class: "grid grid-cols-[auto_1fr] gap-x-3 gap-y-1",
                    for (name, value) in health.facts.iter() {
                        dt { class: "text-gray-500", "{name}" }
                        dd { class: "text-gray-300 break-all", "{value}" }
                    }
                }
            }
        }
    }
}

fn issue_class(severity: HealthLevel) -> &'static str {
    match severity {
        HealthLevel::Broken => "text-red-400 shrink-0",
        HealthLevel::Degraded => "text-yellow-400 shrink-0",
        HealthLevel::Ready | HealthLevel::Checking => "text-gray-500 shrink-0",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::system::{HealthIssue, HealthLevel, ImporterHealth};

    fn health(level: HealthLevel, issues: Vec<HealthIssue>) -> ImporterHealth {
        ImporterHealth {
            level,
            issues,
            facts: vec![],
        }
    }

    #[test]
    fn beets_row_never_says_connected() {
        assert_eq!(
            beets_row(&health(HealthLevel::Checking, vec![])),
            ("bg-gray-500", "Checking...".to_string())
        );
        assert_eq!(
            beets_row(&health(HealthLevel::Ready, vec![])),
            ("bg-beet-accent", "Ready".to_string())
        );
        let warning = HealthIssue {
            severity: HealthLevel::Degraded,
            title: "import.autotag is off".into(),
            detail: "Imports will not be tagged.".into(),
        };
        assert_eq!(
            beets_row(&health(HealthLevel::Degraded, vec![warning.clone()])),
            ("bg-yellow-500", "Ready with warnings".to_string())
        );
        let blocking = HealthIssue {
            severity: HealthLevel::Broken,
            title: "No metadata source plugin loaded".into(),
            detail: "Add musicbrainz to plugins:.".into(),
        };
        assert_eq!(
            beets_row(&health(HealthLevel::Broken, vec![warning, blocking])),
            ("bg-red-500", "No metadata source plugin loaded".to_string())
        );
    }
}
