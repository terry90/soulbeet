//! Crisp Lucide-geometry inline SVG icon set for Soulbeet's Refined Midnight Studio UI.
//!
//! All icons use a 24x24 viewBox, uniform `1.75` stroke width, rounded caps/joins,
//! `stroke="currentColor"`, and `aria-hidden="true"` by default (parent buttons supply
//! accessible labels).

use dioxus::prelude::*;

// ============================================================================
// Navigation Icons
// ============================================================================

#[component]
pub fn IconHome(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M15 21v-8a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v8" }
            path { d: "M3 10a2 2 0 0 1 .709-1.528l7-5.999a2 2 0 0 1 2.582 0l7 5.999A2 2 0 0 1 21 10v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" }
        }
    }
}

#[component]
pub fn IconCompass(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            circle { cx: "12", cy: "12", r: "10" }
            polygon { points: "16.24 7.76 14.12 14.12 7.76 16.24 9.88 9.88 16.24 7.76" }
        }
    }
}

#[component]
pub fn IconSearch(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            circle { cx: "11", cy: "11", r: "8" }
            path { d: "m21 21-4.3-4.3" }
        }
    }
}

#[component]
pub fn IconLibrary(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "m16 6 4 14" }
            path { d: "M12 6v14" }
            path { d: "M8 8v12" }
            path { d: "M4 4v16" }
        }
    }
}

#[component]
pub fn IconActivity(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2" }
        }
    }
}

#[component]
pub fn IconSettings(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" }
            circle { cx: "12", cy: "12", r: "3" }
        }
    }
}

#[component]
pub fn IconSparkles(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M9.937 15.5A2 2 0 0 0 8.5 14.063l-6.135-1.582a.5.5 0 0 1 0-.962L8.5 9.936A2 2 0 0 0 9.937 8.5l1.582-6.135a.5.5 0 0 1 .963 0L14.063 8.5A2 2 0 0 0 15.5 9.937l6.135 1.581a.5.5 0 0 1 0 .964L15.5 14.063a2 2 0 0 0-1.437 1.437l-1.582 6.135a.5.5 0 0 1-.963 0z" }
            path { d: "M20 3v4" }
            path { d: "M22 5h-4" }
            path { d: "M4 17v2" }
            path { d: "M5 18H3" }
        }
    }
}

#[component]
pub fn IconRadio(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M4.9 19.1C1 15.2 1 8.8 4.9 4.9" }
            path { d: "M7.8 16.2c-2.3-2.3-2.3-6.1 0-8.5" }
            circle { cx: "12", cy: "12", r: "2" }
            path { d: "M16.2 7.8c2.3 2.3 2.3 6.1 0 8.5" }
            path { d: "M19.1 4.9C23 8.8 23 15.1 19.1 19" }
        }
    }
}

// ============================================================================
// Playback Icons
// ============================================================================

#[component]
pub fn IconPlay(
    #[props(default = 18)] size: u32,
    #[props(default)] filled: bool,
    #[props(default)] class: String,
) -> Element {
    let fill = if filled { "currentColor" } else { "none" };
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "{fill}",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            polygon { points: "6 3 20 12 6 21 6 3" }
        }
    }
}

#[component]
pub fn IconPause(
    #[props(default = 18)] size: u32,
    #[props(default)] filled: bool,
    #[props(default)] class: String,
) -> Element {
    let fill = if filled { "currentColor" } else { "none" };
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "{fill}",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            rect { x: "14", y: "4", width: "4", height: "16", rx: "1" }
            rect { x: "6", y: "4", width: "4", height: "16", rx: "1" }
        }
    }
}

#[component]
pub fn IconSkipBack(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            polygon { points: "19 20 9 12 19 4 19 20" }
            line { x1: "5", x2: "5", y1: "19", y2: "5" }
        }
    }
}

#[component]
pub fn IconSkipForward(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            polygon { points: "5 4 15 12 5 20 5 4" }
            line { x1: "19", x2: "19", y1: "5", y2: "19" }
        }
    }
}

#[component]
pub fn IconShuffle(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M2 18h1.4c1.3 0 2.5-.6 3.3-1.7l6.1-8.6c.7-1.1 2-1.7 3.3-1.7H22" }
            path { d: "m18 2 4 4-4 4" }
            path { d: "M2 6h1.9c1.5 0 2.9.9 3.6 2.2" }
            path { d: "M22 18h-5.9c-1.3 0-2.6-.7-3.3-1.8l-.5-.8" }
            path { d: "m18 14 4 4-4 4" }
        }
    }
}

#[component]
pub fn IconRepeat(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "m17 2 4 4-4 4" }
            path { d: "M3 11v-1a4 4 0 0 1 4-4h14" }
            path { d: "m7 22-4-4 4-4" }
            path { d: "M21 13v1a4 4 0 0 1-4 4H3" }
        }
    }
}

