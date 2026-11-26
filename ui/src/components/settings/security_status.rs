use api::security::SecurityStatus;
use dioxus::prelude::*;

use super::panel::{LoadError, Loading, Panel};
use crate::auth::use_auth;
use crate::friendly_error;

/// Admin-only panel listing what stops sign-ins and what keeps MCP
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
    if status.mcp_off.is_none()
        && status.sign_in_problem.is_none()
        && status.sso_off.is_none()
        && status.mcp_blocked_by.is_empty()
    {
        return rsx! {
            p { class: "text-sm text-beet-leaf-text", "All security checks pass." }
        };
    }
    rsx! {
        if let Some(problem) = status.sign_in_problem {
            p { class: "px-3.5 py-2.5 rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-solid/40 text-sm text-beet-crimson-text mb-3", "{problem}" }
        }
        if let Some(why) = status.sso_off {
            p { class: "px-3.5 py-2.5 rounded-[10px] bg-beet-gold-surface border border-beet-gold/25 text-sm text-beet-gold mb-3",
                "Single sign-on is off: {why}."
            }
        }
        if let Some(why) = status.mcp_off {
            p { class: "text-sm text-beet-text-secondary mb-3",
                "MCP access is turned off: {why}."
            }
        }
        if !status.mcp_blocked_by.is_empty() {
            p { class: "text-sm text-beet-text-secondary mb-2.5",
                "MCP access stays off until these are fixed:"
            }
            ul { class: "list-disc pl-5 space-y-1.5 text-sm text-beet-gold",
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
            mcp_off: None,
            sso_off: None,
        }
    }

    #[test]
    fn checks_say_when_mcp_is_switched_off() {
        let html = render(SecurityStatus {
            mcp_blocked_by: vec![],
            sign_in_problem: None,
            mcp_off: Some("MCP=off is set".into()),
            sso_off: None,
        });
        assert!(html.contains("MCP access is turned off: MCP=off is set."));
        assert!(!html.contains("All security checks pass."));
        let html = render(SecurityStatus {
            mcp_blocked_by: vec![],
            sign_in_problem: None,
            mcp_off: Some("MCP is set to 'disabled', which is neither on nor off".into()),
            sso_off: None,
        });
        assert!(html.contains(
            "MCP access is turned off: MCP is set to &#39;disabled&#39;, which is neither on nor off."
        ));
    }

    #[test]
    fn checks_say_why_single_sign_on_is_off() {
        let mut s = status(vec![], None);
        s.sso_off = Some("OIDC_CLIENT_ID is not set".into());
        let html = render(s);
        assert!(html.contains("Single sign-on is off: OIDC_CLIENT_ID is not set."));
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
        assert!(html.contains("MCP access stays off until these are fixed:"));
        assert!(html.contains("still has the password"));
    }

    #[test]
    fn checks_show_a_sign_in_problem() {
        let html = render(status(vec![], Some("PUBLIC_URL is not usable: bad.")));
        assert!(html.contains("PUBLIC_URL is not usable: bad."));
        assert!(!html.contains("All security checks pass."));
        assert!(!html.contains("MCP access stays off"));
    }
}
