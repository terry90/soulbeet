use api::security::SecurityStatus;
use dioxus::prelude::*;

use super::panel::{LoadError, Loading, Panel};
use crate::auth::use_auth;
use crate::friendly_error;

/// Admin-only panel listing what stops sign-ins and what keeps Claude (MCP)
/// access off.
#[component]
pub fn SecurityStatusPanel() -> Element {
    let auth = use_auth();
    let mut status =
        use_resource(move || async move { auth.call(api::get_security_status()).await });

    rsx! {
        Panel { title: "Security Checks",
            match &*status.read() {
                None => rsx! { Loading {} },
                Some(Err(e)) => rsx! {
                    LoadError { message: friendly_error(e), on_retry: move |_| status.restart() }
                },
                Some(Ok(s)) => rsx! { SecurityChecks { status: s.clone() } },
            }
        }
    }
}

#[component]
fn SecurityChecks(status: SecurityStatus) -> Element {
    if status.mcp_enabled && status.sign_in_problem.is_none() && status.mcp_blocked_by.is_empty() {
        return rsx! {
            p { class: "text-sm font-mono text-gray-300", "All security checks pass." }
        };
    }
    rsx! {
        if let Some(problem) = status.sign_in_problem {
            p { class: "text-sm font-mono text-red-400 mb-3", "{problem}" }
        }
        if !status.mcp_enabled {
            p { class: "text-sm font-mono text-gray-300 mb-3",
                "Claude (MCP) access is turned off: MCP=off is set."
            }
        }
        if !status.mcp_blocked_by.is_empty() {
            p { class: "text-sm font-mono text-gray-300 mb-3",
                "Claude (MCP) access stays off until these are fixed:"
            }
            ul { class: "list-disc pl-5 space-y-1 text-sm font-mono text-yellow-400",
                for reason in status.mcp_blocked_by {
                    li { "{reason.message}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::security::{GateCode, GateReason};

    fn render(status: SecurityStatus) -> String {
        let mut dom = VirtualDom::new_with_props(
            |status: SecurityStatus| rsx! { SecurityChecks { status } },
            status,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn status(mcp_blocked_by: Vec<GateReason>, sign_in_problem: Option<&str>) -> SecurityStatus {
        SecurityStatus {
            mcp_blocked_by,
            sign_in_problem: sign_in_problem.map(Into::into),
            mcp_enabled: true,
        }
    }

    #[test]
    fn checks_say_when_mcp_is_switched_off() {
        let html = render(SecurityStatus {
            mcp_blocked_by: vec![],
            sign_in_problem: None,
            mcp_enabled: false,
        });
        assert!(html.contains("Claude (MCP) access is turned off: MCP=off is set."));
        assert!(!html.contains("All security checks pass."));
    }

    #[test]
    fn checks_say_when_all_pass() {
        assert!(render(status(vec![], None)).contains("All security checks pass."));
    }

    #[test]
    fn checks_show_each_gate_reason() {
        let html = render(status(
            vec![GateReason {
                code: GateCode::DefaultAdminPassword,
                message: "The seeded admin account still has the password 'admin'. Change it in Settings > Users.".into(),
            }],
            None,
        ));
        assert!(html.contains("Claude (MCP) access stays off until these are fixed:"));
        assert!(html.contains("still has the password"));
    }

    #[test]
    fn checks_show_a_sign_in_problem() {
        let html = render(status(vec![], Some("PUBLIC_URL is not usable: bad.")));
        assert!(html.contains("PUBLIC_URL is not usable: bad."));
        assert!(!html.contains("All security checks pass."));
        assert!(!html.contains("Claude (MCP) access stays off"));
    }
}
