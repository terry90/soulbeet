use api::connections::{LoggedRequest, RemoteAccess};
use api::get_remote_access;
use dioxus::prelude::*;

use super::connected_apps::use_browser_origin;
use super::panel::{utc_time, LoadError, Loading, Panel, SectionTitle};
use crate::auth::use_auth;
use crate::friendly_error;

const CELL_CLASS: &str = "px-2 py-1 align-top";

/// Admin-only panel: the address Soulful derives for the page's own request,
/// what keeps MCP clients from connecting there, and the latest connection
/// requests.
#[component]
pub fn RemoteAccessPanel() -> Element {
    let auth = use_auth();
    let origin = use_browser_origin();
    // Starts once the browser has given its origin.
    let mut access = use_resource(move || async move {
        let origin = origin();
        if origin.is_empty() {
            return None;
        }
        Some(auth.call(get_remote_access(origin)).await)
    });

    rsx! {
        Panel { title: "Remote access",
            match &*access.read() {
                None | Some(None) => rsx! { Loading {} },
                Some(Some(Err(e))) => rsx! {
                    LoadError { message: friendly_error(e), on_retry: move |_| access.restart() }
                },
                Some(Some(Ok(a))) => rsx! { RemoteAccessView { access: a.clone() } },
            }
        }
    }
}

#[component]
fn RemoteAccessView(access: RemoteAccess) -> Element {
    rsx! {
        div { class: "text-sm font-mono text-gray-300 space-y-1 mb-4",
            match (&access.derived_base, &access.base_problem) {
                (Some(base), _) => rsx! { p { "Soulbeet sees this page's request at {base}" } },
                (None, Some(problem)) => rsx! {
                    p { class: "text-red-400", "Soulbeet cannot tell this page's address: {problem}" }
                },
                (None, None) => rsx! {},
            }
            p { class: "text-xs text-gray-400",
                "host from {access.host_from}, scheme from {access.scheme_from}"
            }
        }
        ul { class: "list-disc pl-5 space-y-1 text-sm font-mono text-yellow-400 mb-6",
            for line in access.advice {
                li { "{line}" }
            }
        }
        SectionTitle { title: "Latest connection requests" }
        if access.recent.is_empty() {
            p { class: "text-gray-500 font-mono italic",
                "No request has reached the connection paths since Soulbeet started."
            }
        } else {
            div { class: "overflow-x-auto",
                table { class: "w-full text-xs font-mono text-gray-300 text-left",
                    thead { class: "text-gray-400 uppercase tracking-wider",
                        tr {
                            for heading in ["Time (UTC)", "Request", "Address", "From", "Headers", "Status"] {
                                th { class: CELL_CLASS, "{heading}" }
                            }
                        }
                    }
                    tbody {
                        for request in access.recent {
                            RequestRow { request }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn RequestRow(request: LoggedRequest) -> Element {
    let headers = [
        ("Host", &request.host_header),
        ("X-Forwarded-Host", &request.x_forwarded_host),
        ("X-Forwarded-Proto", &request.x_forwarded_proto),
        ("Forwarded", &request.forwarded),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.as_ref().map(|v| format!("{name}: {v}")))
    .collect::<Vec<_>>();
    let address = match (&request.base, &request.base_problem) {
        (Some(base), _) => base.clone(),
        (None, Some(problem)) => problem.clone(),
        (None, None) => String::new(),
    };
    let time = utc_time(request.at, "%Y-%m-%d %H:%M:%S");
    let status = match request.status {
        Some(status) => status.to_string(),
        None => "no answer: the client left first".to_string(),
    };

    rsx! {
        tr { class: "border-t border-white/5",
            td { class: "{CELL_CLASS} whitespace-nowrap", "{time}" }
            td { class: "{CELL_CLASS} break-all", "{request.method} {request.path}" }
            td { class: "{CELL_CLASS} break-all", "{address}" }
            td { class: CELL_CLASS, "host from {request.host_from}, scheme from {request.scheme_from}" }
            td { class: "{CELL_CLASS} break-all",
                for header in headers {
                    div { "{header}" }
                }
            }
            td { class: CELL_CLASS,
                "{status}"
                if let Some(refusal) = &request.refusal {
                    div { class: "text-yellow-400", "{refusal}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_access_lists_advice_and_requests() {
        let access = RemoteAccess {
            derived_base: Some("http://192.168.1.5:9765".into()),
            base_problem: None,
            host_from: "host".into(),
            scheme_from: "local_host_is_http".into(),
            advice: vec!["MCP clients cannot use this address: it is plain http.".into()],
            recent: vec![
                LoggedRequest {
                    at: 1_800_000_000,
                    method: "GET".into(),
                    path: "/.well-known/oauth-protected-resource/mcp".into(),
                    base: Some("http://192.168.1.5:9765".into()),
                    base_problem: None,
                    host_from: "host".into(),
                    scheme_from: "local_host_is_http".into(),
                    host_header: Some("192.168.1.5:9765".into()),
                    x_forwarded_host: None,
                    x_forwarded_proto: None,
                    forwarded: None,
                    status: Some(403),
                    refusal: Some("https_required".into()),
                },
                LoggedRequest {
                    at: 1_800_000_000,
                    method: "GET".into(),
                    path: "/oauth/authorize".into(),
                    base: Some("https://music.example.com".into()),
                    base_problem: None,
                    host_from: "host".into(),
                    scheme_from: "named_host_is_https".into(),
                    host_header: Some("music.example.com".into()),
                    x_forwarded_host: None,
                    x_forwarded_proto: None,
                    forwarded: None,
                    status: None,
                    refusal: None,
                },
            ],
        };
        let mut dom = VirtualDom::new_with_props(
            |access: RemoteAccess| rsx! { RemoteAccessView { access } },
            access,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for text in [
            "http://192.168.1.5:9765",
            "host from host, scheme from local_host_is_http",
            "MCP clients cannot use this address: it is plain http.",
            "/.well-known/oauth-protected-resource/mcp",
            "403",
            "https_required",
            "Host: 192.168.1.5:9765",
            "no answer: the client left first",
        ] {
            assert!(html.contains(text), "{text}");
        }
    }
}
