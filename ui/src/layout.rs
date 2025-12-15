use crate::components::Footer;
use dioxus::prelude::*;

#[component]
pub fn Layout(children: Element) -> Element {
    rsx! {
      // CRT Scanline Effect Overlay
      div { class: "fixed inset-0 z-50 pointer-events-none opacity-50 crt-overlay h-full w-full" }

      // Main container
      div { class: "relative z-10 flex flex-col h-screen max-w-7xl mx-auto px-4 sm:px-6 lg:px-8",
        {children}
        Footer {}
      }
    }
}
