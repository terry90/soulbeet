use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use shared::backends::{BackendMeta, Delivery, DEFAULT_ID};
use shared::download::{DownloadableGroup, DownloadableItem};
use std::collections::HashSet;

use crate::{use_auth, Checkbox, LossyChip, FIELD_CLASS};

/// Format a sample rate in Hz as kHz, trimming a trailing .0 (44100 -> "44.1", 48000 -> "48").
fn format_khz(hz: u32) -> String {
    let khz = f64::from(hz) / 1000.0;
    if khz.fract() == 0.0 {
        format!("{}", khz as i64)
    } else {
        format!("{khz:.1}")
    }
}

/// The bitrate a backend estimates, when it only has an estimate (`backend_facts.estimated_kbps`).
fn estimated_kbps(item: &DownloadableItem) -> Option<u64> {
    item.facts
        .backend_facts
        .get("estimated_kbps")
        .and_then(|v| v.as_u64())
}

/// The badge text and colour of a file, from the facts its backend reported.
fn quality_label(item: &DownloadableItem) -> (String, &'static str) {
    let format = item
        .facts
        .extension
        .clone()
        .unwrap_or_else(|| item.quality.to_lowercase());
    let known_bitrate = item.facts.bitrate_kbps.filter(|&br| br > 0);
    let estimate = || estimated_kbps(item).filter(|_| known_bitrate.is_none());
    let label = if format == "mp3" {
        match (known_bitrate, estimate()) {
            (Some(br), _) if br >= 320 => "320k".to_string(),
            (Some(br), _) if br >= 256 => "256k".to_string(),
            (Some(br), _) if br >= 192 => "192k".to_string(),
            (Some(br), _) => format!("{br}k"),
            (None, Some(est)) => format!("~{est}k"),
            (None, None) => "MP3".to_string(),
        }
    } else if matches!(format.as_str(), "flac" | "wav") {
        // Lossless: show bit depth / sample rate like slskd does (e.g. FLAC 16/44.1)
        match (item.facts.bit_depth, item.facts.sample_rate_hz) {
            (Some(d), Some(r)) => {
                format!("{} {}/{}", format.to_uppercase(), d, format_khz(r))
            }
            (None, Some(r)) => format!("{} {}", format.to_uppercase(), format_khz(r)),
            _ => format.to_uppercase(),
        }
    } else {
        match estimate() {
            Some(est) => format!("{} ~{est}k", format.to_uppercase()),
            None => format.to_uppercase(),
        }
    };

    let color = match format.as_str() {
        "flac" | "wav" => "bg-blue-600/30 text-blue-300 border-blue-500/40",
        "m4a" | "aac" | "ogg" => "bg-green-600/30 text-green-300 border-green-500/40",
        _ => "bg-gray-600/30 text-gray-300 border-gray-500/40",
    };
    (label, color)
}

fn quality_badge(item: &DownloadableItem) -> Element {
    let (label, color) = quality_label(item);
    rsx! {
        span {
            class: "text-[10px] font-mono px-1.5 py-0.5 rounded border {color} uppercase shrink-0",
            "{label}"
        }
    }
}

/// The backend a group's files came from, when this build knows it.
fn backend_of(group: &DownloadableGroup) -> Option<&'static BackendMeta> {
    group
        .items
        .first()
        .and_then(|i| shared::backends::meta(&i.backend))
}

/// The page of the group's source at its backend, when the backend gave an https one.
fn item_url(group: &DownloadableGroup) -> Option<String> {
    let url = group
        .items
        .first()?
        .facts
        .backend_facts
        .get("item_url")?
        .as_str()?;
    url.starts_with("https://").then(|| url.to_string())
}

fn is_lossy(meta: Option<&BackendMeta>) -> bool {
    meta.is_some_and(|m| m.delivery == Delivery::LossyOnly)
}

#[derive(Props, PartialEq, Clone)]
pub struct Props {
    pub results: Vec<DownloadableGroup>,
    /// Shown under "No results found": why no source may have answered.
    pub unanswered_note: Option<String>,
    pub is_searching: bool,
    pub is_downloading: Signal<bool>,
    #[props(into)]
    pub on_download: EventHandler<(Vec<DownloadableItem>, String)>,
    #[props(into)]
    pub on_back: EventHandler<()>,
}

