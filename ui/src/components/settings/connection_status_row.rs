use dioxus::prelude::*;
use shared::system::{HealthLevel, ImporterHealth};

use crate::{importer_chip, Disclosure};

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

/// What the Beets row shows: the status dot, a summary that is never
/// "Connected", and when Broken, the blocking issue's detail, which says
/// what to fix.
#[derive(Debug, PartialEq)]
pub(crate) struct BeetsRow {
    pub dot_class: &'static str,
    pub summary: String,
    pub note: Option<String>,
}

pub(crate) fn beets_row(h: &ImporterHealth) -> BeetsRow {
    let (dot_class, _) = importer_chip(h.level);
    let blocking = h.blocking_issue();
    let summary = match h.level {
        HealthLevel::Checking => "Checking...".to_string(),
        HealthLevel::Ready => "Ready".to_string(),
        HealthLevel::Degraded => "Ready with warnings".to_string(),
        HealthLevel::Broken => blocking
            .map(|issue| issue.title.clone())
            .unwrap_or_else(|| h.level.chip().to_string()),
    };
    let note = match h.level {
        HealthLevel::Broken => blocking
            .map(|issue| issue.detail.clone())
            .filter(|detail| !detail.is_empty()),
        _ => None,
    };
    BeetsRow {
        dot_class,
        summary,
        note,
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
        let row = |dot_class, summary: &str, note: Option<&str>| BeetsRow {
            dot_class,
            summary: summary.to_string(),
            note: note.map(str::to_string),
        };
        assert_eq!(
            beets_row(&health(HealthLevel::Checking, vec![])),
            row("bg-gray-500 animate-pulse", "Checking...", None)
        );
        assert_eq!(
            beets_row(&health(HealthLevel::Ready, vec![])),
            row("bg-beet-accent", "Ready", None)
        );
        let warning = HealthIssue {
            severity: HealthLevel::Degraded,
            title: "import.autotag is off".into(),
            detail: "Imports will not be tagged.".into(),
        };
        assert_eq!(
            beets_row(&health(HealthLevel::Degraded, vec![warning.clone()])),
            row("bg-yellow-500", "Ready with warnings", None)
        );
        let blocking = HealthIssue {
            severity: HealthLevel::Broken,
            title: "No metadata source plugin loaded".into(),
            detail: "Add musicbrainz to plugins:.".into(),
        };
        assert_eq!(
            beets_row(&health(HealthLevel::Broken, vec![warning, blocking])),
            row(
                "bg-red-500",
                "No metadata source plugin loaded",
                Some("Add musicbrainz to plugins:.")
            )
        );
    }
}
