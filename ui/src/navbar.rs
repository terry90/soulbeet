use dioxus::prelude::*;

use crate::{Logo, SearchReset};

/// `home` is where the logo leads: the app's search page. Clicking it also
/// clears the search, like the Search link does.
#[component]
pub fn Navbar(#[props(into)] home: NavigationTarget, children: Element) -> Element {
    let search_reset = try_use_context::<SearchReset>();

    rsx! {
        header { class: "px-4 sm:px-6 lg:px-8 flex justify-between items-center gap-3 py-6 border-b border-white/5",
            Link {
                class: "flex shrink-0 items-center gap-3 group cursor-pointer",
                to: home,
                aria_label: "Soulbeet home",
                onclick: move |_| {
                    if let Some(SearchReset(mut reset)) = search_reset {
                        reset += 1;
                    }
                },
                Logo {
                    size: 48,
                    class: "drop-shadow-[0_0_10px_rgba(255,0,255,0.45)] group-hover:-translate-y-1 transition-transform",
                }
                h1 { class: "hidden md:block text-2xl font-bold tracking-tighter uppercase text-transparent bg-clip-text bg-gradient-to-r from-white to-gray-400",
                    "Soulbeet"
                }
            }

            // Menu
            nav { class: "flex items-center gap-3 sm:gap-8 bg-beet-panel/50 px-3 sm:px-6 py-2 rounded-full border border-white/5 backdrop-blur-sm",
                {children}
            }
        }
    }
}
