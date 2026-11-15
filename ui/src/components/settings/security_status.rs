use api::security::GateReason;
use dioxus::prelude::*;

use crate::friendly_error;

/// Admin-only panel listing what keeps Claude (MCP) access off.
#[component]
pub fn SecurityStatusPanel() -> Element {
    let mut status = use_resource(|| async { api::get_security_status().await });

    rsx! {
        div { class: "bg-beet-panel border border-white/10 p-6 rounded-lg shadow-2xl relative z-10",
            h2 { class: "text-xl font-bold mb-4 text-beet-accent font-display", "Security Checks" }
            match &*status.read() {
                None => rsx! {
                    div { class: "animate-pulse text-gray-400 font-mono", "Loading..." }
                },
                Some(Err(e)) => rsx! {
                    div { class: "text-red-400 text-sm font-mono mb-3", "{friendly_error(e)}" }
                    button {
                        class: "text-xs font-mono text-gray-400 hover:text-white underline decoration-dotted cursor-pointer",
                        onclick: move |_| status.restart(),
                        "Retry"
                    }
                },
                Some(Ok(s)) => rsx! { GateReasonList { reasons: s.mcp_blocked_by.clone() } },
            }
        }
    }
}

#[component]
fn GateReasonList(reasons: Vec<GateReason>) -> Element {
    if reasons.is_empty() {
        return rsx! {
            p { class: "text-sm font-mono text-gray-300", "All security checks pass." }
        };
    }
    rsx! {
        p { class: "text-sm font-mono text-gray-300 mb-3",
            "Claude (MCP) access stays off until these are fixed:"
        }
        ul { class: "list-disc pl-5 space-y-1 text-sm font-mono text-yellow-400",
            for reason in reasons {
                li { "{reason.message}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::security::GateCode;

    fn render(reasons: Vec<GateReason>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |reasons: Vec<GateReason>| rsx! { GateReasonList { reasons } },
            reasons,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn gate_list_says_when_all_checks_pass() {
        assert!(render(vec![]).contains("All security checks pass."));
    }

    #[test]
    fn gate_list_shows_each_reason() {
        let html = render(vec![GateReason {
            code: GateCode::DefaultAdminPassword,
            message: "The seeded admin account still has the password 'admin'. Change it in Settings > Users.".into(),
        }]);
        assert!(html.contains("Claude (MCP) access stays off until these are fixed:"));
        assert!(html.contains("still has the password"));
    }
}
