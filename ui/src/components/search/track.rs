use dioxus::prelude::*;
use shared::metadata::Track;

use super::download_icon::{DownloadIcon, DownloadRowState};
use crate::{AcquireStageBadge, IconLoader, IconMusic, IconPlay, IconRefreshCw, IconSlidersHorizontal};

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub track: Track,
    pub on_album_click: Callback,
    pub on_search_sources: Callback,
    pub download_state: DownloadRowState,
    #[props(into)]
    pub on_download: EventHandler<()>,
    /// Live Acquire & Play stage badge for this track (when integrated with the Studio player).
    #[props(default)]
    pub acquiring_stage: Option<AcquireStageBadge>,
    /// Trigger 1-tap Acquire & Play for this catalog track.
    #[props(default)]
    pub on_acquire_and_play: Option<EventHandler<()>>,
    /// Open the manual choice modal when Acquire & Play needs a source pick.
    #[props(default)]
    pub on_pick_acquire_source: Option<EventHandler<()>>,
}

#[component]
pub fn TrackResult(props: Props) -> Element {
    let track = props.track.clone();
    let mut cover_error = use_signal(|| false);

    let cover_url = track
        .release_mbid
        .as_ref()
        .map(|mbid| format!("https://coverartarchive.org/release/{}/front-250", mbid));

    let in_flight = props
        .acquiring_stage
        .as_ref()
        .is_some_and(AcquireStageBadge::is_in_flight);
    let can_acquire = props.on_acquire_and_play.is_some();

    let on_art_click = {
        let stage = props.acquiring_stage.clone();
        let on_acquire = props.on_acquire_and_play;
        let on_pick = props.on_pick_acquire_source;
        let on_album_click = props.on_album_click;
        move |evt: MouseEvent| {
            evt.stop_propagation();
            match &stage {
                Some(s) if s.is_in_flight() => {}
                Some(AcquireStageBadge::NeedsManualChoice { .. }) => {
                    if let Some(cb) = on_pick {
                        cb.call(());
                    }
                }
                _ => {
                    if let Some(cb) = on_acquire {
                        cb.call(());
                    } else {
                        on_album_click.call(());
                    }
                }
            }
        }
    };

    rsx! {
      div {
        class: "relative bg-beet-surface hover:bg-beet-surface-high border border-beet-border-subtle hover:border-beet-border-default rounded-[12px] transition-colors duration-150 group flex items-center gap-3 px-3 py-2.5",

        // Cover art (with Acquire & Play click surface when enabled)
        button {
          r#type: "button",
          class: "relative w-11 h-11 flex-shrink-0 bg-beet-surface-low border border-beet-border-subtle rounded-[6px] flex items-center justify-center overflow-hidden cursor-pointer disabled:cursor-default",
          aria_label: if can_acquire {
            format!("Acquire and play {}", track.title)
          } else {
            format!("Inspect album for {}", track.title)
          },
          disabled: in_flight,
          onclick: on_art_click,
          if let Some(url) = cover_url.filter(|_| !cover_error()) {
            img {
              src: "{url}",
              alt: "",
              class: "w-full h-full object-cover",
              onerror: move |_| cover_error.set(true),
            }
          } else {
            IconMusic { size: 18, class: "text-beet-text-disabled".to_string() }
          }
          if can_acquire && !in_flight {
            div { class: "absolute inset-0 bg-beet-canvas/65 opacity-0 group-hover:opacity-100 transition-opacity flex items-center justify-center text-beet-leaf-text",
              IconPlay { size: 15, filled: true }
            }
          }
        }

        // Track info
        div { class: "flex-grow min-w-0",
          p { class: "text-[14px] font-semibold text-beet-text-primary truncate group-hover:text-beet-leaf-text transition-colors",
            "{track.title}"
          }
          p { class: "text-[13px] text-beet-text-secondary truncate", "{track.artist}" }
          if let Some(album_title) = &track.album_title {
            p {
              class: "text-[12px] text-beet-text-tertiary truncate cursor-pointer hover:text-beet-leaf-text transition-colors",
              onclick: move |_| props.on_album_click.call(()),
              "{album_title}"
            }
          }
        }

        // Duration
        if let Some(duration) = &track.duration {
          p { class: "text-[12px] tabular-nums-mono text-beet-text-tertiary whitespace-nowrap shrink-0 hidden sm:block",
            "{duration}"
          }
        }

        // Action buttons: Play / Acquire & Play · Search sources · Auto-download
        div { class: "flex items-center gap-1 shrink-0",
          if let Some(on_acquire) = props.on_acquire_and_play {
            match &props.acquiring_stage {
              Some(stage) if stage.is_in_flight() => rsx! {
                span {
                  class: "inline-flex items-center gap-1.5 h-8 px-2.5 rounded-full bg-beet-accent-surface border border-beet-accent/35 text-beet-accent-text tabular-nums-mono text-[10px] font-semibold uppercase tracking-wide shrink-0",
                  IconLoader { size: 12 }
                  span { "{stage.badge_label()}" }
                }
              },
              Some(AcquireStageBadge::NeedsManualChoice { message }) => {
                let tip = message.clone().unwrap_or_else(|| "Pick a source".to_string());
                let on_pick = props.on_pick_acquire_source;
                rsx! {
                  button {
                    r#type: "button",
                    title: "{tip}",
                    class: "inline-flex items-center gap-1.5 h-8 px-2.5 rounded-full bg-beet-accent-surface border border-beet-accent/40 text-beet-accent-text hover:bg-beet-accent-solid hover:text-beet-text-primary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wide transition-colors cursor-pointer shrink-0",
                    onclick: move |evt: MouseEvent| {
                      evt.stop_propagation();
                      if let Some(cb) = on_pick {
                        cb.call(());
                      }
                    },
                    IconSlidersHorizontal { size: 12 }
                    span { "Pick source" }
                  }
                }
              }
              Some(AcquireStageBadge::Failed { message }) => {
                let tip = message.clone().unwrap_or_else(|| "Retry Acquire & Play".to_string());
                rsx! {
                  button {
                    r#type: "button",
                    title: "{tip}",
                    class: "inline-flex items-center gap-1.5 h-8 px-2.5 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/35 text-beet-crimson-text hover:brightness-110 tabular-nums-mono text-[10px] font-semibold uppercase tracking-wide transition-colors cursor-pointer shrink-0",
                    onclick: move |evt: MouseEvent| {
                      evt.stop_propagation();
                      on_acquire.call(());
                    },
                    IconRefreshCw { size: 12 }
                    span { "Retry" }
                  }
                }
              }
              _ => rsx! {
                button {
                  r#type: "button",
                  aria_label: "Acquire and play",
                  title: "Acquire & Play",
                  class: "inline-flex items-center gap-1.5 h-8 px-2.5 rounded-full bg-beet-leaf-surface border border-beet-leaf/25 text-beet-leaf-text hover:bg-beet-leaf-solid hover:text-beet-leaf-ink text-[11px] font-semibold transition-colors cursor-pointer shrink-0",
                  onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    on_acquire.call(());
                  },
                  IconPlay { size: 11, filled: true }
                  span { class: "hidden sm:inline", "Play" }
                }
              },
            }
          }

          // Search sources button
          button {
            r#type: "button",
            class: "p-2 rounded-full hover:bg-beet-surface-highest transition-colors cursor-pointer group/src",
            aria_label: "Search sources",
            title: "Search sources",
            onclick: move |evt: MouseEvent| {
              evt.stop_propagation();
              props.on_search_sources.call(());
            },
            svg {
              class: "w-4 h-4 text-beet-text-secondary group-hover/src:text-beet-text-primary transition-colors",
              fill: "none",
              stroke: "currentColor",
              stroke_width: "1.75",
              view_box: "0 0 24 24",
              path {
                stroke_linecap: "round",
                stroke_linejoin: "round",
                d: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
              }
            }
          }

          // Auto-download button
          DownloadIcon {
            state: props.download_state.clone(),
            on_download: props.on_download,
          }
        }

        // Signature Soulbeet seam-light while Acquire & Play is active
        if in_flight {
          span {
            class: "beet-seam-light absolute inset-x-3 bottom-0 rounded-full",
            aria_hidden: "true",
          }
        }
      }
    }
}