#[derive(Props, Clone, PartialEq)]
struct AlbumResultItemProps {
    album: DownloadableGroup,
    /// The backend the group's files came from.
    backend: Option<&'static BackendMeta>,
    selected_tracks: Signal<HashSet<String>>,
    on_album_select_all: EventHandler<DownloadableGroup>,
    on_track_toggle: EventHandler<String>,
    is_best: bool,
    starts_expanded: bool,
}

#[derive(Props, Clone, PartialEq)]
struct TrackItemProps {
    track: DownloadableItem,
    is_selected: bool,
    on_toggle: EventHandler<String>,
}

fn get_track_id(track: &DownloadableItem) -> String {
    format!("{}{}", track.id, track.source)
}

#[component]
fn TrackItem(props: TrackItemProps) -> Element {
    let unique_id = get_track_id(&props.track);

    rsx! {
        li {
            key: "{unique_id}",
            class: "flex items-center gap-2 p-1 rounded-md hover:bg-white/10 cursor-pointer",
            onclick: move |_| props.on_toggle.call(unique_id.clone()),

            Checkbox { is_selected: props.is_selected }

            label { class: "cursor-pointer text-gray-300 font-mono text-sm flex-1 min-w-0 truncate",
                "{props.track.title}"
            }

            {quality_badge(&props.track)}
        }
    }
}

#[component]
fn AlbumResultItem(props: AlbumResultItemProps) -> Element {
    let album = props.album.clone();
    let source_page = item_url(&album);
    let mut is_expanded = use_signal(|| props.starts_expanded);

    rsx! {
        div {
            key: "{album.group_id}",
            class: "bg-white/5 border border-white/5 p-3 sm:p-4 rounded-md",

            // Clickable header for expand/collapse
            div {
                class: "flex justify-between items-center gap-3 cursor-pointer",
                onclick: move |_| is_expanded.toggle(),

                div { class: "flex-grow min-w-0",
                    if props.is_best {
                        span {
                            class: "inline-block mb-2 text-[10px] font-mono px-1.5 py-0.5 rounded border bg-beet-accent/20 text-beet-accent border-beet-accent/40 uppercase",
                            "Best match"
                        }
                    }
                    if let Some(meta) = props.backend {
                        LossyChip { meta }
                    }
                    h4 { class: "text-base leading-snug font-bold text-beet-leaf break-words", "{album.title}" }
                    if let Some(artist) = album.artist.as_ref().filter(|a| !a.is_empty()) {
                        p { class: "mt-1 text-sm text-gray-300 font-mono break-words", "{artist}" }
                    }
                    p { class: "mt-1 text-xs text-gray-400 font-mono uppercase tracking-wide",
                        "{album.quality} · Score {album.score:.2}"
                    }
                    if let Some(meta) = props.backend.filter(|m| m.id != DEFAULT_ID) {
                        p { class: "mt-1 text-xs text-gray-300 font-mono", "{meta.name}" }
                    }
                    if let Some(attribution) = props.backend.and_then(|m| m.attribution) {
                        p { class: "mt-1 text-xs text-gray-400 font-mono break-words", "{attribution}" }
                    }
                    if let Some(url) = source_page {
                        a {
                            class: "mt-1 inline-block text-xs font-mono text-gray-300 hover:text-white underline decoration-dotted",
                            href: "{url}",
                            target: "_blank",
                            rel: "noopener noreferrer",
                            onclick: move |e| e.stop_propagation(),
                            "Source page"
                        }
                    }
                }

                // Expand/collapse chevron
                svg {
                    class: "w-4 h-4 transition-transform duration-200 shrink-0",
                    class: if *is_expanded.read() { "rotate-90 text-gray-400" } else { "text-gray-600" },
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    view_box: "0 0 24 24",
                    path {
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        d: "M8.25 4.5l7.5 7.5-7.5 7.5",
                    }
                }
            }

            // Track list and Select All only visible when expanded
            if *is_expanded.read() {
                button {
                    class: "font-mono uppercase text-[10px] whitespace-nowrap tracking-widest px-3 py-1 mt-4 mb-2 border border-beet-leaf/30 text-beet-leaf hover:bg-beet-leaf hover:text-beet-dark transition-colors cursor-pointer rounded",
                    onclick: move |_| props.on_album_select_all.call(album.clone()),
                    "Select All"
                }
                ul { class: "space-y-1",
                    for track in props.album.items {
                        TrackItem {
                            is_selected: props.selected_tracks.read().contains(&get_track_id(&track)),
                            track,
                            on_toggle: props.on_track_toggle,
                        }
                    }
                }
            }
        }
    }
}

