use dioxus::prelude::*;

/// The pixel-art beet mark, also served as the favicon.
pub const LOGO: Asset = asset!("/assets/logo.svg");

/// Soulbeet's logo. The art is a 16x16 pixel grid, so keep `size` a multiple
/// of 16: anything else puts its pixels between screen pixels and blurs them.
#[component]
pub fn Logo(size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        img {
            src: LOGO,
            width: "{size}",
            height: "{size}",
            alt: "Soulbeet",
            class: "{class}",
        }
    }
}
