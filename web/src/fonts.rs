//! The app's two typefaces, served from the bundle so no page asks a font
//! host for them.
//!
//! The faces are declared here and not in tailwind.css: dx copies a
//! stylesheet's url() as written, and only asset!() knows the hashed file
//! name that lets the server cache a font for a year.

use dioxus::prelude::*;

/// Space Grotesk, weights 300 to 700 (assets/fonts/SpaceGrotesk-OFL.txt).
const SPACE_GROTESK: Asset = asset!("/assets/fonts/SpaceGrotesk-Variable.woff2");
/// Commit Mono, weights 200 to 700, upright and italic
/// (assets/fonts/CommitMono-OFL.txt).
const COMMIT_MONO: Asset = asset!("/assets/fonts/CommitMono-Variable.woff2");

/// Preloads both fonts and declares their faces.
///
/// Both are on the first screen of every page, so they load alongside the
/// stylesheet and are usually in before the first paint. `swap` shows the
/// fallback when they are not, so text is never hidden, and the page takes
/// the font as soon as it arrives.
///
/// Commit Mono's italics are on an axis of the same file: the italic face
/// gives the true italic where a browser would slant the upright.
#[component]
pub fn Fonts() -> Element {
    rsx! {
        document::Link {
            rel: "preload",
            href: SPACE_GROTESK,
            r#as: "font",
            r#type: "font/woff2",
            crossorigin: "anonymous",
        }
        document::Link {
            rel: "preload",
            href: COMMIT_MONO,
            r#as: "font",
            r#type: "font/woff2",
            crossorigin: "anonymous",
        }
        document::Style {
            "@font-face {{ font-family: \"Space Grotesk\"; src: url(\"{SPACE_GROTESK}\") format(\"woff2\"); font-weight: 300 700; font-style: normal; font-display: swap; }}
            @font-face {{ font-family: \"Commit Mono\"; src: url(\"{COMMIT_MONO}\") format(\"woff2\"); font-weight: 200 700; font-style: normal; font-display: swap; }}
            @font-face {{ font-family: \"Commit Mono\"; src: url(\"{COMMIT_MONO}\") format(\"woff2\"); font-weight: 200 700; font-style: italic; font-display: swap; }}"
        }
    }
}
