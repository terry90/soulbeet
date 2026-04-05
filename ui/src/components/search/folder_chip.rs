use api::models::folder::Folder;
use dioxus::prelude::*;

use super::download_options_menu::FolderDropdown;

#[derive(Props, PartialEq, Clone)]
pub struct FolderChipProps {
    pub folders: Vec<Folder>,
    pub selected_folder_id: Signal<Option<String>>,
    pub on_folder_change: EventHandler<Folder>,
    pub active_menu: Signal<Option<String>>,
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() > max {
        format!("{}...", &name[..max])
    } else {
        name.to_string()
    }
}

const MENU_ID: &str = "__folder_chip__";

#[component]
pub fn FolderChip(props: FolderChipProps) -> Element {
    let folders = &props.folders;
    let selected_folder_id = props.selected_folder_id;
    let on_folder_change = props.on_folder_change;
    let mut active_menu = props.active_menu;

    let is_open = active_menu().as_deref() == Some(MENU_ID);

    let selected_folder: Option<Folder> = selected_folder_id()
        .as_ref()
        .and_then(|fid| folders.iter().find(|f| f.id == *fid).cloned());

    let (chip_text, needs_attention) = if let Some(ref folder) = selected_folder {
        (truncate_name(&folder.name, 14), false)
    } else {
        ("Select folder".to_string(), true)
    };

    let chip_class = if is_open {
        "flex items-center gap-1.5 px-2.5 py-1.5 text-xs font-mono rounded-md transition-all duration-200 cursor-pointer bg-white/10 border border-white/10 text-white mr-2"
    } else if needs_attention {
        "flex items-center gap-1.5 px-2.5 py-1.5 text-xs font-mono rounded-md transition-all duration-200 cursor-pointer bg-white/5 border border-yellow-500/30 text-yellow-400 hover:bg-white/10 hover:text-yellow-300 mr-2"
    } else {
        "flex items-center gap-1.5 px-2.5 py-1.5 text-xs font-mono rounded-md transition-all duration-200 cursor-pointer bg-white/5 border border-white/5 text-gray-300 hover:bg-white/10 hover:text-white mr-2"
    };

    rsx! {
        div { class: "relative",
            button {
                class: "{chip_class}",
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    if is_open {
                        active_menu.set(None);
                    } else {
                        active_menu.set(Some(MENU_ID.to_string()));
                    }
                },
                svg {
                    class: "w-3.5 h-3.5 opacity-70",
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
                "{chip_text}"
            }

            if is_open {
                FolderDropdown {
                    folders: props.folders.clone(),
                    selected_folder_id: selected_folder_id().clone(),
                    on_select_folder: move |folder: Folder| {
                        active_menu.set(None);
                        on_folder_change.call(folder);
                    },
                    align: "left",
                }
            }
        }
    }
}
