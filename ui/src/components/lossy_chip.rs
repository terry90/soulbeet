use dioxus::prelude::*;
use shared::backends::{BackendMeta, Delivery};

/// `LOSSY · {NAME}` for a backend that delivers lossy files only; nothing for any other.
#[component]
pub fn LossyChip(meta: &'static BackendMeta) -> Element {
    if meta.delivery != Delivery::LossyOnly {
        return rsx! {};
    }
    let name = meta.name.to_uppercase();
    rsx! {
        span {
            class: "text-[10px] font-mono px-1.5 py-0.5 rounded border bg-amber-500/20 text-amber-300 border-amber-500/40 uppercase shrink-0",
            title: "Lossy files only",
            "LOSSY · {name}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::backends::{Switch, SLSKD};

    static YOUTUBE: BackendMeta = BackendMeta {
        id: "ytdlp",
        name: "YouTube Music",
        delivery: Delivery::LossyOnly,
        switch: Switch::AdminToggle,
        ..SLSKD
    };

    fn render(meta: &'static BackendMeta) -> String {
        let mut dom = VirtualDom::new_with_props(
            |meta: &'static BackendMeta| rsx! { LossyChip { meta } },
            meta,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn only_a_lossy_backend_gets_the_chip() {
        let html = render(&YOUTUBE);
        assert!(html.contains(">LOSSY · YOUTUBE MUSIC</span>"), "{html}");
        assert!(html.contains("bg-amber-500/20"), "{html}");
        assert!(!render(&SLSKD).contains("LOSSY"));
    }
}