#[component]
pub fn IconRepeatOne(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "m17 2 4 4-4 4" }
            path { d: "M3 11v-1a4 4 0 0 1 4-4h14" }
            path { d: "m7 22-4-4 4-4" }
            path { d: "M21 13v1a4 4 0 0 1-4 4H3" }
            path { d: "M11 10h1v4" }
        }
    }
}

#[component]
pub fn IconVolume2(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z" }
            path { d: "M16 9a5 5 0 0 1 0 6" }
            path { d: "M19.364 18.364a9 9 0 0 0 0-12.728" }
        }
    }
}

#[component]
pub fn IconVolumeX(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z" }
            line { x1: "22", x2: "16", y1: "9", y2: "15" }
            line { x1: "16", x2: "22", y1: "9", y2: "15" }
        }
    }
}

#[component]
pub fn IconListMusic(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M21 15V6" }
            path { d: "M18.5 18a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Z" }
            path { d: "M12 12H3" }
            path { d: "M16 6H3" }
            path { d: "M12 18H3" }
        }
    }
}

#[component]
pub fn IconMic2(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "m12 8-9.04 9.06a2.82 2.82 0 1 0 3.98 3.98L16 12" }
            circle { cx: "17", cy: "7", r: "5" }
        }
    }
}

#[component]
pub fn IconInfo(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            circle { cx: "12", cy: "12", r: "10" }
            path { d: "M12 16v-4" }
            path { d: "M12 8h.01" }
        }
    }
}

#[component]
pub fn IconLaptop(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M20 16V7a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v9m16 0H4m16 0 1.28 2.55a1 1 0 0 1-.9 1.45H3.62a1 1 0 0 1-.9-1.45L4 16" }
        }
    }
}

#[component]
pub fn IconMinimize2(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            polyline { points: "4 14 10 14 10 20" }
            polyline { points: "20 10 14 10 14 4" }
            line { x1: "14", x2: "21", y1: "10", y2: "3" }
            line { x1: "3", x2: "10", y1: "21", y2: "14" }
        }
    }
}

#[component]
pub fn IconPictureInPicture2(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M21 9V6a2 2 0 0 0-2-2H4a2 2 0 0 0-2 2v10c0 1.1.9 2 2 2h4" }
            rect { width: "10", height: "7", x: "12", y: "13", rx: "2" }
        }
    }
}

// ============================================================================
// Curation & Status Icons
// ============================================================================

#[component]
pub fn IconStar(
    #[props(default = 18)] size: u32,
    #[props(default)] filled: bool,
    #[props(default)] class: String,
) -> Element {
    let fill = if filled { "currentColor" } else { "none" };
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "{fill}",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            polygon { points: "12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2" }
        }
    }
}

#[component]
pub fn IconHeart(
    #[props(default = 18)] size: u32,
    #[props(default)] filled: bool,
    #[props(default)] class: String,
) -> Element {
    let fill = if filled { "currentColor" } else { "none" };
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "{fill}",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M19 14c1.49-1.46 3-3.21 3-5.5A5.5 5.5 0 0 0 16.5 3c-1.76 0-3 .5-4.5 2-1.5-1.5-2.74-2-4.5-2A5.5 5.5 0 0 0 2 8.5c0 2.3 1.5 4.05 3 5.5l7 7Z" }
        }
    }
}

#[component]
pub fn IconCheck(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M20 6 9 17l-5-5" }
        }
    }
}

#[component]
pub fn IconThumbsDown(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M17 14V2" }
            path { d: "M9 18.12 10 14H4.17a2 2 0 0 1-1.92-2.56l2.33-8A2 2 0 0 1 6.5 2H20a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2h-2.76a2 2 0 0 0-1.79 1.11L12 22a3.13 3.13 0 0 1-3-3.88Z" }
        }
    }
}

#[component]
pub fn IconX(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M18 6 6 18" }
            path { d: "m6 6 12 12" }
        }
    }
}

#[component]
pub fn IconTrash2(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M3 6h18" }
            path { d: "M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6" }
            path { d: "M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2" }
            line { x1: "10", x2: "10", y1: "11", y2: "17" }
            line { x1: "14", x2: "14", y1: "11", y2: "17" }
        }
    }
}

#[component]
pub fn IconDownload(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }
            polyline { points: "7 10 12 15 17 10" }
            line { x1: "12", x2: "12", y1: "15", y2: "3" }
        }
    }
}

#[component]
pub fn IconWifiOff(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M12 20h.01" }
            path { d: "M8.5 16.429a5 5 0 0 1 7 0" }
            path { d: "M5 12.859a10 10 0 0 1 5.17-2.69" }
            path { d: "M19 12.859a10 10 0 0 0-2.007-1.523" }
            path { d: "M2 8.82a15 15 0 0 1 4.177-2.643" }
            path { d: "M22 8.82a15 15 0 0 0-11.288-3.764" }
            path { d: "m2 2 20 20" }
        }
    }
}