/// The groups, best first, each with the backend it came from (`backends`, one per group), and
/// "Lossy, from {name}" before the first group of a backend that delivers lossy files only.
#[component]
fn GroupList(
    groups: Vec<DownloadableGroup>,
    backends: Vec<Option<&'static BackendMeta>>,
    selected_tracks: Signal<HashSet<String>>,
    on_album_select_all: EventHandler<DownloadableGroup>,
    on_track_toggle: EventHandler<String>,
) -> Element {
    let first_lossy = backends.iter().position(|b| is_lossy(*b));
    rsx! {
        for (idx, (album, backend)) in groups.into_iter().zip(backends).enumerate() {
            if let Some(meta) = backend.filter(|_| Some(idx) == first_lossy) {
                h4 { class: "pt-2 text-sm font-mono text-amber-300", "Lossy, from {meta.name}" }
            }
            AlbumResultItem {
                backend,
                album,
                selected_tracks,
                on_album_select_all,
                on_track_toggle,
                is_best: idx == 0,
                starts_expanded: idx == 0,
            }
        }
    }
}

/// Main component responsible for displaying all download options.
#[component]
pub fn DownloadResults(props: Props) -> Element {
    let mut selected_tracks = use_signal(HashSet::<String>::new);
    let results = props.results.clone();
    let mut folders = use_signal(std::vec::Vec::new);
    let mut selected_folder = use_signal(|| "".to_string());
    let mut is_downloading = props.is_downloading;
    let auth = use_auth();

    use_future(move || async move {
        if let Ok(user_folders) = auth.call(api::get_user_folders()).await {
            info!("Fetched {} user folders", user_folders.len());

            // Only select if the user has exactly one folder
            // It could be error prone to auto-select if there are multiple folders
            // (I failed multiple times because of this)
            if user_folders.len() == 1 {
                selected_folder.set(user_folders[0].path.clone());
            }
            folders.set(user_folders);
        }
    });

    let handle_album_select_all = move |group: DownloadableGroup| {
        let mut selected = selected_tracks.write();
        let all_selected = group
            .items
            .iter()
            .all(|t| selected.contains(&get_track_id(t)));

        if all_selected {
            for item in &group.items {
                selected.remove(&get_track_id(item));
            }
        } else {
            for item in &group.items {
                selected.insert(get_track_id(item));
            }
        }
    };

    let handle_track_toggle = move |filename: String| {
        info!("Toggle track selection: {}", filename);
        let mut selected = selected_tracks.write();
        if selected.contains(&filename) {
            selected.remove(&filename);
        } else {
            selected.insert(filename);
        }
    };

    let handle_download = move |_| {
        // Prevent double-clicks by checking if already downloading
        if *is_downloading.read() {
            info!("Download already in progress, ignoring click");
            return;
        }

        let selected_ids = selected_tracks.read();

        let items_to_download: Vec<DownloadableItem> = props
            .results
            .iter()
            .flat_map(|group| group.items.iter())
            .filter(|item| selected_ids.contains(&get_track_id(item)))
            .cloned()
            .collect();

        if items_to_download.is_empty() {
            return;
        }

        // Set downloading state immediately to prevent double-clicks
        is_downloading.set(true);

        props
            .on_download
            .call((items_to_download, selected_folder()));
    };

    rsx! {
        div { class: "bg-beet-panel border border-white/10 text-white p-4 sm:p-8 rounded-lg shadow-2xl w-full max-w-2xl mx-auto my-10 font-display relative",
            div { class: "relative mb-6",
                button {
                    class: "absolute left-0 top-1/2 -translate-y-1/2 p-2 rounded-full hover:bg-white/10 transition-colors cursor-pointer",
                    aria_label: "Back",
                    onclick: move |_| props.on_back.call(()),
                    svg {
                        class: "w-5 h-5 text-gray-400",
                        fill: "none",
                        stroke: "currentColor",
                        view_box: "0 0 24 24",
                        path {
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            stroke_width: "2",
                            d: "M15 19l-7-7 7-7",
                        }
                    }
                }
                h3 { class: "text-2xl font-bold text-center text-beet-accent", "Download Options" }
            }
            div { class: "mb-4",
                label {
                    r#for: "dl_folder",
                    class: "block text-sm font-medium mb-1 text-gray-400 font-mono",
                    "Select Target Folder"
                }
                select {
                    name: "dl_folder",
                    class: FIELD_CLASS,
                    value: "{selected_folder}",
                    onchange: move |e| selected_folder.set(e.value()),
                    option { value: "", disabled: true, "Select a folder" }
                    for folder in folders.read().iter() {
                        option { value: "{folder.path}", "{folder.name}" }
                    }
                }
            }

            div { class: "space-y-4",
                if props.is_searching {
                    div { class: "flex flex-col items-center justify-center p-4 bg-white/5 rounded-lg",
                        div { class: "animate-spin rounded-full h-8 w-8 border-t-2 border-b-2 border-beet-accent mb-2" }
                        p { class: "text-sm text-gray-300 animate-pulse text-center font-mono",
                            "Searching... The rarer your track is, the longer the search can take."
                        }
                    }
                } else if results.is_empty() {
                    div { class: "text-center text-gray-500 py-8 font-mono",
                        p { "No results found" }
                        if let Some(note) = &props.unanswered_note {
                            p { class: "text-sm mt-2", "{note}" }
                        }
                    }
                }
                GroupList {
                    backends: results.iter().map(backend_of).collect::<Vec<_>>(),
                    groups: results.clone(),
                    selected_tracks,
                    on_album_select_all: handle_album_select_all,
                    on_track_toggle: handle_track_toggle,
                }
            }
            div { class: "fixed bottom-28 md:bottom-24 right-6 z-40",
                button {
                    class: "bg-beet-accent hover:bg-fuchsia-400 text-white font-bold p-4 rounded-full shadow-[0_0_15px_rgba(255,0,255,0.5)] transition-transform hover:scale-105 disabled:bg-gray-600 disabled:cursor-not-allowed disabled:shadow-none flex items-center justify-center cursor-pointer",
                    aria_label: "Start download",
                    disabled: selected_tracks.read().is_empty() || selected_folder.read().is_empty() || *is_downloading.read(),
                    onclick: handle_download,
                    if *is_downloading.read() {
                        // Show spinner when downloading
                        div { class: "animate-spin rounded-full h-6 w-6 border-t-2 border-b-2 border-white" }
                    } else {
                        svg {
                            class: "w-6 h-6",
                            fill: "none",
                            stroke: "currentColor",
                            view_box: "0 0 24 24",
                            path {
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                stroke_width: "2",
                                d: "M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4",
                            }
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
    use shared::backends::{Switch, SLSKD};
    use shared::download::FileFacts;

    static GD: BackendMeta = BackendMeta {
        id: "gdstudio",
        name: "NetEase (GD Studio)",
        service: "GD Studio",
        switch: Switch::AdminToggle,
        attribution: Some("NetEase via GD音乐台 (music.gdstudio.xyz)"),
        ..SLSKD
    };
    static YOUTUBE: BackendMeta = BackendMeta {
        id: "ytdlp",
        name: "YouTube Music",
        service: "YouTube",
        delivery: Delivery::LossyOnly,
        switch: Switch::AdminToggle,
        ..SLSKD
    };

    fn item(backend: &str, facts: FileFacts) -> DownloadableItem {
        DownloadableItem {
            id: "07 - Wake Up.flac".into(),
            backend: backend.into(),
            source: "peer".into(),
            title: "Wake Up".into(),
            artist: "Kowloon".into(),
            album: "Come Over".into(),
            size: None,
            duration: None,
            quality: facts.extension.clone().unwrap_or_default(),
            quality_score: 1.0,
            backend_data: None,
            match_score: 0.9,
            below_floor: false,
            facts,
        }
    }

    fn facts(ext: &str) -> FileFacts {
        FileFacts {
            extension: Some(ext.into()),
            ..FileFacts::default()
        }
    }

    fn estimated(ext: &str, kbps: u64) -> FileFacts {
        let mut facts = facts(ext);
        facts
            .backend_facts
            .insert("estimated_kbps".into(), kbps.into());
        facts
    }

    #[test]
    fn the_badge_reads_the_files_facts() {
        let label = |facts| quality_label(&item("slskd", facts)).0;
        for (facts, wanted) in [
            (
                FileFacts {
                    bitrate_kbps: Some(320),
                    ..facts("mp3")
                },
                "320k",
            ),
            (
                FileFacts {
                    bitrate_kbps: Some(224),
                    ..facts("mp3")
                },
                "192k",
            ),
            (
                FileFacts {
                    bitrate_kbps: Some(128),
                    ..facts("mp3")
                },
                "128k",
            ),
            (facts("mp3"), "MP3"),
            (
                FileFacts {
                    bit_depth: Some(16),
                    sample_rate_hz: Some(44_100),
                    ..facts("flac")
                },
                "FLAC 16/44.1",
            ),
            (
                FileFacts {
                    sample_rate_hz: Some(48_000),
                    ..facts("wav")
                },
                "WAV 48",
            ),
            (facts("flac"), "FLAC"),
            (facts("m4a"), "M4A"),
            (estimated("mp3", 160), "~160k"),
            (estimated("opus", 130), "OPUS ~130k"),
            (
                FileFacts {
                    bitrate_kbps: Some(256),
                    ..estimated("mp3", 160)
                },
                "256k",
            ),
        ] {
            assert_eq!(label(facts.clone()), wanted, "{facts:?}");
        }
    }

    fn group(backend: &str, url: Option<&str>) -> DownloadableGroup {
        let mut facts = facts("flac");
        if let Some(url) = url {
            facts.backend_facts.insert("item_url".into(), url.into());
        }
        DownloadableGroup {
            source: format!("{backend}-source"),
            group_id: format!("{backend}/group"),
            title: format!("Come Over from {backend}"),
            artist: Some("Kowloon".into()),
            item_count: 1,
            total_size: 1,
            items: vec![item(backend, facts)],
            quality: "flac".into(),
            score: 0.9,
        }
    }

    fn render(
        groups: Vec<DownloadableGroup>,
        backends: Vec<Option<&'static BackendMeta>>,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(groups, backends): (Vec<DownloadableGroup>, Vec<Option<&'static BackendMeta>>)| {
                let selected_tracks = use_signal(HashSet::new);
                rsx! {
                    GroupList {
                        groups,
                        backends,
                        selected_tracks,
                        on_album_select_all: |_| {},
                        on_track_toggle: |_| {},
                    }
                }
            },
            (groups, backends),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_group_of_another_backend_names_it_and_its_source_page() {
        let html = render(
            vec![
                group("slskd", None),
                group("gdstudio", Some("https://music.163.com/song?id=1")),
                group("other", Some("http://example.invalid/item")),
            ],
            vec![Some(&SLSKD), Some(&GD), None],
        );
        let slskd = &html[..html.find("Come Over from gdstudio").unwrap()];
        assert!(!slskd.contains("Soulseek"), "{slskd}");
        assert!(html.contains(">NetEase (GD Studio)</p>"), "{html}");
        assert!(
            html.contains(">NetEase via GD音乐台 (music.gdstudio.xyz)</p>"),
            "{html}"
        );
        assert!(
            html.contains("href=\"https://music.163.com/song?id=1\""),
            "{html}"
        );
        assert!(!html.contains("example.invalid"), "{html}");
        assert_eq!(html.matches("Source page").count(), 1, "{html}");
        assert!(!html.contains("Lossy"), "{html}");
    }

    #[test]
    fn lossy_groups_carry_the_chip_after_one_heading() {
        let html = render(
            vec![
                group("slskd", None),
                group("ytdlp", None),
                group("ytdlp", None),
            ],
            vec![Some(&SLSKD), Some(&YOUTUBE), Some(&YOUTUBE)],
        );
        assert_eq!(
            html.matches("Lossy, from YouTube Music").count(),
            1,
            "{html}"
        );
        assert_eq!(html.matches("LOSSY · YOUTUBE MUSIC").count(), 2, "{html}");
        let heading = html.find("Lossy, from YouTube Music").unwrap();
        assert!(html.find("Come Over from slskd").unwrap() < heading);
        assert!(heading < html.find("Come Over from ytdlp").unwrap());
    }
}
