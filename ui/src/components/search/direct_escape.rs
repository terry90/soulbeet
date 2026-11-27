use dioxus::prelude::*;
use shared::download::DownloadQuery;
use shared::metadata::Track;

use crate::{IconSearch, TextInput};

/// Splits a raw query into `(artist, title)` when the user typed `"Artist - Title"`,
/// or honors an explicit `artist_hint` if one was filled in the search bar.
pub fn split_artist_title(artist_hint: Option<&str>, query: &str) -> (String, String) {
    let trimmed_query = query.trim();
    if let Some(hint) = artist_hint.map(str::trim).filter(|s| !s.is_empty()) {
        return (hint.to_string(), trimmed_query.to_string());
    }
    for sep in [" - ", " – "] {
        if let Some((left, right)) = trimmed_query.split_once(sep) {
            let artist = left.trim();
            let title = right.trim();
            if !artist.is_empty() && !title.is_empty() {
                return (artist.to_string(), title.to_string());
            }
        }
    }
    (String::new(), trimmed_query.to_string())
}

/// Builds a direct `DownloadQuery` for a track without a MusicBrainz ID (`mbid: None`).
pub fn direct_track_query(artist: &str, title: &str) -> DownloadQuery {
    let artist = artist.trim().to_string();
    let title = title.trim().to_string();
    DownloadQuery::from(Track {
        id: format!("direct:{artist}:{title}"),
        title,
        artist,
        album_id: None,
        album_title: None,
        release_date: None,
        duration: None,
        mbid: None,
        release_mbid: None,
    })
}

/// Subtle secondary escape hatch rendered when a MusicBrainz track search returns 0 results,
/// allowing the user to search enabled download backends directly with artist and title.
#[component]
pub fn DirectSearchEscape(
    query: String,
    #[props(default)] artist_hint: Option<String>,
    on_search_sources: EventHandler<DownloadQuery>,
) -> Element {
    let (init_artist, init_title) = split_artist_title(artist_hint.as_deref(), &query);
    let mut open = use_signal(|| false);
    let mut artist = use_signal(move || init_artist);
    let mut title = use_signal(move || init_title);

    // Keep pre-filled values synced when the user searches a different query.
    let seed_now = (artist_hint.clone(), query.clone());
    let seed_init = seed_now.clone();
    let mut last_seed = use_signal(move || seed_init);
    if *last_seed.peek() != seed_now {
        let (a, t) = split_artist_title(artist_hint.as_deref(), &query);
        artist.set(a);
        title.set(t);
        open.set(false);
        last_seed.set(seed_now);
    }

    let can_submit = !artist.read().trim().is_empty() && !title.read().trim().is_empty();
    let submit = move || {
        let a = artist.read().trim().to_string();
        let t = title.read().trim().to_string();
        if a.is_empty() || t.is_empty() {
            return;
        }
        on_search_sources.call(direct_track_query(&a, &t));
    };

    rsx! {
        div { class: "mt-4 flex flex-col items-center gap-3",
            if !open() {
                button {
                    r#type: "button",
                    class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-[12px] font-medium text-beet-text-secondary hover:text-beet-text-primary hover:border-beet-accent/40 transition-colors cursor-pointer",
                    onclick: move |_| open.set(true),
                    IconSearch { size: 13 }
                    span { "Not on MusicBrainz? Search sources directly" }
                }
            } else {
                div { class: "w-full max-w-md rounded-[12px] bg-beet-surface-low border border-beet-border-subtle p-3.5 text-left space-y-2.5",
                    p { class: "text-[12px] font-medium text-beet-text-secondary",
                        "Search download sources directly"
                    }
                    div { class: "grid grid-cols-1 sm:grid-cols-2 gap-2",
                        TextInput {
                            class: "text-[12px] rounded-[8px] text-beet-text-primary placeholder:text-beet-text-disabled",
                            placeholder: "Artist",
                            aria_label: "Direct search artist",
                            value: artist(),
                            oninput: move |v: String| artist.set(v),
                            on_enter: move |_| submit(),
                        }
                        TextInput {
                            class: "text-[12px] rounded-[8px] text-beet-text-primary placeholder:text-beet-text-disabled",
                            placeholder: "Track title",
                            aria_label: "Direct search title",
                            value: title(),
                            oninput: move |v: String| title.set(v),
                            on_enter: move |_| submit(),
                        }
                    }
                    div { class: "flex items-center justify-end gap-2",
                        button {
                            r#type: "button",
                            class: "h-8 px-3 rounded-[8px] bg-beet-surface border border-beet-border-subtle text-[12px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer",
                            onclick: move |_| open.set(false),
                            "Cancel"
                        }
                        button {
                            r#type: "button",
                            disabled: !can_submit,
                            class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-[8px] bg-beet-accent-solid text-white text-[12px] font-semibold cursor-pointer disabled:opacity-45 disabled:cursor-not-allowed",
                            onclick: move |_| submit(),
                            IconSearch { size: 13 }
                            span { "Search sources" }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_artist_and_title_from_dash_query_or_hint() {
        assert_eq!(
            split_artist_title(None, "Burial - Untrue (Bootleg)"),
            ("Burial".into(), "Untrue (Bootleg)".into())
        );
        assert_eq!(
            split_artist_title(Some("Four Tet"), "Unreleased Dub"),
            ("Four Tet".into(), "Unreleased Dub".into())
        );
        assert_eq!(
            split_artist_title(None, "Just A Title"),
            ("".into(), "Just A Title".into())
        );
    }

    #[test]
    fn direct_track_query_leaves_mbid_none() {
        let q = direct_track_query("  Burial ", " Archangel Edit ");
        assert_eq!(q.tracks.len(), 1);
        assert_eq!(q.tracks[0].artist, "Burial");
        assert_eq!(q.tracks[0].title, "Archangel Edit");
        assert_eq!(q.tracks[0].mbid, None);
        assert_eq!(q.album, None);
    }
}
