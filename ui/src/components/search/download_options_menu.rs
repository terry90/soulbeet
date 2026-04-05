use api::models::folder::Folder;
use dioxus::prelude::*;

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() > max {
        format!("{}...", &name[..max])
    } else {
        name.to_string()
    }
}

#[derive(Props, PartialEq, Clone)]
pub struct FolderDropdownProps {
    pub folders: Vec<Folder>,
    pub selected_folder_id: Option<String>,
    #[props(into)]
    pub on_select_folder: EventHandler<Folder>,
    /// Alignment: "left" (default) or "right"
    #[props(default = "left".to_string())]
    pub align: String,
    /// Show quality override placeholder (for download icon context menu)
    #[props(default = false)]
    pub show_quality: bool,
}

#[component]
pub fn FolderDropdown(props: FolderDropdownProps) -> Element {
    let on_select_folder = props.on_select_folder;
    let folders = &props.folders;
    let selected_folder_id = &props.selected_folder_id;

    let align_class = if props.align == "right" {
        "absolute right-0 top-full mt-2 w-52 bg-beet-panel border border-white/10 rounded-lg shadow-2xl z-50 overflow-hidden"
    } else {
        "absolute left-0 top-full mt-2 w-52 bg-beet-panel border border-white/10 rounded-lg shadow-2xl z-50 overflow-hidden"
    };

    rsx! {
        div {
            class: "{align_class}",
            onclick: move |evt| evt.stop_propagation(),

            // Header
            div { class: "px-3 py-2 border-b border-white/5",
                p { class: "text-[10px] font-mono text-gray-500 uppercase tracking-widest", "Download to" }
            }

            // Folder list
            for folder in folders.iter() {
                {
                    let is_active = selected_folder_id.as_ref() == Some(&folder.id);
                    let folder_clone = folder.clone();
                    rsx! {
                        button {
                            class: if is_active {
                                "flex items-center gap-2 w-full text-left px-3 py-2.5 text-sm font-mono cursor-pointer bg-beet-leaf/10 text-beet-leaf"
                            } else {
                                "flex items-center gap-2 w-full text-left px-3 py-2.5 hover:bg-white/5 text-sm font-mono cursor-pointer text-gray-300"
                            },
                            onclick: move |_| on_select_folder.call(folder_clone.clone()),
                            if is_active {
                                svg {
                                    class: "w-3.5 h-3.5 shrink-0",
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
                            } else {
                                svg {
                                    class: "w-3.5 h-3.5 shrink-0 opacity-40",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: "1.5",
                                    view_box: "0 0 24 24",
                                    path {
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        d: "M2.25 12.75V12A2.25 2.25 0 014.5 9.75h15A2.25 2.25 0 0121.75 12v.75m-8.69-6.44l-2.12-2.12a1.5 1.5 0 00-1.061-.44H4.5A2.25 2.25 0 002.25 6v12a2.25 2.25 0 002.25 2.25h15A2.25 2.25 0 0021.75 18V9a2.25 2.25 0 00-2.25-2.25h-5.379a1.5 1.5 0 01-1.06-.44z",
                                    }
                                }
                            }
                            span { class: "truncate", "{truncate_name(&folder.name, 20)}" }
                        }
                    }
                }
            }

            // Quality override placeholder (D-02)
            if props.show_quality {
                div { class: "border-t border-white/5" }
                div { class: "px-3 py-2",
                    p { class: "text-[10px] font-mono text-gray-600 italic", "Quality: auto" }
                }
            }
        }
    }
}
