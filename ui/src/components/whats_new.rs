use dioxus::prelude::*;
use shared::changelog::{Release, Section, Span, Text};

use crate::Modal;

/// What the section waiting for the next release shows instead of a version.
pub const NEXT_RELEASE: &str = "Coming in the next release";

#[derive(Props, PartialEq, Clone)]
pub struct WhatsNewProps {
    /// Newest first. The first shows in full, the others compact below it.
    pub sections: Vec<Section>,
    /// Called by "Got it", the close button, a click outside and Escape.
    pub on_close: EventHandler,
}

/// The release notes of CHANGELOG.md as a dialog.
#[component]
pub fn WhatsNew(props: WhatsNewProps) -> Element {
    let Some((newest, earlier)) = props.sections.split_first() else {
        return rsx! {};
    };
    let on_close = props.on_close;

    rsx! {
        Modal {
            on_close: move |_| on_close.call(()),
            header: rsx! {
                h2 { class: "text-xs font-mono uppercase tracking-widest text-gray-400", "What's new in Soulbeet" }
            },
            footer: rsx! {
                div { class: "flex flex-wrap items-center gap-x-4 gap-y-3",
                    if let Some(link) = &newest.link {
                        a {
                            class: "inline-flex items-center gap-1.5 py-2 -my-2 text-xs font-mono text-gray-300 hover:text-beet-leaf underline decoration-dotted underline-offset-4 transition-colors",
                            href: "{link.url}",
                            target: "_blank",
                            rel: "noopener noreferrer",
                            "{link.text}"
                            svg {
                                class: "w-3.5 h-3.5",
                                fill: "none",
                                view_box: "0 0 24 24",
                                stroke: "currentColor",
                                "aria-hidden": "true",
                                path {
                                    stroke_linecap: "round",
                                    stroke_linejoin: "round",
                                    stroke_width: "2",
                                    d: "M14 5h5v5M19 5l-8 8M10 5H6a1 1 0 00-1 1v12a1 1 0 001 1h12a1 1 0 001-1v-4",
                                }
                            }
                        }
                    }
                    button {
                        class: "retro-btn rounded ml-auto",
                        onclick: move |_| on_close.call(()),
                        "Got it"
                    }
                }
            },
            ReleaseCard { section: newest.clone() }
            if !earlier.is_empty() {
                div { class: "mt-6 pt-4 border-t border-white/10",
                    h3 { class: "text-[10px] font-mono uppercase tracking-widest text-gray-400",
                        "Earlier releases"
                    }
                    for section in earlier {
                        CompactRelease { section: section.clone() }
                    }
                }
            }
        }
    }
}

/// The version chip and date, or the next-release chip.
#[component]
fn ReleaseLabel(release: Option<Release>) -> Element {
    match release {
        Some(release) => {
            let date = release.date;
            let iso = format!("{:04}-{:02}-{:02}", date.year, date.month, date.day);
            rsx! {
                div { class: "flex flex-wrap items-center gap-2",
                    span { class: "px-2 py-0.5 rounded-full border border-beet-leaf/30 bg-beet-leaf/10 text-beet-leaf text-[11px] font-mono tracking-wider",
                        "v{release.version}"
                    }
                    time { class: "text-[11px] font-mono text-gray-400", datetime: "{iso}", "{date}" }
                }
            }
        }
        None => rsx! {
            div { class: "flex",
                span { class: "px-2 py-0.5 rounded-full border border-beet-accent/40 bg-beet-accent/10 text-beet-accent text-[11px] font-mono tracking-wider",
                    "{NEXT_RELEASE}"
                }
            }
        },
    }
}

#[component]
fn ReleaseCard(section: Section) -> Element {
    rsx! {
        ReleaseLabel { release: section.release }
        h3 { class: "mt-3 text-xl sm:text-2xl font-bold font-display text-white leading-tight",
            RichText { text: section.headline }
        }
        p { class: "mt-2 text-sm text-gray-300 leading-relaxed",
            RichText { text: section.intro }
        }
        ul { class: "mt-5 space-y-3", role: "list",
            for highlight in section.highlights {
                li { class: "flex gap-3",
                    span {
                        class: "mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-beet-leaf shadow-[0_0_6px_rgba(0,255,157,0.7)]",
                        aria_hidden: "true",
                    }
                    div { class: "min-w-0",
                        p { class: "text-sm font-semibold text-white",
                            RichText { text: highlight.title }
                        }
                        p { class: "text-sm text-gray-400 leading-relaxed",
                            RichText { text: highlight.text }
                        }
                    }
                }
            }
        }
    }
}

