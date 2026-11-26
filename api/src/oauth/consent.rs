//! The pages `/oauth/authorize` shows in the browser: the consent question
//! and the error page. Every value is escaped; the pages load nothing and
//! cannot be framed.

use axum::http::header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// No `form-action`: Chromium applies it to the redirect after the form
/// posts, which would stop Allow from returning to the app.
const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'";

/// The app colors of `web/assets/input.css`.
const STYLE: &str = "\
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:#0f0518;color:#e5e7eb;font-family:system-ui,sans-serif;line-height:1.5}\
main{max-width:32rem;margin:1rem;padding:2rem;background:#1a0b2e;border:1px solid #4a2b6b;\
border-radius:.5rem}\
h1{margin-top:0;font-size:1.4rem;color:#fff}\
.warning{color:#ff00ff}\
form{display:flex;gap:1rem;margin-top:1.5rem}\
button{flex:1;padding:.6rem 1rem;font:inherit;font-weight:600;border-radius:.375rem;cursor:pointer}\
button[value=allow]{background:#00ff9d;color:#0f0518;border:1px solid #00ff9d}\
button[value=deny]{background:transparent;color:#e5e7eb;border:1px solid #4a2b6b}";

/// `s` safe in HTML text and in a quoted attribute value.
pub(crate) fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// What the consent page says (D8).
pub(crate) struct ConsentView<'a> {
    /// The app's main name: the host it is known by.
    pub app_host: &'a str,
    /// The name the app gives itself, shown as its claim.
    pub client_name: Option<&'a str>,
    /// The host the answer goes to, without port.
    pub redirect_host: &'a str,
    /// The answer goes to a loopback redirect URI: an app on a computer.
    pub returns_to_loopback: bool,
    pub username: &'a str,
    pub is_admin: bool,
    pub request_id: &'a str,
}

/// The question whether `view.app_host` may use Soulful as the user.
pub(crate) fn consent_page(view: &ConsentView) -> Response {
    let app = escape_html(view.app_host);
    let redirect = escape_html(view.redirect_host);
    let mut body = format!(
        "<h1>Connect {app} to Soulbeet</h1>\n<p>{app} wants to use Soulbeet as {}.</p>\n",
        escape_html(view.username)
    );
    if let Some(name) = view.client_name {
        body.push_str(&format!(
            "<p>It calls itself \"{}\".</p>\n",
            escape_html(name)
        ));
    }
    body.push_str(
        "<p>Once allowed, it can see your library, downloads and settings, search for music, \
         download into your folders, and manage your playlists and ratings. It cannot delete, \
         move or re-tag library files unless you turn that on for it in Settings &gt; Users &gt; Connected apps.</p>\n",
    );
    if view.is_admin {
        body.push_str(
            "<p>Admin library access across other users' folders also starts off until you turn \
             it on for this app in Settings &gt; Users &gt; Connected apps.</p>\n",
        );
    }
    body.push_str(&format!(
        "<p>When you answer, Soulbeet sends you back to {redirect}.</p>\n"
    ));
    if view.returns_to_loopback {
        body.push_str(&format!(
            "<p class=\"warning\">This app runs on a computer (it returns to {redirect}). Allow \
             it only if you started it yourself.</p>\n"
        ));
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/oauth/authorize\">\n\
         <input type=\"hidden\" name=\"request\" value=\"{}\">\n\
         <button type=\"submit\" name=\"decision\" value=\"allow\">Allow</button>\n\
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button>\n\
         </form>\n",
        escape_html(view.request_id)
    ));
    page(StatusCode::OK, &format!("Connect {app} to Soulbeet"), &body)
}

/// `message` on a page of its own, with the consent page's headers.
pub(crate) fn error_page(status: StatusCode, message: &str) -> Response {
    page(
        status,
        "Soulbeet",
        &format!("<h1>Soulbeet</h1>\n<p>{}</p>\n", escape_html(message)),
    )
}

/// `title` and `body` are HTML already escaped.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<main>\n{body}</main>\n\
         </body>\n</html>\n"
    );
    (
        status,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CONTENT_SECURITY_POLICY, PAGE_CSP),
            (REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_special_characters_are_escaped() {
        assert_eq!(
            escape_html(r#"<a href="x">Tom & Jerry's</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&#39;s&lt;/a&gt;"
        );
    }

    #[tokio::test]
    async fn the_error_page_escapes_its_message_and_keeps_the_headers() {
        let response = error_page(StatusCode::BAD_REQUEST, "<b>no</b>");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[CONTENT_SECURITY_POLICY], PAGE_CSP);
        assert_eq!(response.headers()[REFERRER_POLICY], "no-referrer");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("<p>&lt;b&gt;no&lt;/b&gt;</p>"), "{body}");
    }
}
