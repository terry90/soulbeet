use api::models::folder::Folder;
use dioxus::prelude::*;

use super::download_options_menu::FolderDropdown;
use crate::Chevron;

#[derive(Props, PartialEq, Clone)]
pub struct FolderChipProps {
    pub folders: Vec<Folder>,
    pub selected_folder_id: Signal<Option<String>>,
    pub on_folder_change: EventHandler<Folder>,
    pub active_menu: Signal<Option<String>>,
}

const MENU_ID: &str = "__folder_chip__";

/// The folder downloads go to, which opens a menu to pick another. It sits
/// at the right end of a row, and a long name ends in an ellipsis.
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

    let (chip_text, needs_attention) = match selected_folder {
        Some(folder) => (folder.name, false),
        None => ("Select folder".to_string(), true),
    };

    let pill_class = if is_open {
        "bg-white/10 border-white/10 text-white"
    } else if needs_attention {
        "bg-white/5 border-yellow-500/30 text-yellow-400 group-hover/chip:bg-white/10 group-hover/chip:text-yellow-300"
    } else {
        "bg-white/5 border-white/5 text-gray-300 group-hover/chip:bg-white/10 group-hover/chip:text-white"
    };

    rsx! {
        div { class: "relative ml-auto flex min-w-0",
            button {
                class: "group/chip flex items-center min-w-0 max-w-full h-[44px] px-[2px] font-mono text-[12px] cursor-pointer focus-visible:outline-none",
                r#type: "button",
                title: "{chip_text}",
                aria_expanded: is_open,
                onclick: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    if is_open {
                        active_menu.set(None);
                    } else {
                        active_menu.set(Some(MENU_ID.to_string()));
                    }
                },
                span { class: "flex items-center gap-[6px] min-w-0 px-[8px] py-[5px] rounded-md border transition-colors duration-200 group-focus-visible/chip:outline-2 group-focus-visible/chip:outline-offset-1 group-focus-visible/chip:outline-beet-leaf {pill_class}",
                    svg {
                        class: "size-[14px] flex-none opacity-70",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "1.5",
                        view_box: "0 0 24 24",
                        "aria-hidden": "true",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            d: "M2.25 12.75V12A2.25 2.25 0 014.5 9.75h15A2.25 2.25 0 0121.75 12v.75m-8.69-6.44l-2.12-2.12a1.5 1.5 0 00-1.061-.44H4.5A2.25 2.25 0 002.25 6v12a2.25 2.25 0 002.25 2.25h15A2.25 2.25 0 0021.75 18V9a2.25 2.25 0 00-2.25-2.25h-5.379a1.5 1.5 0 01-1.06-.44z",
                        }
                    }
                    span { class: "sr-only", "Download folder: " }
                    span { class: "min-w-0 truncate", "{chip_text}" }
                    // The narrowest phones give its room to the name.
                    Chevron { class: "opacity-60 max-[399px]:hidden" }
                }
            }

            if is_open {
                FolderDropdown {
                    folders: props.folders.clone(),
                    selected_folder_id: selected_folder_id().clone(),
                    on_select_folder: move |folder: Folder| {
                        active_menu.set(None);
                        on_folder_change.call(folder);
                    },
                    align: "right",
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::opening_tag;

    pub(crate) fn folder(id: &str, name: &str) -> Folder {
        Folder {
            id: id.to_string(),
            user_id: "u".to_string(),
            name: name.to_string(),
            path: format!("/music/{id}"),
            shared: false,
        }
    }

    #[test]
    fn a_long_name_is_whole_in_the_page_and_names_the_chip() {
        let mut dom = VirtualDom::new(|| {
            let selected_folder_id = use_signal(|| Some("b".to_string()));
            let active_menu = use_signal(|| None);
            rsx! {
                FolderChip {
                    folders: vec![folder("a", "Vinyl rips"), folder("b", "Lossless archive from the old NAS")],
                    selected_folder_id,
                    on_folder_change: |_| {},
                    active_menu,
                }
            }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        // Cut short by the layout alone, with an ellipsis.
        assert!(
            html.contains(
                "<span class=\"min-w-0 truncate\">Lossless archive from the old NAS</span>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"sr-only\">Download folder: </span>"),
            "{html}"
        );
        let button = opening_tag(&html, "group/chip");
        assert!(button.contains("type=\"button\""), "{button}");
        assert!(button.contains("aria-expanded=false"), "{button}");
        assert!(button.contains("h-[44px]"), "{button}");
    }
}
