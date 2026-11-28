use dioxus::prelude::*;

#[derive(Clone, PartialEq, Debug, Default)]
pub enum DownloadRowState {
    #[default]
    Idle,
    Searching,
    Downloading,
    Done,
    Failed(String),
    Disabled,
}

#[derive(Props, PartialEq, Clone)]
pub struct DownloadIconProps {
    pub state: DownloadRowState,
    #[props(into)]
    pub on_download: EventHandler<()>,
}

#[component]
pub fn DownloadIcon(props: DownloadIconProps) -> Element {
    let state = &props.state;
    let on_download = props.on_download;
    let is_interactive = matches!(state, DownloadRowState::Idle | DownloadRowState::Failed(_));

    rsx! {
        div { class: "relative shrink-0",
            button {
                r#type: "button",
                class: "p-2 rounded-full hover:bg-beet-surface-highest transition-colors cursor-pointer group/dl disabled:cursor-not-allowed",
                aria_label: "Download",
                disabled: !is_interactive,
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    if is_interactive {
                        on_download.call(());
                    }
                },
                title: match state {
                    DownloadRowState::Disabled => "Configure your music library folder in Settings".to_string(),
                    DownloadRowState::Failed(msg) => msg.clone(),
                    _ => "Auto-download".to_string(),
                },

                match state {
                    DownloadRowState::Idle => rsx! {
                        svg {
                            class: "w-4 h-4 text-beet-text-secondary group-hover/dl:text-beet-leaf-text transition-colors",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "1.75",
                            view_box: "0 0 24 24",
                            path {
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                d: "M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4",
                            }
                        }
                    },
                    DownloadRowState::Searching | DownloadRowState::Downloading => rsx! {
                        div {
                            class: "w-4 h-4 animate-spin rounded-full border-2 border-beet-border-subtle border-t-beet-accent-text border-b-beet-leaf-text",
                        }
                    },
                    DownloadRowState::Done => rsx! {
                        svg {
                            class: "w-4 h-4 text-beet-leaf-text transition-opacity duration-500",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            view_box: "0 0 24 24",
                            path {
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                d: "M4.5 12.75l6 6 9-13.5",
                            }
                        }
                    },
                    DownloadRowState::Failed(_msg) => rsx! {
                        svg {
                            class: "w-4 h-4 text-beet-crimson-text",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "1.75",
                            view_box: "0 0 24 24",
                            path {
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                d: "M12 9v3.75m-9.303 3.376c-.866 1.5.217 3.374 1.948 3.374h14.71c1.73 0 2.813-1.874 1.948-3.374L13.949 3.378c-.866-1.5-3.032-1.5-3.898 0L2.697 16.126ZM12 15.75h.007v.008H12v-.008Z",
                            }
                        }
                    },
                    DownloadRowState::Disabled => rsx! {
                        svg {
                            class: "w-4 h-4 text-beet-text-disabled cursor-not-allowed",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "1.75",
                            view_box: "0 0 24 24",
                            path {
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                d: "M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4",
                            }
                        }
                    },
                }
            }
        }
    }
}
