use api::models::folder::Folder;
use dioxus::prelude::*;
use shared::metadata::asks_no_title;

use super::folder_chip::FolderChip;
use super::search_type_toggle::{SearchType, SearchTypeToggle};
use crate::{submitted_text, Button, FieldKind, FieldLook, TextInput, FOCUS_RING};

/// The keys the search form submits its fields under.
const QUERY: &str = "query";
const ARTIST: &str = "artist";

const QUERY_ID: &str = "search-query";
const ARTIST_ID: &str = "search-artist";

/// A row of the bar, edged cleanly in Studio leaf green while a field in it has focus.
const ROW_CLASS: &str =
    "flex items-center focus-within:shadow-[inset_2px_0_0_var(--color-beet-leaf-solid)]";

/// The icon or word before a row's field.
const LEAD_CLASS: &str = "flex-none w-[44px] self-stretch flex items-center justify-center text-beet-text-tertiary cursor-text group-focus-within/row:text-beet-leaf-text transition-colors";

/// Whether the form has something to search for: a title, or an artist
/// whose albums to list.
fn can_search(query: &str, artist: Option<&str>) -> bool {
    !asks_no_title(query) || artist.is_some_and(|a| !a.trim().is_empty())
}

#[derive(Props, Clone, PartialEq)]
pub struct SearchBarProps {
    pub search: Signal<String>,
    pub artist: Signal<Option<String>>,
    /// The artist row was asked for. It also shows while the artist holds a
    /// value, such as one a search was prefilled with.
    pub artist_open: Signal<bool>,
    pub search_type: Signal<SearchType>,
    pub loading: bool,
    pub folders: Vec<Folder>,
    pub selected_folder_id: Signal<Option<String>>,
    pub on_folder_change: EventHandler<Folder>,
    pub active_menu: Signal<Option<String>>,
    /// Runs on every submit, once the fields' text is in `search` and `artist`.
    pub on_search: EventHandler,
    /// Optional placeholder override (e.g. when Offline Only mode is active).
    #[props(default)]
    pub placeholder_override: Option<String>,
    /// Whether to show the `ALBUM` / `TRACK` entity toggle in the bar controls strip.
    #[props(default = true)]
    pub show_type_toggle: bool,
}

