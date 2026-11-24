use dioxus::prelude::*;

/// The colour of the glow at the top left of a page. The one at the bottom
/// right is always the leaf green.
#[derive(Clone, Copy, PartialEq)]
pub enum GlowColor {
    Accent,
    Blue,
}

/// How far a glow reaches past its 16rem disc.
#[derive(Clone, Copy, PartialEq)]
pub enum GlowSpread {
    Wide,
    Tight,
}

/// The two soft discs of colour behind a page. They are painted as
/// gradients (`.glow` in input.css), never with a CSS blur filter: Safari on
/// iOS 26 stalls for seconds on a large blur over fixed elements.
#[component]
pub fn BackgroundGlows(left: GlowColor, spread: GlowSpread) -> Element {
    let left = match left {
        GlowColor::Accent => "[--glow-color:var(--color-beet-accent)]",
        GlowColor::Blue => "[--glow-color:var(--color-blue-500)]",
    };
    let spread = match spread {
        GlowSpread::Wide => "glow-wide",
        GlowSpread::Tight => "glow-tight",
    };

    rsx! {
        div { class: "glow glow-top-left {spread} {left}" }
        div { class: "glow glow-bottom-right {spread} [--glow-color:var(--color-beet-leaf)]" }
    }
}