#[component]
pub fn IconCloudCheck(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M4 14.899A7 7 0 1 1 15.71 8h1.79a4.5 4.5 0 0 1 2.5 8.242" }
            path { d: "m9 15 2 2 4-4" }
        }
    }
}

#[component]
pub fn IconShieldAlert(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" }
            path { d: "M12 8v4" }
            path { d: "M12 16h.01" }
        }
    }
}

#[component]
pub fn IconSlidersHorizontal(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            line { x1: "21", x2: "14", y1: "4", y2: "4" }
            line { x1: "10", x2: "3", y1: "4", y2: "4" }
            line { x1: "21", x2: "12", y1: "12", y2: "12" }
            line { x1: "8", x2: "3", y1: "12", y2: "12" }
            line { x1: "21", x2: "16", y1: "20", y2: "20" }
            line { x1: "12", x2: "3", y1: "20", y2: "20" }
            line { x1: "14", x2: "14", y1: "2", y2: "6" }
            line { x1: "8", x2: "8", y1: "10", y2: "14" }
            line { x1: "16", x2: "16", y1: "18", y2: "22" }
        }
    }
}

#[component]
pub fn IconChevronRight(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "m9 18 6-6-6-6" }
        }
    }
}

#[component]
pub fn IconChevronLeft(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "m15 18-6-6 6-6" }
        }
    }
}

#[component]
pub fn IconPlus(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M5 12h14" }
            path { d: "M12 5v14" }
        }
    }
}

#[component]
pub fn IconRefreshCw(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" }
            path { d: "M21 3v5h-5" }
            path { d: "M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" }
            path { d: "M8 16H3v5" }
        }
    }
}

#[component]
pub fn IconLoader(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 animate-spin motion-reduce:animate-none {class}",
            path { d: "M21 12a9 9 0 1 1-6.219-8.56" }
        }
    }
}

#[component]
pub fn IconMusic(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M9 18V5l12-2v13" }
            circle { cx: "6", cy: "18", r: "3" }
            circle { cx: "18", cy: "16", r: "3" }
        }
    }
}

#[component]
pub fn IconCommand(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M15 6v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3" }
        }
    }
}

#[component]
pub fn IconLogOut(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" }
            polyline { points: "16 17 21 12 16 7" }
            line { x1: "21", x2: "9", y1: "12", y2: "12" }
        }
    }
}

#[component]
pub fn IconSmartphone(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            rect { width: "14", height: "20", x: "5", y: "2", rx: "2", ry: "2" }
            path { d: "M12 18h.01" }
        }
    }
}

#[component]
pub fn IconMonitor(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            rect { width: "20", height: "14", x: "2", y: "3", rx: "2" }
            line { x1: "8", x2: "16", y1: "21", y2: "21" }
            line { x1: "12", x2: "12", y1: "17", y2: "21" }
        }
    }
}

#[component]
pub fn IconCar(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M19 17h2c.6 0 1-.4 1-1v-3c0-.9-.7-1.7-1.5-1.9C18.7 10.6 16 10 16 10s-1.3-1.4-2.2-2.3c-.5-.4-1.1-.7-1.8-.7H5c-.6 0-1.1.4-1.4.9l-1.4 2.9A3.7 3.7 0 0 0 2 12v4c0 .6.4 1 1 1h2" }
            circle { cx: "7", cy: "17", r: "2" }
            path { d: "M9 17h6" }
            circle { cx: "17", cy: "17", r: "2" }
        }
    }
}

#[component]
pub fn IconDisc3(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            circle { cx: "12", cy: "12", r: "10" }
            path { d: "M6 12c0-1.7.7-3.2 1.8-4.2" }
            circle { cx: "12", cy: "12", r: "2" }
            path { d: "M18 12c0 1.7-.7 3.2-1.8 4.2" }
        }
    }
}

#[component]
pub fn IconClock(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            circle { cx: "12", cy: "12", r: "10" }
            polyline { points: "12 6 12 12 16 14" }
        }
    }
}

#[component]
pub fn IconListPlus(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M11 12H3" }
            path { d: "M16 6H3" }
            path { d: "M16 18H3" }
            path { d: "M18 9v6" }
            path { d: "M21 12h-6" }
        }
    }
}

#[component]
pub fn IconVolume1(#[props(default = 18)] size: u32, #[props(default)] class: String) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z" }
            path { d: "M16 9a5 5 0 0 1 0 6" }
        }
    }
}

#[component]
pub fn IconArrowRight(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M5 12h14" }
            path { d: "m12 5 7 7-7 7" }
        }
    }
}

#[component]
pub fn IconShieldCheck(
    #[props(default = 18)] size: u32,
    #[props(default)] class: String,
) -> Element {
    rsx! {
        svg {
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.75",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            class: "shrink-0 {class}",
            path { d: "M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" }
            path { d: "m9 12 2 2 4-4" }
        }
    }
}