/// An earlier release: its label, headline and highlight titles.
#[component]
fn CompactRelease(section: Section) -> Element {
    rsx! {
        div { class: "mt-4",
            ReleaseLabel { release: section.release }
            h4 { class: "mt-1.5 text-sm font-semibold font-display text-white",
                RichText { text: section.headline }
            }
            ul { class: "mt-1 space-y-0.5", role: "list",
                for highlight in section.highlights {
                    li { class: "flex gap-2 text-xs text-gray-400",
                        span { class: "mt-1.5 h-1 w-1 shrink-0 rounded-full bg-gray-500", aria_hidden: "true" }
                        span { RichText { text: highlight.title } }
                    }
                }
            }
            if let Some(link) = section.link {
                a {
                    class: "-mt-1 -mb-2 inline-block py-2 text-xs font-mono text-gray-400 hover:text-beet-leaf underline decoration-dotted underline-offset-4 transition-colors",
                    href: "{link.url}",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    "{link.text}"
                }
            }
        }
    }
}

#[component]
fn RichText(text: Text) -> Element {
    rsx! {
        for span in text.0 {
            match span {
                Span::Plain(words) => rsx! { "{words}" },
                Span::Code(code) => rsx! {
                    code { class: "font-mono text-[0.85em] px-1 py-0.5 rounded bg-black/30 text-beet-leaf/90 break-words",
                        "{code}"
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::changelog::parse;

    const LOG: &str = "# Changelog\n\n## Unreleased\n\n### Ask your assistant\n\n\
        Soulbeet answers at `/mcp` now.\n\n\
        - **Plain words**: Ask for an album.\n\
        - **Keeps going**: Downloads carry on.\n\
        - **Your account**: Each person connects.\n\n\
        [How to set it up](https://github.com/terry90/soulbeet#mcp-server)\n\n\
        ## 0.9.0 - 2026-12-01\n\n### Quieter imports\n\nFewer prompts.\n\n\
        - **Skips**: Skipped imports wait for you.\n\
        - **Matches**: Pick a match.\n\
        - **Notes**: Import notes are shorter.\n\n\
        [Read more](https://example.org/0.9)\n";

    fn render(sections: Vec<Section>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |sections: Vec<Section>| rsx! { WhatsNew { sections, on_close: |_| {} } },
            sections,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn sections() -> Vec<Section> {
        parse(LOG).unwrap().sections
    }

    #[test]
    fn the_newest_section_shows_in_full_with_its_actions() {
        let html = render(sections());
        assert!(html.contains("What&#39;s new in Soulbeet"), "{html}");
        assert!(html.contains(NEXT_RELEASE), "{html}");
        assert!(html.contains("Ask your assistant"), "{html}");
        assert!(html.contains("Soulbeet answers at <code"), "{html}");
        assert!(html.contains(">/mcp</code> now."), "{html}");
        assert!(html.contains("Plain words") && html.contains("Ask for an album."));
        assert!(html.contains("href=\"https://github.com/terry90/soulbeet#mcp-server\""));
        assert!(html.contains("How to set it up"));
        assert!(html.contains(">Got it</button>"), "{html}");
    }

    #[test]
    fn earlier_sections_show_compact_below() {
        let html = render(sections());
        let earlier = html.find("Earlier releases").expect("an Earlier part");
        let rest = &html[earlier..];
        assert!(rest.contains("v0.9.0"), "{html}");
        assert!(rest.contains("datetime=\"2026-12-01\""), "{html}");
        assert!(rest.contains("1 Dec 2026"), "{html}");
        assert!(rest.contains("Quieter imports") && rest.contains("Skips"));
        // Headings nest: the earlier releases are a group below the newest.
        assert!(rest.starts_with("Earlier releases</h3>"), "{html}");
        assert!(rest.contains(">Quieter imports</h4>"), "{html}");
        assert_eq!(html.matches("role=\"list\"").count(), 2, "{html}");
        // Compact: titles only, no intro or sentences.
        assert!(!html.contains("Fewer prompts."), "{html}");
        assert!(!html.contains("Skipped imports wait for you."), "{html}");
    }

    #[test]
    fn a_released_section_alone_shows_its_version_and_no_earlier_part() {
        let html = render(sections().split_off(1));
        assert!(
            html.contains("v0.9.0") && html.contains("1 Dec 2026"),
            "{html}"
        );
        assert!(html.contains("Fewer prompts."), "{html}");
        assert!(!html.contains(NEXT_RELEASE), "{html}");
        assert!(!html.contains("Earlier releases"), "{html}");
    }

    #[test]
    fn the_dialog_is_named_by_its_title() {
        let html = render(sections());
        assert!(html.contains("role=\"dialog\""), "{html}");
        assert!(html.contains("aria-modal=\"true\""), "{html}");
        let start = html.find("aria-labelledby=\"").unwrap() + "aria-labelledby=\"".len();
        let id = &html[start..start + html[start..].find('"').unwrap()];
        let title = &html[html.find(&format!("id=\"{id}\"")).expect("the title")..];
        assert!(title.contains("What&#39;s new in Soulbeet"), "{html}");
    }

    #[test]
    fn nothing_shows_without_sections() {
        assert_eq!(render(Vec::new()), "");
    }
}
