use dioxus::prelude::*;
use shared::download::DownloadableGroup;

#[derive(Clone, PartialEq, Debug)]
pub struct FallbackToastData {
    pub id: String,
    pub track_name: String,
    pub results: Vec<DownloadableGroup>,
}

#[derive(Props, PartialEq, Clone)]
pub struct FallbackToastProps {
    pub toast: FallbackToastData,
    #[props(into)]
    pub on_pick_source: EventHandler<Vec<DownloadableGroup>>,
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
    let track_name = toast.track_name.clone();
    let results = toast.results.clone();

    rsx! {
        div { class: "bg-beet-panel border border-white/10 rounded-lg shadow-2xl p-4 relative",
            // Close button
            button {
                class: "absolute top-2 right-2 text-gray-500 hover:text-white cursor-pointer p-1",
                onclick: move |_| {
                    on_dismiss.call(toast_id.clone());
                },
                svg {
                    class: "w-4 h-4",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    view_box: "0 0 24 24",
                    path {
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        d: "M6 18L18 6M6 6l12 12",
                    }
                }
            }

            // Line 1: fallback message
            p { class: "text-sm text-gray-300 font-mono pr-6",
                "No confident match for {track_name}."
            }

            // Line 2: pick source link
            p {
                class: "text-xs text-beet-leaf font-mono cursor-pointer hover:underline mt-1",
                onclick: move |_| {
                    on_pick_source.call(results.clone());
                    on_dismiss.call(toast_id_pick.clone());
                },
                "Pick a source manually"
            }
        }
    }
}
