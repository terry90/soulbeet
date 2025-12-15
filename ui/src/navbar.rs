use dioxus::prelude::*;

#[component]
pub fn Navbar(children: Element) -> Element {
    rsx! {
        header { class: "flex justify-between items-center py-6 border-b border-white/5",
            // Logo area
            div { class: "flex items-center gap-3 group cursor-default",
                div { class: "w-10 h-10 bg-beet-accent rounded-sm flex items-center justify-center shadow-[0_0_15px_rgba(217,70,239,0.5)] group-hover:rotate-12 transition-transform",
                    svg {
                        class: "w-6 h-6 text-white",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M9 19V6l12-3v13M9 19c0 1.105-1.343 2-3 2s-3-.895-3-2 1.343-2 3-2 3 .895 3 2zm12-3c0 1.105-1.343 2-3 2s-3-.895-3-2 1.343-2 3-2 3 .895 3 2zM9 10l12-3",
                        }
                    }
                }
                h1 { class: "text-2xl font-bold tracking-tighter uppercase text-transparent bg-clip-text bg-gradient-to-r from-white to-gray-400",
                    "Soulbeet"
                }
            }

            // Menu
            nav { class: "flex items-center gap-8 bg-beet-panel/50 px-6 py-2 rounded-full border border-white/5 backdrop-blur-sm",
                {children}
            }
        }
    }
}
