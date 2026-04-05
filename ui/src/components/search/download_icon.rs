use api::models::folder::Folder;
use dioxus::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::download_options_menu::FolderDropdown;

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
    pub item_id: String,
    pub state: DownloadRowState,
    pub folders: Vec<Folder>,
    pub selected_folder_id: Option<String>,
    /// Global signal: which item_id has its menu open (None = all closed)
    pub active_menu: Signal<Option<String>>,
    #[props(into)]
    pub on_download: EventHandler<()>,
    #[props(into)]
    pub on_override_download: EventHandler<Folder>,
}

#[component]
pub fn DownloadIcon(props: DownloadIconProps) -> Element {
    let state = &props.state;
    let on_download = props.on_download;
    let on_override_download = props.on_override_download;
    let mut active_menu = props.active_menu;
    let item_id = props.item_id.clone();

    let mut timer_id = use_signal(|| None::<i32>);
    let mut suppress_click = use_signal(|| false);

    let is_interactive = matches!(state, DownloadRowState::Idle | DownloadRowState::Failed(_));
    let is_menu_open = active_menu().as_ref() == Some(&item_id);

    let item_id_open = item_id.clone();
    let item_id_toggle = item_id.clone();

    let mut cancel_timer = move || {
        if let Some(id) = timer_id() {
            if let Some(window) = web_sys::window() {
                let _ = window.clear_timeout_with_handle(id);
            }
            timer_id.set(None);
        }
    };

    // Wrapper z-index: high when this row's menu is open so it paints above other rows
    let wrapper_class = if is_menu_open {
        "relative shrink-0 z-[60]"
    } else {
        "relative shrink-0"
    };

    rsx! {
        div { class: "{wrapper_class}",
            button {
                class: "p-2 rounded-full hover:bg-white/10 transition-colors cursor-pointer group/dl",
                disabled: !is_interactive,
                onpointerdown: move |evt: PointerEvent| {
                    evt.stop_propagation();
                    if !is_interactive {
                        return;
                    }
                    cancel_timer();
                    let id_for_timer = item_id_open.clone();
                    if let Some(window) = web_sys::window() {
                        let cb = Closure::once(move || {
                            suppress_click.set(true);
                            active_menu.set(Some(id_for_timer));
                        });
                        if let Ok(id) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                            cb.as_ref().unchecked_ref(),
                            500,
                        ) {
                            cb.forget();
                            timer_id.set(Some(id));
                        }
                    }
                },
                onpointerup: move |_| {
                    cancel_timer();
                },
                onpointerleave: move |_| {
                    cancel_timer();
                },
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    if suppress_click() {
                        suppress_click.set(false);
                        return;
                    }
                    if is_interactive {
                        on_download.call(());
                    }
                },
                oncontextmenu: move |evt: MouseEvent| {
                    evt.prevent_default();
                    evt.stop_propagation();
                    if !is_interactive {
                        return;
                    }
                    if is_menu_open {
                        active_menu.set(None);
                    } else {
                        active_menu.set(Some(item_id_toggle.clone()));
                    }
                },
                title: if matches!(state, DownloadRowState::Disabled) { "Configure a folder in Settings" } else { "" },

                match state {
                    DownloadRowState::Idle => rsx! {
                        svg {
                            class: "w-5 h-5 text-gray-400 group-hover/dl:text-beet-accent transition-colors",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
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
                            class: "w-5 h-5 animate-spin rounded-full border-2 border-transparent border-t-beet-accent border-b-beet-accent",
                        }
                    },
                    DownloadRowState::Done => rsx! {
                        svg {
                            class: "w-5 h-5 text-beet-leaf transition-opacity duration-500",
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
                            class: "w-5 h-5 text-red-400",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
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
                            class: "w-5 h-5 text-gray-700 cursor-not-allowed",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
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

            if is_menu_open {
                FolderDropdown {
                    folders: props.folders.clone(),
                    selected_folder_id: props.selected_folder_id.clone(),
                    on_select_folder: move |folder: Folder| {
                        active_menu.set(None);
                        on_override_download.call(folder);
                    },
                    align: "right",
                    show_quality: true,
                }
            }
        }
    }
}
