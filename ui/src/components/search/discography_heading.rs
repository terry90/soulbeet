use dioxus::prelude::*;
use shared::metadata::ArtistSummary;

use crate::FOCUS_RING;

/// `artist`'s name, then its disambiguation after `between` when it has one.
fn artist_label(artist: &ArtistSummary, between: &str, after: &str) -> String {
    match &artist.disambiguation {
        Some(d) => format!("{}{between}{d}{after}", artist.name),
        None => artist.name.clone(),
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct DiscographyHeadingProps {
    /// The name the search was for.
    pub name: String,
    /// The artist whose albums are listed.
    pub artist: ArtistSummary,
    /// The other artists carrying the name.
    pub others: Vec<ArtistSummary>,
    /// Every album and EP of the artist is listed.
    pub complete: bool,
    /// How many are listed.
    pub shown: usize,
    /// Runs with the MBID of the other artist picked.
    pub on_pick: EventHandler<String>,
}

/// The heading of an artist-only search: whose albums these are, the other
/// artists of that name to list instead, and whether some were left out.
#[component]
pub fn DiscographyHeading(props: DiscographyHeadingProps) -> Element {
    let DiscographyHeadingProps {
        name,
        artist,
        others,
        complete,
        shown,
        on_pick,
    } = props;
    // Each other artist's MBID, button text and accessible name.
    let picks: Vec<(String, String, String)> = (others.iter())
        .map(|o| {
            let aria = format!("Show albums by {}", artist_label(o, ", ", ""));
            (o.mbid.clone(), artist_label(o, " (", ")"), aria)
        })
        .collect();

    rsx! {
      div { class: "mb-4 border-b border-white/10 pb-2 space-y-2",
        h5 { class: "text-xl font-display font-bold text-white",
          "Albums by {artist.name}"
          if let Some(disambiguation) = &artist.disambiguation {
            span { class: "ml-2 text-sm font-mono font-normal text-gray-500", "{disambiguation}" }
          }
        }
        if !picks.is_empty() {
          div { class: "flex flex-wrap items-center gap-2 text-xs font-mono text-gray-400",
            span { "Other artists named {name}:" }
            for (mbid, label, aria) in picks {
              button {
                key: "{mbid}",
                class: "min-h-[44px] px-[10px] rounded border border-white/10 hover:border-white/20 hover:text-white cursor-pointer {FOCUS_RING}",
                r#type: "button",
                aria_label: aria,
                onclick: move |_| on_pick.call(mbid.clone()),
                "{label}"
              }
            }
          }
        }
        if !complete {
          p { class: "text-xs font-mono text-gray-500",
            "Showing {shown} albums and EPs, studio ones first. Add a title to search the rest."
          }
        }
      }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opening_tag;

    thread_local! {
        static PICKED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn nirvana(mbid: &str, disambiguation: &str) -> ArtistSummary {
        ArtistSummary {
            mbid: mbid.into(),
            name: "Nirvana".into(),
            disambiguation: Some(disambiguation.into()),
        }
    }

    #[derive(Props, Clone, PartialEq)]
    struct Setup {
        others: Vec<ArtistSummary>,
        complete: bool,
    }

    fn heading(others: Vec<ArtistSummary>, complete: bool) -> VirtualDom {
        VirtualDom::new_with_props(
            |setup: Setup| {
                rsx! {
                  DiscographyHeading {
                    name: "Nirvana",
                    artist: nirvana(
                        "5b11f4ce-a62d-471e-81fc-a69a8278c7da",
                        "1980s–1990s US grunge band",
                    ),
                    others: setup.others,
                    complete: setup.complete,
                    shown: 300,
                    on_pick: |mbid| PICKED.with(|p| p.borrow_mut().push(mbid)),
                  }
                }
            },
            Setup { others, complete },
        )
    }

    fn render(others: Vec<ArtistSummary>, complete: bool) -> String {
        let mut dom = heading(others, complete);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_heading_names_the_artist_and_offers_the_others() {
        let uk = || {
            vec![nirvana(
                "9282c8b4-ca0b-4c6b-b7e3-4f7762dfc4d6",
                "60s band from the UK",
            )]
        };
        let html = render(uk(), true);
        assert!(html.contains("Albums by Nirvana"), "{html}");
        assert!(html.contains("1980s–1990s US grunge band"), "{html}");
        assert!(html.contains("Other artists named Nirvana:"), "{html}");
        let button = opening_tag(
            &html,
            "aria-label=\"Show albums by Nirvana, 60s band from the UK\"",
        );
        assert!(button.contains("type=\"button\""), "{button}");
        assert!(
            html.contains(">Nirvana (60s band from the UK)</button>"),
            "{html}"
        );
        assert!(!html.contains("Showing"), "{html}");

        let mut dom = heading(uk(), true);
        let clicks = crate::listeners(&mut dom, "click");
        assert_eq!(clicks.len(), 1);
        crate::click(&mut dom, clicks[0]);
        assert_eq!(
            PICKED.with(|p| p.take()),
            ["9282c8b4-ca0b-4c6b-b7e3-4f7762dfc4d6"]
        );
    }

    #[test]
    fn the_heading_says_when_albums_were_left_out() {
        let html = render(vec![], false);
        assert!(
            html.contains(
                "Showing 300 albums and EPs, studio ones first. Add a title to search the rest."
            ),
            "{html}"
        );
        assert!(!html.contains("Other artists"), "{html}");
    }
}
