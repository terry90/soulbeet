//! Shared Refined Midnight Studio stylesheet and bundled font assets for both the main
//! Desktop Studio window and the floating `360×110` always-on-top Mini-Player window.
//!
//! Uses `include_str!` for `web/assets/tailwind.css` and a cross-platform `use_asset_handler`
//! route (`/studio-fonts/...`) backed by `include_bytes!` so both windows always stay 100% in
//! sync with the workspace design system without duplicating binary font or CSS assets.

use dioxus::prelude::*;
use dioxus_desktop::use_asset_handler;
use dioxus_desktop::wry::http::{header, Response as HttpResponse, StatusCode};

const STUDIO_CSS: &str = include_str!("../../web/assets/tailwind.css");
const SPACE_GROTESK_WOFF2: &[u8] =
    include_bytes!("../../web/assets/fonts/SpaceGrotesk-Variable.woff2");
const COMMIT_MONO_WOFF2: &[u8] =
    include_bytes!("../../web/assets/fonts/CommitMono-Variable.woff2");

/// Mounts the Refined Midnight Studio stylesheet and bundled `Space Grotesk` + `Commit Mono`
/// variable fonts inside a desktop window document.
#[component]
pub fn StudioAssets() -> Element {
    use_asset_handler("studio-fonts", move |request, responder| {
        let path = request.uri().path();
        let bytes = if path.ends_with("SpaceGrotesk-Variable.woff2") {
            Some(SPACE_GROTESK_WOFF2)
        } else if path.ends_with("CommitMono-Variable.woff2") {
            Some(COMMIT_MONO_WOFF2)
        } else {
            None
        };

        let resp = match bytes {
            Some(data) => HttpResponse::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "font/woff2")
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .body(data.to_vec())
                .expect("valid font response"),
            None => HttpResponse::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Vec::new())
                .expect("valid 404 response"),
        };
        responder.respond(resp);
    });

    rsx! {
        document::Style { "{STUDIO_CSS}" }
        document::Style {
            "@font-face {{ font-family: \"Space Grotesk\"; src: url(\"/studio-fonts/SpaceGrotesk-Variable.woff2\") format(\"woff2\"); font-weight: 300 700; font-style: normal; font-display: swap; }}
            @font-face {{ font-family: \"Commit Mono\"; src: url(\"/studio-fonts/CommitMono-Variable.woff2\") format(\"woff2\"); font-weight: 200 700; font-style: normal; font-display: swap; }}
            @font-face {{ font-family: \"Commit Mono\"; src: url(\"/studio-fonts/CommitMono-Variable.woff2\") format(\"woff2\"); font-weight: 200 700; font-style: italic; font-display: swap; }}
            html, body {{ background-color: var(--beet-canvas); color: var(--beet-text-primary); margin: 0; padding: 0; overflow: hidden; }}"
        }
    }
}
