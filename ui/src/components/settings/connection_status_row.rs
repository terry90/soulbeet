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
        div { class: "p-4 rounded-[12px] bg-beet-surface border border-beet-border-subtle",
            h3 { class: "text-sm font-semibold text-beet-text-primary mb-2", "{props.label}" }
            div { class: "mb-2",
                StatusLine { dot_class: props.dot_class, summary: props.summary }
            }
            if let Some(note) = &props.note {
                p { class: "text-xs text-beet-text-secondary leading-relaxed", "{note}" }
            }
            if let Some(details) = props.details {
                Disclosure { class: "mt-2.5", label: "details", {details} }
            }
        }
    }
}

/// A status dot and its summary.
#[component]
pub(crate) fn StatusLine(dot_class: &'static str, summary: String) -> Element {
    rsx! {
        div { class: "flex items-center gap-2",
            span { class: "w-2 h-2 rounded-full shrink-0 {dot_class}" }
            span { class: "text-xs font-medium text-beet-text-primary", "{summary}" }
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
        div { class: "mt-3 p-3 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle space-y-3 text-xs",
            if !health.issues.is_empty() {
                ul { class: "space-y-2",
                    for issue in health.issues.iter() {
                        li {
                            div { class: "flex items-baseline gap-2",
                                span { class: "tabular-nums-mono text-[11px] font-semibold {issue_class(issue.severity)}", "{issue.severity.chip()}" }
                                span { class: "text-beet-text-primary", "{issue.title}" }
                            }
                            if !issue.detail.is_empty() {
                                p { class: "text-beet-text-secondary mt-0.5 whitespace-pre-wrap break-words leading-relaxed", "{issue.detail}" }
                            }
                        }
                    }
                }
            }
            if !health.facts.is_empty() {
                dl { class: "grid grid-cols-[auto_1fr] gap-x-3 gap-y-1.5 tabular-nums-mono",
                    for (name, value) in health.facts.iter() {
                        dt { class: "text-beet-text-tertiary", "{name}" }
                        dd { class: "text-beet-text-secondary break-all", "{value}" }
                    }
                }
            }
        }
    }
}

fn issue_class(severity: HealthLevel) -> &'static str {
    match severity {
        HealthLevel::Broken => "text-beet-crimson-text shrink-0",
        HealthLevel::Degraded => "text-beet-gold shrink-0",
        HealthLevel::Ready | HealthLevel::Checking => "text-beet-text-tertiary shrink-0",
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
            row("bg-beet-text-tertiary animate-pulse", "Checking...", None)
        );
        assert_eq!(
            beets_row(&health(HealthLevel::Ready, vec![])),
            row("bg-beet-leaf-solid", "Ready", None)
        );
        let warning = HealthIssue {
            severity: HealthLevel::Degraded,
            title: "import.autotag is off".into(),
            detail: "Imports will not be tagged.".into(),
        };
        assert_eq!(
            beets_row(&health(HealthLevel::Degraded, vec![warning.clone()])),
            row("bg-beet-gold", "Ready with warnings", None)
        );
        let blocking = HealthIssue {
            severity: HealthLevel::Broken,
            title: "No metadata source plugin loaded".into(),
            detail: "Add musicbrainz to plugins:.".into(),
        };
        assert_eq!(
            beets_row(&health(HealthLevel::Broken, vec![warning, blocking])),
            row(
                "bg-beet-crimson-text",
                "No metadata source plugin loaded",
                Some("Add musicbrainz to plugins:.")
            )
        );
    }
}
