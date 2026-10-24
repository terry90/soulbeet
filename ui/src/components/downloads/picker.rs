use std::future::Future;
use std::pin::Pin;

use dioxus::prelude::*;
use shared::download::{ImportChoice, MatchCandidate, MatchChoices, MatchKind};

use super::Failure;
use crate::{use_open_handler, Choice, ChoiceList, Disclosure, RowAction, TextInput};

/// Resolves to the matches of the row's item, or to where the failure to
/// load them shows.
pub type ChoicesLoader =
    Callback<String, Pin<Box<dyn Future<Output = Result<MatchChoices, Failure>>>>>;

/// "71.4%" for 714 per mille.
fn percent(per_mille: u16) -> String {
    format!("{}.{}%", per_mille / 10, per_mille % 10)
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// How a candidate reads in the list. The source is named only when the
/// list mixes sources.
fn choice(candidate: &MatchCandidate, name_source: bool) -> Choice {
    let c = candidate;
    let mut detail: Vec<String> = [
        c.album.clone(),
        c.year.map(|y| y.to_string()),
        c.country.clone(),
        c.disambig.clone(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if let Some(secs) = c.length_secs {
        detail.push(format!("{}:{:02}", secs / 60, secs % 60));
    }
    if let Some(fit) = c.tracks {
        detail.push(match fit.matched == fit.total {
            true => plural(fit.total, "track", "tracks"),
            false => format!(
                "{} of {}",
                fit.matched,
                plural(fit.total, "track", "tracks")
            ),
        });
        if fit.extra_files > 0 {
            let files = plural(fit.extra_files, "file", "files");
            detail.push(format!("{files} left behind"));
        }
    }
    if name_source {
        detail.push(c.data_source.clone());
    }
    Choice {
        title: format!("{} - {}", c.artist, c.title),
        aside: Some(percent(c.per_mille)),
        detail: (!detail.is_empty()).then(|| detail.join(" · ")),
        note: (!c.differs.is_empty()).then(|| format!("differs: {}", c.differs.join(", "))),
    }
}

/// What "Import with this match" imports: a typed link or ID, else the
/// picked candidate.
fn chosen(
    candidates: &[MatchCandidate],
    picked: Option<&(String, String)>,
    typed: &str,
) -> Option<ImportChoice> {
    if !typed.trim().is_empty() {
        return Some(ImportChoice::MusicBrainz(typed.to_string()));
    }
    let (data_source, id) = picked?;
    candidates
        .iter()
        .any(|c| c.data_source == *data_source && c.id == *id)
        .then(|| ImportChoice::Candidate {
            data_source: data_source.clone(),
            id: id.clone(),
        })
}

fn id_label(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::Track => "MusicBrainz recording link or ID",
        MatchKind::Album => "MusicBrainz release link or ID",
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct MatchPickerProps {
    pub item: String,
    /// An action of the row is running.
    pub busy: bool,
    pub on_load: ChoicesLoader,
    pub on_choose: Callback<ImportChoice>,
    /// Why the last load or pick failed. The picker sets it for loads.
    pub notice: Signal<Option<String>>,
}

/// The matches beets weighed for a skipped import, loaded on open, and a
/// field for a MusicBrainz link or ID, to import with one of them.
#[component]
pub fn MatchPicker(props: MatchPickerProps) -> Element {
    let mut found = use_signal(|| None::<MatchChoices>);
    // (data_source, id) of the picked candidate, so a reload keeps the pick.
    let mut picked = use_signal(|| None::<(String, String)>);
    let mut typed = use_signal(String::new);

    let on_load = props.on_load;
    let mut notice = props.notice;
    let item = props.item.clone();
    let load = use_open_handler(move || {
        let item = item.clone();
        async move {
            match on_load.call(item).await {
                Ok(choices) => {
                    found.set(Some(choices));
                    notice.set(None);
                    true
                }
                // A failed reopen still shows the list it loaded before.
                Err(Failure::InPicker(message)) => {
                    notice.set(Some(message));
                    found.read().is_some()
                }
                // The row took the picker away and says why.
                Err(Failure::OnRow) => false,
            }
        }
    });

    let label = match &*found.read() {
        Some(choices) => format!("matches ({})", choices.candidates.len()),
        None => "matches".to_string(),
    };
    let on_choose = props.on_choose;

    rsx! {
        div { class: "mt-2",
            Disclosure { label, on_open: load,
                if let Some(choices) = found() {
                    {
                        let keys: Vec<(String, String)> = choices
                            .candidates
                            .iter()
                            .map(|c| (c.data_source.clone(), c.id.clone()))
                            .collect();
                        let selected = picked.read().as_ref().and_then(|p| keys.iter().position(|k| k == p));
                        let name_source = keys.windows(2).any(|pair| pair[0].0 != pair[1].0);
                        let list: Vec<Choice> = choices
                            .candidates
                            .iter()
                            .map(|c| choice(c, name_source))
                            .collect();
                        let choice = chosen(&choices.candidates, picked.read().as_ref(), &typed.read());
                        let on_enter = choice.clone();
                        let id_label = id_label(choices.kind);
                        rsx! {
                            div { class: "mt-2 space-y-2",
                                if list.is_empty() {
                                    p { class: "text-xs font-mono text-gray-500", "beets found no matches." }
                                } else {
                                    ChoiceList {
                                        class: "max-h-80 overflow-y-auto",
                                        name: "matches-{props.item}",
                                        label: "Matches beets found",
                                        choices: list,
                                        selected,
                                        on_select: move |i: usize| {
                                            picked.set(keys.get(i).cloned());
                                            typed.set(String::new());
                                        },
                                    }
                                }
                                TextInput {
                                    class: "text-sm placeholder-gray-600",
                                    placeholder: id_label,
                                    aria_label: id_label,
                                    value: typed(),
                                    oninput: move |text: String| {
                                        if !text.trim().is_empty() {
                                            picked.set(None);
                                        }
                                        typed.set(text);
                                    },
                                    on_enter: move |_| {
                                        if let Some(choice) = on_enter.clone() {
                                            on_choose.call(choice);
                                        }
                                    },
                                }
                                RowAction {
                                    busy: props.busy,
                                    disabled: choice.is_none(),
                                    onclick: move |_| {
                                        if let Some(choice) = choice.clone() {
                                            on_choose.call(choice);
                                        }
                                    },
                                    "Import with this match"
                                }
                            }
                        }
                    }
                }
            }
            // Always present, so a screen reader announces the text it gets.
            p {
                class: if notice.read().is_some() { "mt-2 text-xs font-mono text-red-400 break-words" },
                role: "status",
                aria_live: "polite",
                if let Some(text) = notice() {
                    "{text}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::simple::ToggleDriver;
    use shared::download::TrackFit;

    fn track(source: &str, id: &str) -> MatchCandidate {
        MatchCandidate {
            data_source: source.to_string(),
            id: id.to_string(),
            artist: "Marrow Choir".to_string(),
            title: "Vesper Bells (Live)".to_string(),
            album: Some("Live at Somewhere".to_string()),
            year: Some(2019),
            country: Some("GB".to_string()),
            disambig: Some("choir version".to_string()),
            per_mille: 714,
            differs: vec!["length".to_string(), "title".to_string()],
            length_secs: Some(222),
            tracks: None,
        }
    }

    const LOAD_FAILED: &str = "Could not reach the server. Check your connection.";

    /// A picker whose first `good_loads` loads find one match and whose
    /// later loads fail in the picker.
    fn picker(notice: Option<&'static str>, good_loads: u32) -> VirtualDom {
        VirtualDom::new_with_props(
            |(notice, good_loads): (Option<&'static str>, u32)| {
                let notice = use_signal(|| notice.map(str::to_string));
                let loads = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(0)));
                let on_load: ChoicesLoader = Callback::new(move |_: String| {
                    loads.set(loads.get() + 1);
                    let result = match loads.get() <= good_loads {
                        true => Ok(MatchChoices {
                            kind: MatchKind::Track,
                            candidates: vec![track("MusicBrainz", "r1")],
                        }),
                        false => Err(Failure::InPicker(LOAD_FAILED.to_string())),
                    };
                    Box::pin(async move { result })
                        as Pin<Box<dyn Future<Output = Result<MatchChoices, Failure>>>>
                });
                rsx! {
                    MatchPicker { item: "a", busy: false, on_load, on_choose: |_| {}, notice }
                }
            },
            (notice, good_loads),
        )
    }

    fn render_picker(notice: Option<&'static str>) -> String {
        let mut dom = picker(notice, 0);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_failed_first_load_stays_closed_and_says_why() {
        let mut picker = ToggleDriver::new(picker(None, 0));
        picker.click();
        let html = picker.html();
        assert!(html.contains(LOAD_FAILED), "{html}");
        assert!(html.contains("Show matches<"), "{html}");
        assert!(!html.contains("Import with this match"), "{html}");
    }

    #[test]
    fn a_failed_reload_opens_with_the_list_loaded_before_and_says_why() {
        let mut picker = ToggleDriver::new(picker(None, 1));
        picker.click();
        let html = picker.html();
        assert!(html.contains("Hide matches (1)"), "{html}");
        assert!(!html.contains(LOAD_FAILED), "{html}");
        picker.click();
        assert!(picker.html().contains("Show matches (1)"));
        picker.click();
        let html = picker.html();
        assert!(html.contains(LOAD_FAILED), "{html}");
        assert!(html.contains("Hide matches (1)"), "{html}");
        assert!(
            html.contains("Marrow Choir - Vesper Bells (Live)"),
            "{html}"
        );
        assert!(html.contains("Import with this match"), "{html}");
    }

    #[test]
    fn the_notice_is_a_live_region_present_before_it_has_text() {
        let quiet = render_picker(None);
        assert!(quiet.contains("role=\"status\""), "{quiet}");
        assert!(quiet.contains("aria-live=\"polite\""), "{quiet}");

        let told = render_picker(Some(LOAD_FAILED));
        assert!(told.contains(LOAD_FAILED), "{told}");
        // Closed, the picker shows only its toggle and the notice.
        assert!(told.contains("Show matches"), "{told}");
        assert!(!told.contains("Import with this match"), "{told}");
    }

    #[test]
    fn percent_keeps_one_decimal() {
        assert_eq!(percent(714), "71.4%");
        assert_eq!(percent(1000), "100.0%");
        assert_eq!(percent(5), "0.5%");
        assert_eq!(percent(0), "0.0%");
    }

    #[test]
    fn a_track_reads_artist_title_then_album_year_country_disambig_length() {
        let c = choice(&track("MusicBrainz", "r1"), false);
        assert_eq!(c.title, "Marrow Choir - Vesper Bells (Live)");
        assert_eq!(c.aside.as_deref(), Some("71.4%"));
        assert_eq!(
            c.detail.as_deref(),
            Some("Live at Somewhere · 2019 · GB · choir version · 3:42")
        );
        assert_eq!(c.note.as_deref(), Some("differs: length, title"));
    }

    #[test]
    fn missing_parts_are_left_out() {
        let bare = MatchCandidate {
            album: None,
            year: None,
            country: None,
            disambig: None,
            length_secs: None,
            differs: vec![],
            per_mille: 1000,
            ..track("MusicBrainz", "r1")
        };
        let c = choice(&bare, false);
        assert_eq!(c.detail, None);
        assert_eq!(c.note, None);
        assert_eq!(c.aside.as_deref(), Some("100.0%"));
    }

    #[test]
    fn an_album_shows_how_the_files_fit_and_the_source_when_mixed() {
        let album = MatchCandidate {
            title: "Live at Somewhere".to_string(),
            album: None,
            disambig: None,
            length_secs: None,
            tracks: Some(TrackFit {
                matched: 9,
                total: 10,
                extra_files: 1,
            }),
            ..track("Discogs", "123")
        };
        let c = choice(&album, true);
        assert_eq!(c.title, "Marrow Choir - Live at Somewhere");
        assert_eq!(
            c.detail.as_deref(),
            Some("2019 · GB · 9 of 10 tracks · 1 file left behind · Discogs")
        );
        let whole = MatchCandidate {
            tracks: Some(TrackFit {
                matched: 10,
                total: 10,
                extra_files: 2,
            }),
            year: None,
            country: None,
            ..album
        };
        assert_eq!(
            choice(&whole, false).detail.as_deref(),
            Some("10 tracks · 2 files left behind")
        );
    }

    #[test]
    fn a_typed_id_wins_over_the_picked_candidate() {
        let list = [track("MusicBrainz", "r1"), track("MusicBrainz", "r2")];
        let picked = ("MusicBrainz".to_string(), "r2".to_string());
        assert_eq!(chosen(&list, None, ""), None);
        assert_eq!(chosen(&list, None, "   "), None);
        assert_eq!(
            chosen(&list, Some(&picked), " "),
            Some(ImportChoice::Candidate {
                data_source: "MusicBrainz".to_string(),
                id: "r2".to_string(),
            })
        );
        assert_eq!(
            chosen(
                &list,
                Some(&picked),
                " https://musicbrainz.org/recording/x "
            ),
            Some(ImportChoice::MusicBrainz(
                " https://musicbrainz.org/recording/x ".to_string()
            ))
        );
    }

    #[test]
    fn a_pick_no_longer_listed_is_no_choice() {
        let list = [track("MusicBrainz", "r1")];
        let gone = ("MusicBrainz".to_string(), "r9".to_string());
        assert_eq!(chosen(&list, Some(&gone), ""), None);
        let other_source = ("Discogs".to_string(), "r1".to_string());
        assert_eq!(chosen(&list, Some(&other_source), ""), None);
    }
}
