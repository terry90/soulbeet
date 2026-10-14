use dioxus::prelude::*;

use crate::Logo;

#[component]
pub fn Navbar(children: Element) -> Element {
    rsx! {
        header { class: "px-4 sm:px-6 lg:px-8 flex justify-between items-center py-6 border-b border-white/5",
            // Logo area
            div { class: "flex items-center gap-3 group cursor-default",
                Logo {
                    size: 48,
                    class: "drop-shadow-[0_0_10px_rgba(255,0,255,0.45)] group-hover:-translate-y-1 transition-transform",
                }
                h1 { class: "hidden md:block text-2xl font-bold tracking-tighter uppercase text-transparent bg-clip-text bg-gradient-to-r from-white to-gray-400",
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
