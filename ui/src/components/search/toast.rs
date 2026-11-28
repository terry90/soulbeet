use super::SourceChoices;
use dioxus::prelude::*;
use shared::download::{DownloadableGroup, ManualReason};

#[derive(Clone, PartialEq, Debug)]
pub struct FallbackToastData {
    pub id: String,
    pub track_name: String,
    /// The stored search `results` came from.
    pub search_id: String,
    pub results: Vec<DownloadableGroup>,
    /// Why no source was picked on its own.
    pub reason: ManualReason,
}

/// What the toast says of `track_name` for `reason`.
fn fallback_message(track_name: &str, reason: &ManualReason) -> String {
    match reason {
        ManualReason::NoConfidentMatch => format!("No confident match for {track_name}."),
        ManualReason::LossyOnly { backend } => format!(
            "Only {} has {track_name}, and its files are lossy.",
            shared::backends::name(backend)
        ),
    }
}

#[derive(Props, PartialEq, Clone)]
pub struct FallbackToastProps {
    pub toast: FallbackToastData,
    #[props(into)]
    pub on_pick_source: EventHandler<SourceChoices>,
    #[props(into)]
    pub on_dismiss: EventHandler<String>,
}

#[component]
pub fn FallbackToast(props: FallbackToastProps) -> Element {
    let toast = &props.toast;
    let on_pick_source = props.on_pick_source;
    let on_dismiss = props.on_dismiss;
    let toast_id = toast.id.clone();
    let toast_id_pick = toast.id.clone();
    let message = fallback_message(&toast.track_name, &toast.reason);
    let choices = SourceChoices::single(toast.search_id.clone(), toast.results.clone(), None);

    rsx! {
        div { class: "bg-beet-surface-high border border-beet-border-default rounded-[12px] shadow-xl p-4 relative",
            // Close button
            button {
                r#type: "button",
                aria_label: "Dismiss notice",
                class: "absolute top-2.5 right-2.5 text-beet-text-tertiary hover:text-beet-text-primary transition-colors cursor-pointer p-1 rounded-[6px]",
                onclick: move |_| {
                    on_dismiss.call(toast_id.clone());
                },
                svg {
                    class: "w-4 h-4",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "1.75",
                    view_box: "0 0 24 24",
                    path {
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        d: "M6 18L18 6M6 6l12 12",
                    }
                }
            }

            // Line 1: fallback message
            p { class: "text-[13px] text-beet-text-primary pr-6 leading-snug",
                "{message}"
            }

            // Line 2: pick source link
            button {
                r#type: "button",
                class: "text-[12px] text-beet-leaf-text font-medium cursor-pointer hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-beet-leaf-solid rounded mt-1.5",
                onclick: move |_| {
                    on_pick_source.call(choices.clone());
                    on_dismiss.call(toast_id_pick.clone());
                },
                "Pick a source manually"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::backends::YTDLP;

    fn render(reason: ManualReason) -> String {
        let toast = FallbackToastData {
            id: "b1".into(),
            track_name: "Morse Bloom".into(),
            search_id: "s1".into(),
            results: vec![],
            reason,
        };
        let mut dom = VirtualDom::new_with_props(
            |toast: FallbackToastData| {
                rsx! {
                    FallbackToast { toast, on_pick_source: |_| {}, on_dismiss: |_| {} }
                }
            },
            toast,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_toast_says_no_confident_match_when_nothing_scored_well_enough() {
        let html = render(ManualReason::NoConfidentMatch);
        assert!(
            html.contains(">No confident match for Morse Bloom.</p>"),
            "{html}"
        );
        assert!(html.contains(">Pick a source manually</button>"), "{html}");
    }

    #[test]
    fn the_toast_names_the_lossy_backend_when_only_it_has_a_confident_match() {
        let html = render(ManualReason::LossyOnly {
            backend: YTDLP.id.into(),
        });
        let said = format!(
            ">Only {} has Morse Bloom, and its files are lossy.</p>",
            YTDLP.name
        );
        assert!(html.contains(&said), "{html}");
        assert!(!html.contains("No confident match"), "{html}");
        assert!(html.contains(">Pick a source manually</button>"), "{html}");
    }
}
