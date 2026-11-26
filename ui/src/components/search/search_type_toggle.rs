use dioxus::prelude::*;

pub use crate::settings_context::SearchType;

/// The key the search form submits the chosen type under.
const NAME: &str = "search_type";

/// Album or track, as a segmented Midnight Studio radio group.
#[component]
pub fn SearchTypeToggle(search_type: Signal<SearchType>) -> Element {
    rsx! {
      fieldset { class: "flex flex-none items-center gap-1 min-w-0 m-0 p-0 border-0",
        legend { class: "sr-only", "Search for" }
        for (kind, label) in [(SearchType::Album, "Album"), (SearchType::Track, "Track")] {
          label {
            key: "{kind.as_str()}",
            class: "relative flex items-center h-[40px] cursor-pointer",
            input {
              class: "peer absolute inset-0 m-0 opacity-0 cursor-pointer",
              r#type: "radio",
              name: NAME,
              value: kind.as_str(),
              checked: search_type() == kind,
              onchange: move |_| search_type.set(kind),
            }
            span {
              class: if search_type() == kind {
                "text-beet-text-primary bg-beet-surface-highest border-beet-border-default"
              } else {
                "text-beet-text-tertiary border-transparent peer-hover:text-beet-text-primary peer-hover:bg-beet-surface-high"
              },
              class: "block px-3 max-[399px]:px-2 py-1.5 rounded-[8px] border tabular-nums-mono text-[11px] font-semibold tracking-[0.06em] uppercase transition-colors duration-150 peer-focus-visible:outline-2 peer-focus-visible:outline-offset-1 peer-focus-visible:outline-beet-leaf",
              "{label}"
            }
          }
        }
      }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opening_tag;

    #[test]
    fn album_and_track_are_radios_in_a_named_group() {
        let mut dom = VirtualDom::new(|| {
            let search_type = use_signal(|| SearchType::Track);
            rsx! { SearchTypeToggle { search_type } }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.starts_with("<fieldset"), "{html}");
        assert!(
            html.contains("<legend class=\"sr-only\">Search for</legend>"),
            "{html}"
        );
        let album = opening_tag(&html, "value=\"album\"");
        let track = opening_tag(&html, "value=\"track\"");
        for radio in [album, track] {
            assert!(radio.contains("type=\"radio\""), "{radio}");
            assert!(radio.contains(&format!("name=\"{NAME}\"")), "{radio}");
        }
        assert!(track.contains("checked=true"), "{track}");
        assert!(!album.contains("checked=true"), "{album}");
    }

    #[test]
    fn choosing_a_radio_sets_the_search_type() {
        thread_local! {
            static CHOSEN: std::cell::Cell<Option<SearchType>> = const { std::cell::Cell::new(None) };
        }
        let mut dom = VirtualDom::new(|| {
            let search_type = use_signal(|| SearchType::Album);
            use_effect(move || CHOSEN.with(|c| c.set(Some(search_type()))));
            rsx! { SearchTypeToggle { search_type } }
        });
        let changes = crate::listeners(&mut dom, "change");
        assert_eq!(changes.len(), 2);
        crate::change(&mut dom, changes[1]);
        assert_eq!(CHOSEN.with(|c| c.get()), Some(SearchType::Track));
    }
}
