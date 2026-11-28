//! Reusable Studio artwork components with Midnight Studio fallback tiles
//! and procedural Mix Sleeve rendering for curated Soulbeet playlists.

use dioxus::prelude::*;

use crate::{
    mix_sleeve_theme, render_mix_sleeve_art, IconDisc3, IconListMusic, IconMusic, IconUser,
};

/// Semantic entity type controlling the icon shown inside [`CoverImage`]'s
/// Midnight Studio fallback tile when artwork is absent or fails to load.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArtFallbackKind {
    Album,
    Artist,
    Track,
    Playlist,
}

/// Resilient cover-art image with automatic `onerror` recovery and Midnight Studio
/// fallback artwork. Automatically retries whenever `src` changes to a new URL.
#[component]
pub fn CoverImage(
    src: Option<String>,
    alt: String,
    #[props(default = "w-full h-full object-cover".to_string())] img_class: String,
    #[props(default = "w-full h-full".to_string())] fallback_class: String,
    #[props(default = ArtFallbackKind::Album)] kind: ArtFallbackKind,
    #[props(default = 24)] icon_size: u32,
    #[props(default = false)] rounded_full: bool,
) -> Element {
    let mut failed_src = use_signal(|| None::<String>);
    let clean_src = src
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);

    let show_img = clean_src
        .as_ref()
        .filter(|url| failed_src.read().as_deref() != Some(url.as_str()));

    if let Some(url) = show_img {
        let url_for_err = url.clone();
        rsx! {
            img {
                src: "{url}",
                alt: "{alt}",
                class: "{img_class}",
                loading: "lazy",
                decoding: "async",
                onload: move |_| {
                    let had_failure = failed_src.read().is_some();
                    if had_failure {
                        failed_src.set(None);
                    }
                },
                onerror: move |_| failed_src.set(Some(url_for_err.clone())),
            }
        }
    } else {
        let shape_cls = if rounded_full { "rounded-full" } else { "" };
        let icon_cls = "relative text-beet-text-tertiary/70".to_string();
        rsx! {
            div {
                class: "relative flex items-center justify-center overflow-hidden bg-gradient-to-br from-beet-panel via-[#191627] to-[#110f1a] {shape_cls} {fallback_class}",
                "aria-hidden": "true",
                div { class: "pointer-events-none absolute inset-0 bg-[radial-gradient(circle_at_30%_25%,rgba(168,85,247,0.16),transparent_65%)]" }
                match kind {
                    ArtFallbackKind::Album => rsx! { IconDisc3 { size: icon_size, class: icon_cls } },
                    ArtFallbackKind::Artist => rsx! { IconUser { size: icon_size, class: icon_cls } },
                    ArtFallbackKind::Track => rsx! { IconMusic { size: icon_size, class: icon_cls } },
                    ArtFallbackKind::Playlist => rsx! { IconListMusic { size: icon_size, class: icon_cls } },
                }
            }
        }
    }
}

/// Renders either a bespoke Midnight Studio Mix Sleeve (for curated Soulbeet mixes)
/// or real Navidrome playlist cover art via [`CoverImage`] (for smart & user playlists).
#[component]
pub fn PlaylistArtwork(
    mix_slug: Option<String>,
    cover_art_url: Option<String>,
    alt: String,
    svg_id_prefix: String,
    #[props(default = 20)] icon_size: u32,
) -> Element {
    if let Some(slug) = mix_slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let theme = mix_sleeve_theme(slug);
        rsx! {
            div {
                class: "relative w-full h-full overflow-hidden flex items-center justify-center",
                style: "background: {theme.background};",
                {render_mix_sleeve_art(slug, &svg_id_prefix)}
            }
        }
    } else {
        rsx! {
            CoverImage {
                src: cover_art_url,
                alt,
                kind: ArtFallbackKind::Playlist,
                icon_size,
            }
        }
    }
}