/// The unified Studio search form: query input, optional artist row for
/// discography & filtered searches, `ALBUM` / `TRACK` segmented radios, and
/// destination download folder selector.
#[component]
pub fn SearchBar(props: SearchBarProps) -> Element {
    let SearchBarProps {
        mut search,
        mut artist,
        mut artist_open,
        search_type,
        loading,
        on_search,
        placeholder_override,
        show_type_toggle,
        ..
    } = props;

    // Set when a click shows or hides the artist row, so focus moves to
    // whichever of its field or the "+ Artist" button appears in place of
    // the control that was clicked.
    let mut move_focus = use_signal(|| false);
    let focus_if_moved = move |evt: MountedEvent| {
        if *move_focus.peek() {
            move_focus.set(false);
            let element = evt.data();
            spawn(async move {
                let _ = element.set_focus(true).await;
            });
        }
    };

    let artist_shown = artist_open() || artist.read().is_some();
    // With an artist, the title may be left out to list the artist's albums.
    let placeholder = placeholder_override.unwrap_or_else(|| {
        match (search_type(), artist_shown) {
            (SearchType::Album, false) => "Search albums",
            (SearchType::Track, false) => "Search tracks",
            (SearchType::Album, true) => "Album title (optional)",
            (SearchType::Track, true) => "Track title (optional)",
        }
        .to_string()
    });

    rsx! {
      form {
        class: "group relative",
        role: "search",
        aria_label: "Search music",
        aria_busy: loading,
        onsubmit: move |evt: Event<FormData>| {
            evt.prevent_default();
            search.set(submitted_text(&evt, QUERY));
            let typed = submitted_text(&evt, ARTIST);
            let typed = typed.trim();
            artist.set((!typed.is_empty()).then(|| typed.to_string()));
            on_search.call(());
        },

        div { class: "relative bg-beet-surface border border-beet-border-default focus-within:border-beet-border-strong rounded-[12px] transition-colors",

          div { class: "{ROW_CLASS} group/row relative min-h-[54px] pr-[6px] rounded-t-[11px]",
            label { class: LEAD_CLASS, r#for: QUERY_ID,
              svg {
                class: "size-[18px]",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "1.75",
                view_box: "0 0 24 24",
                "aria-hidden": "true",
                path {
                  stroke_linecap: "round",
                  stroke_linejoin: "round",
                  d: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
                }
              }
              span { class: "sr-only", "{placeholder}" }
            }
            TextInput {
              look: FieldLook::Bar,
              class: "text-beet-text-primary placeholder:text-beet-text-tertiary",
              kind: FieldKind::Search,
              enterkeyhint: "search",
              id: QUERY_ID,
              name: QUERY,
              value: search(),
              oninput: move |text| search.set(text),
              placeholder: placeholder.clone(),
            }
            Button {
              class: "flex-none inline-flex items-center justify-center min-h-[42px] min-w-[92px] md:min-w-[104px] ml-[8px] rounded-[8px] whitespace-nowrap {FOCUS_RING}",
              submit: true,
              disabled: !can_search(&search.read(), artist.read().as_deref()),
              busy: loading,
              if loading {
                span {
                  class: "size-[14px] rounded-full border-2 border-beet-leaf/20 border-t-beet-leaf-text animate-spin motion-reduce:animate-none",
                  aria_hidden: "true",
                }
                span { class: "sr-only", "Searching" }
              } else {
                "Search"
              }
            }
            // The light along the seam under the query while a search runs.
            if loading {
              span {
                class: "seam-light absolute inset-x-0 -bottom-px h-[2px]",
                aria_hidden: "true",
              }
            }
          }

          if artist_shown {
            div { class: "{ROW_CLASS} group/row min-h-[46px] border-t border-beet-border-subtle",
              label { class: "{LEAD_CLASS} tabular-nums-mono text-[12px] italic", r#for: ARTIST_ID,
                span { aria_hidden: "true", "by" }
                span { class: "sr-only", "Artist" }
              }
              TextInput {
                look: FieldLook::Bar,
                class: "text-beet-text-secondary placeholder:text-beet-text-tertiary",
                enterkeyhint: "search",
                id: ARTIST_ID,
                name: ARTIST,
                value: artist.read().clone().unwrap_or_default(),
                oninput: move |text: String| {
                    // Emptying the field keeps the row it was typed in.
                    if !*artist_open.peek() {
                        artist_open.set(true);
                    }
                    artist.set((!text.is_empty()).then_some(text));
                },
                onmounted: focus_if_moved,
                placeholder: "Artist name",
              }
              button {
                class: "flex-none size-[44px] flex items-center justify-center rounded-[8px] text-beet-text-tertiary hover:text-beet-text-primary cursor-pointer transition-colors {FOCUS_RING}",
                r#type: "button",
                aria_label: "Remove artist filter",
                onclick: move |_| {
                    artist.set(None);
                    artist_open.set(false);
                    move_focus.set(true);
                },
                svg {
                  class: "size-[16px]",
                  fill: "none",
                  stroke: "currentColor",
                  stroke_width: "1.75",
                  view_box: "0 0 24 24",
                  "aria-hidden": "true",
                  path { stroke_linecap: "round", d: "M6 6l12 12M18 6L6 18" }
                }
              }
            }
          }

          div { class: "flex items-center gap-1.5 min-h-[44px] px-2 border-t border-beet-border-subtle bg-beet-surface-low rounded-b-[11px]",
            if show_type_toggle {
              SearchTypeToggle { search_type }
            }
            if !artist_shown {
              button {
                class: "flex-none inline-flex items-center gap-1.5 h-[36px] px-2.5 max-[399px]:px-2 rounded-[8px] tabular-nums-mono text-[11px] font-semibold tracking-[0.06em] uppercase text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high cursor-pointer transition-colors {FOCUS_RING}",
                r#type: "button",
                aria_label: "Add artist filter",
                onmounted: focus_if_moved,
                onclick: move |_| {
                    artist_open.set(true);
                    move_focus.set(true);
                },
                svg {
                  class: "size-[12px]",
                  fill: "none",
                  stroke: "currentColor",
                  stroke_width: "2",
                  view_box: "0 0 24 24",
                  "aria-hidden": "true",
                  path { stroke_linecap: "round", d: "M12 5v14M5 12h14" }
                }
                "Artist"
              }
            }
            if props.folders.len() > 1 {
              FolderChip {
                folders: props.folders.clone(),
                selected_folder_id: props.selected_folder_id,
                on_folder_change: props.on_folder_change,
                active_menu: props.active_menu,
              }
            }
          }
        }
      }
      // Outside the busy form, whose changes a screen reader may hold back.
      p { class: "sr-only", role: "status",
        if loading {
          "Searching"
        }
      }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opening_tag;
    use crate::search::folder_chip::tests::folder;

    #[derive(Props, Clone, PartialEq)]
    struct Setup {
        artist: Option<String>,
        folders: usize,
        loading: bool,
        search_type: SearchType,
    }

    impl Default for Setup {
        fn default() -> Self {
            Self {
                artist: None,
                folders: 0,
                loading: false,
                search_type: SearchType::Album,
            }
        }
    }

    thread_local! {
        static SEARCHED: std::cell::RefCell<Vec<(String, Option<String>)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn bar(setup: Setup) -> VirtualDom {
        VirtualDom::new_with_props(
            |setup: Setup| {
                let search = use_signal(String::new);
                let artist = use_signal(|| setup.artist.clone());
                let artist_open = use_signal(|| false);
                let search_type = use_signal(|| setup.search_type);
                let selected_folder_id = use_signal(|| Some("f0".to_string()));
                let active_menu = use_signal(|| None);
                let folders = (0..setup.folders)
                    .map(|i| folder(&format!("f{i}"), "Lossless archive from the old NAS"))
                    .collect::<Vec<_>>();
                rsx! {
                  SearchBar {
                    search,
                    artist,
                    artist_open,
                    search_type,
                    loading: setup.loading,
                    folders,
                    selected_folder_id,
                    on_folder_change: |_| {},
                    active_menu,
                    on_search: move |_| SEARCHED.with(|s| s.borrow_mut().push((search(), artist()))),
                  }
                }
            },
            setup,
        )
    }

    fn render(setup: Setup) -> String {
        let mut dom = bar(setup);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn enter_and_the_button_both_go_through_the_form() {
        let html = render(Setup::default());
        let form = opening_tag(&html, "role=\"search\"");
        assert!(form.starts_with("<form"), "{form}");
        assert!(form.contains("aria-label=\"Search music\""), "{form}");
        let query = opening_tag(&html, &format!("id=\"{QUERY_ID}\""));
        for attribute in [
            "type=\"search\"",
            "enterkeyhint=\"search\"",
            "name=\"query\"",
        ] {
            assert!(query.contains(attribute), "{attribute} in {query}");
        }
        let button = opening_tag(&html, "retro-btn");
        assert!(button.contains("type=\"submit\""), "{button}");
        // Nothing to search for yet, so Enter does not submit either.
        assert!(button.contains("disabled=true"), "{button}");

        let mut dom = bar(Setup::default());
        let submits = crate::listeners(&mut dom, "submit");
        assert_eq!(submits.len(), 1);
        let browser_submits = crate::submit_form(
            &mut dom,
            submits[0],
            &[(QUERY, "Abbey Road"), (ARTIST, "The Beatles")],
        );
        assert!(!browser_submits);
        assert_eq!(
            SEARCHED.with(|s| s.take()),
            [("Abbey Road".to_string(), Some("The Beatles".to_string()))]
        );
    }

    #[test]
    fn an_artist_alone_can_be_searched() {
        let chevelle = || Setup {
            artist: Some("Chevelle".to_string()),
            ..Setup::default()
        };
        let html = render(chevelle());
        let button = opening_tag(&html, "retro-btn");
        assert!(!button.contains("disabled=true"), "{button}");
        assert!(
            html.contains("placeholder=\"Album title (optional)\""),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"sr-only\">Album title (optional)</span>"),
            "{html}"
        );
        let html = render(Setup {
            search_type: SearchType::Track,
            ..chevelle()
        });
        assert!(
            html.contains("placeholder=\"Track title (optional)\""),
            "{html}"
        );

        let mut dom = bar(chevelle());
        let submits = crate::listeners(&mut dom, "submit");
        crate::submit_form(&mut dom, submits[0], &[(QUERY, ""), (ARTIST, " Chevelle ")]);
        // The artist is kept trimmed, and a blank one is no artist.
        crate::submit_form(
            &mut dom,
            submits[0],
            &[(QUERY, "Vena Sera"), (ARTIST, "  ")],
        );
        assert_eq!(
            SEARCHED.with(|s| s.take()),
            [
                (String::new(), Some("Chevelle".to_string())),
                ("Vena Sera".to_string(), None)
            ]
        );

        assert!(!can_search("*", None));
        assert!(!can_search(" ", Some("  ")));
        assert!(can_search("*", Some("Chevelle")));
        assert!(can_search("Vena Sera", None));
    }

    #[test]
    fn the_placeholder_follows_the_search_type() {
        let html = render(Setup::default());
        assert!(html.contains("placeholder=\"Search albums\""), "{html}");
        let html = render(Setup {
            search_type: SearchType::Track,
            ..Setup::default()
        });
        assert!(html.contains("placeholder=\"Search tracks\""), "{html}");
        assert!(
            html.contains("<span class=\"sr-only\">Search tracks</span>"),
            "{html}"
        );
    }

    #[test]
    fn the_artist_row_opens_when_asked_for() {
        let html = render(Setup::default());
        assert!(!html.contains(ARTIST_ID), "{html}");
        assert!(html.contains("aria-label=\"Add artist filter\""), "{html}");

        let mut dom = bar(Setup::default());
        // "+ Artist", then the search button, which its form handles.
        let clicks = crate::listeners(&mut dom, "click");
        assert_eq!(clicks.len(), 2);
        crate::click(&mut dom, clicks[0]);
        let html = dioxus_ssr::render(&dom);
        let field = opening_tag(&html, &format!("id=\"{ARTIST_ID}\""));
        assert!(field.contains("enterkeyhint=\"search\""), "{field}");
        assert!(
            html.contains("aria-label=\"Remove artist filter\""),
            "{html}"
        );
        assert!(!html.contains("Add artist filter"), "{html}");
    }

    #[test]
    fn a_prefilled_artist_shows_its_row() {
        let html = render(Setup {
            artist: Some("The Beatles".to_string()),
            ..Setup::default()
        });
        let field = opening_tag(&html, &format!("id=\"{ARTIST_ID}\""));
        assert!(field.contains("value=\"The Beatles\""), "{field}");
        assert!(!html.contains("Add artist filter"), "{html}");
    }

    #[test]
    fn the_folder_chip_shows_at_every_width_with_several_folders() {
        let html = render(Setup {
            folders: 2,
            ..Setup::default()
        });
        assert!(html.contains("Download folder: "), "{html}");
        // One bar for every width: nothing in it is hidden at one.
        for class in ["hidden md:", "md:hidden"] {
            assert!(!html.contains(class), "{class} in {html}");
        }

        let html = render(Setup {
            folders: 1,
            ..Setup::default()
        });
        assert!(!html.contains("Download folder: "), "{html}");
    }

    #[test]
    fn a_running_search_shows_on_the_button_and_to_screen_readers() {
        let idle = render(Setup::default());
        assert!(opening_tag(&idle, "role=\"search\"").contains("aria-busy=false"));
        assert!(!idle.contains("seam-light"), "{idle}");
        assert!(
            idle.contains("</form><p class=\"sr-only\" role=\"status\"></p>"),
            "{idle}"
        );

        let html = render(Setup {
            loading: true,
            ..Setup::default()
        });
        assert!(opening_tag(&html, "role=\"search\"").contains("aria-busy=true"));
        assert!(html.contains("seam-light"), "{html}");
        // After the form, so its aria-busy does not hold the news back.
        assert!(
            html.contains("</form><p class=\"sr-only\" role=\"status\">Searching</p>"),
            "{html}"
        );
        let button = opening_tag(&html, "retro-btn");
        assert!(button.contains("disabled=true"), "{button}");
        // Busy, not dimmed like a button with nothing to do.
        assert!(button.contains("opacity-80"), "{button}");
        assert!(!button.contains("grayscale"), "{button}");
        assert!(button.contains("min-w-[92px]"), "{button}");
    }
}
