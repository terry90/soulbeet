use dioxus::prelude::*;

#[component]
pub fn Footer() -> Element {
    rsx! {
      footer { class: "py-4 text-center border-t border-white/5",
        div { class: "flex justify-center gap-6 text-[10px] font-mono uppercase tracking-widest text-gray-500",
          a {
            class: "hover:text-beet-accent transition-colors",
            href: "https://hub.docker.com/repository/docker/docccccc/soulbeet",
            target: "_blank",
            "[ Docker Hub ]"
          }
          a {
            class: "hover:text-beet-accent transition-colors",
            href: "https://github.com/terry90/soulbeet",
            target: "_blank",
            "[ Github ]"
          }
        }
      }
    }
}
