use dioxus::logger::tracing::info;
use dioxus::prelude::*;
use shared::backends::{BackendMeta, Delivery};
use shared::download::{DownloadableGroup, DownloadableItem};
use std::collections::HashSet;

use crate::{use_auth, Checkbox, LossyChip};

/// Live polling state of one download backend in a manual source search.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendPollState {
    Searching,
    Done,
    Failed,
}

/// Live status of a download backend participating in a manual source search.
#[derive(Clone, PartialEq, Debug)]
pub struct BackendSearchStatus {
    pub id: String,
    pub name: String,
    pub delivery: Delivery,
    pub state: BackendPollState,
    pub group_count: usize,
    pub warning: Option<String>,
}

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

/// The badge text and Midnight Studio color tokens of a file, from the facts its backend reported.
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
        "flac" | "wav" => "bg-beet-leaf-surface text-beet-leaf-text border-beet-leaf/30",
        "m4a" | "aac" | "ogg" | "opus" => {
            "bg-beet-accent-surface text-beet-accent-text border-beet-accent/30"
        }
        _ => "bg-beet-surface-high text-beet-text-secondary border-beet-border-default",
    };
    (label, color)
}

fn quality_badge(item: &DownloadableItem) -> Element {
    let (label, color) = quality_label(item);
    rsx! {
        span {
            class: "text-[10px] tabular-nums-mono font-semibold px-1.5 py-0.5 rounded border {color} shrink-0",
            "{label}"
        }
    }
}

/// The registry backend ID of a group's items, when non-empty.
pub fn group_backend_id(group: &DownloadableGroup) -> Option<&str> {
    group
        .items
        .first()
        .map(|i| i.backend.as_str())
        .filter(|b| !b.is_empty())
}

/// The backend a group's files came from, when this build knows it.
fn backend_of(group: &DownloadableGroup) -> Option<&'static BackendMeta> {
    group_backend_id(group).and_then(shared::backends::meta)
}

/// Sorts merged multi-backend groups: `Delivery::AsFound` (lossless/original) first
/// ordered by score descending, then `Delivery::LossyOnly` ordered by score descending.
pub fn sort_merged_groups(groups: &mut [DownloadableGroup]) {
    let delivery_rank = |group: &DownloadableGroup| match group_backend_id(group)
        .map(shared::backends::delivery)
    {
        Some(Delivery::LossyOnly) => 1u8,
        _ => 0u8,
    };
    groups.sort_by(|a, b| {
        delivery_rank(a).cmp(&delivery_rank(b)).then_with(|| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
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
    /// Live per-backend polling statuses (for multi-backend filter pills and inline warnings).
    #[props(default)]
    pub backend_statuses: Vec<BackendSearchStatus>,
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
    format!("{}::{}::{}", track.backend, track.id, track.source)
}

#[component]
fn TrackItem(props: TrackItemProps) -> Element {
    let unique_id = get_track_id(&props.track);

    rsx! {
        li {
            key: "{unique_id}",
            class: "flex items-center gap-2.5 px-2 py-1.5 rounded-[8px] hover:bg-beet-surface-high cursor-pointer transition-colors",
            onclick: move |_| props.on_toggle.call(unique_id.clone()),

            Checkbox { is_selected: props.is_selected }

            label { class: "cursor-pointer text-beet-text-primary text-[13px] flex-1 min-w-0 truncate",
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
        // Keep `rounded-md` on the group card container for stable E2E group selection.
        div {
            class: "bg-beet-surface-low border border-beet-border-subtle hover:border-beet-border-default p-3.5 sm:p-4 rounded-md transition-colors",

            // Clickable header for expand/collapse
            div {
                class: "flex justify-between items-center gap-3 cursor-pointer",
                onclick: move |_| is_expanded.toggle(),

                div { class: "flex-grow min-w-0",
                    div { class: "flex flex-wrap items-center gap-1.5 mb-2",
                        if props.is_best {
                            span {
                                class: "text-[10px] tabular-nums-mono font-semibold px-2 py-0.5 rounded-full border bg-beet-accent-surface text-beet-accent-text border-beet-accent/40",
                                "Best match"
                            }
                        }
                        if let Some(meta) = props.backend {
                            span {
                                class: "text-[10px] tabular-nums-mono font-semibold px-2 py-0.5 rounded-full border bg-beet-surface-high text-beet-text-secondary border-beet-border-default",
                                "{meta.name}"
                            }
                            LossyChip { meta }
                        }
                    }
                    h4 { class: "text-[15px] leading-snug font-semibold text-beet-leaf-text break-words",
                        "{album.title}"
                    }
                    if let Some(artist) = album.artist.as_ref().filter(|a| !a.is_empty()) {
                        p { class: "mt-0.5 text-[13px] text-beet-text-secondary break-words", "{artist}" }
                    }
                    p { class: "mt-1 text-[11px] text-beet-text-tertiary tabular-nums-mono uppercase tracking-wide",
                        "{album.quality} · Score {album.score:.2}"
                    }
                    if let Some(attribution) = props.backend.and_then(|m| m.attribution) {
                        p { class: "mt-1 text-[11px] text-beet-text-tertiary tabular-nums-mono break-words",
                            "{attribution}"
                        }
                    }
                    if let Some(url) = source_page {
                        a {
                            class: "mt-1 inline-block text-[12px] text-beet-leaf-text hover:underline decoration-dotted",
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
                    class: if *is_expanded.read() {
                        "rotate-90 text-beet-text-secondary"
                    } else {
                        "text-beet-text-tertiary"
                    },
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
                    r#type: "button",
                    class: "tabular-nums-mono uppercase text-[10px] whitespace-nowrap tracking-wider font-semibold px-3 py-1.5 mt-3 mb-2 border border-beet-leaf/30 text-beet-leaf-text hover:bg-beet-leaf-solid hover:text-beet-leaf-ink transition-colors cursor-pointer rounded-[8px]",
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
    let lossy_header_meta = first_lossy.and_then(|idx| backends.get(idx).copied().flatten());
    let mut head: Vec<(usize, DownloadableGroup, Option<&'static BackendMeta>)> = groups
        .into_iter()
        .zip(backends)
        .enumerate()
        .map(|(idx, (album, backend))| (idx, album, backend))
        .collect();
    let split_at = first_lossy.unwrap_or(head.len());
    let tail = head.split_off(split_at);

    rsx! {
        for (idx, album, backend) in head {
            AlbumResultItem {
                key: "{album.group_id}-{album.source}",
                backend,
                album,
                selected_tracks,
                on_album_select_all,
                on_track_toggle,
                is_best: idx == 0,
                starts_expanded: idx == 0,
            }
        }
        if let Some(meta) = lossy_header_meta {
            h4 { class: "pt-2 text-[13px] tabular-nums-mono font-semibold text-beet-gold",
                "Lossy, from {meta.name}"
            }
        }
        for (idx, album, backend) in tail {
            AlbumResultItem {
                key: "{album.group_id}-{album.source}",
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

/// Builds the list of backend filter entries (id, display name, group count, live state)
/// from explicit `backend_statuses` when present, or inferred from `results`.
fn backend_filter_entries(
    results: &[DownloadableGroup],
    statuses: &[BackendSearchStatus],
) -> Vec<(String, String, usize, BackendPollState)> {
    if !statuses.is_empty() {
        return statuses
            .iter()
            .map(|st| {
                let count = results
                    .iter()
                    .filter(|g| group_backend_id(g) == Some(st.id.as_str()))
                    .count();
                (st.id.clone(), st.name.clone(), count, st.state)
            })
            .collect();
    }

    let mut entries: Vec<(String, String, usize, BackendPollState)> = Vec::new();
    for group in results {
        let Some(id) = group_backend_id(group) else {
            continue;
        };
        if let Some(entry) = entries.iter_mut().find(|(eid, _, _, _)| eid == id) {
            entry.2 += 1;
        } else {
            entries.push((
                id.to_string(),
                shared::backends::name(id).to_string(),
                1,
                BackendPollState::Done,
            ));
        }
    }
    entries
}

/// Main component responsible for displaying multi-backend manual download options.
#[component]
pub fn DownloadResults(props: Props) -> Element {
    let mut selected_tracks = use_signal(HashSet::<String>::new);
    let mut active_backend_filter = use_signal(|| None::<String>);
    let results = props.results.clone();
    let mut folders = use_signal(std::vec::Vec::new);
    let mut selected_folder = use_signal(String::new);
    let mut is_downloading = props.is_downloading;
    let auth = use_auth();

    use_future(move || async move {
        if let Ok(user_folders) = auth.call(api::get_user_folders()).await {
            info!("Fetched {} user folders", user_folders.len());

            // Only select if the user has exactly one folder
            // It could be error prone to auto-select if there are multiple folders
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

    let filter_entries = backend_filter_entries(&results, &props.backend_statuses);
    let show_backend_filters = filter_entries.len() > 1;
    let active_filter = active_backend_filter();
    let visible_results: Vec<DownloadableGroup> = match active_filter.as_deref() {
        Some(bid) => results
            .iter()
            .filter(|g| group_backend_id(g) == Some(bid))
            .cloned()
            .collect(),
        None => results.clone(),
    };
    let warnings: Vec<(&str, &str)> = props
        .backend_statuses
        .iter()
        .filter_map(|st| st.warning.as_deref().map(|w| (st.name.as_str(), w)))
        .collect();
    let selected_count = selected_tracks.read().len();

    rsx! {
        div { class: "bg-beet-surface border border-beet-border-subtle text-beet-text-primary p-4 sm:p-6 rounded-[16px] w-full max-w-3xl mx-auto font-display relative space-y-5",
            // Header
            div { class: "relative flex items-center justify-center min-h-[40px]",
                button {
                    r#type: "button",
                    class: "absolute left-0 top-1/2 -translate-y-1/2 p-2 rounded-full hover:bg-beet-surface-high text-beet-text-secondary hover:text-beet-text-primary transition-colors cursor-pointer",
                    aria_label: "Back",
                    onclick: move |_| props.on_back.call(()),
                    svg {
                        class: "w-5 h-5",
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
                h3 { class: "text-[20px] font-bold text-center text-beet-text-primary",
                    "Download Options"
                }
            }

            // Dynamic Backend Filter Pills (when >1 backend is enabled/present)
            if show_backend_filters {
                div { class: "flex flex-wrap items-center gap-2",
                    {
                        let all_active = active_filter.is_none();
                        let all_cls = if all_active {
                            "bg-beet-accent-surface border-beet-accent/40 text-beet-accent-text"
                        } else {
                            "bg-beet-surface-high border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary"
                        };
                        rsx! {
                            button {
                                r#type: "button",
                                class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full border text-[12px] font-medium transition-colors cursor-pointer {all_cls}",
                                onclick: move |_| active_backend_filter.set(None),
                                "All Sources ({results.len()})"
                            }
                        }
                    }
                    for (bid, name, count, state) in filter_entries.iter() {
                        {
                            let bid_click = bid.clone();
                            let is_active = active_filter.as_deref() == Some(bid.as_str());
                            let pill_cls = if is_active {
                                "bg-beet-accent-surface border-beet-accent/40 text-beet-accent-text"
                            } else {
                                "bg-beet-surface-high border-beet-border-subtle text-beet-text-secondary hover:text-beet-text-primary"
                            };
                            rsx! {
                                button {
                                    key: "backend-filter-{bid}",
                                    r#type: "button",
                                    class: "inline-flex items-center gap-1.5 h-8 px-3 rounded-full border text-[12px] font-medium transition-colors cursor-pointer {pill_cls}",
                                    onclick: move |_| active_backend_filter.set(Some(bid_click.clone())),
                                    if *state == BackendPollState::Searching {
                                        span {
                                            class: "size-2.5 rounded-full border-[1.5px] border-beet-border-default border-t-beet-leaf-text animate-spin shrink-0",
                                            aria_hidden: "true",
                                        }
                                    } else if *state == BackendPollState::Failed {
                                        span {
                                            class: "size-1.5 rounded-full bg-beet-gold shrink-0",
                                            aria_hidden: "true",
                                        }
                                    }
                                    span { "{name} ({count})" }
                                }
                            }
                        }
                    }
                }
            }

            // Non-fatal per-backend warning pills
            if !warnings.is_empty() {
                div { class: "space-y-1.5",
                    for (name, msg) in warnings {
                        div {
                            key: "warn-{name}",
                            class: "rounded-[10px] bg-beet-gold-surface border border-beet-gold/30 px-3 py-2 text-[12px] text-beet-gold",
                            span { class: "font-semibold", "{name}: " }
                            span { "{msg}" }
                        }
                    }
                }
            }

            // Target folder selector
            div {
                label {
                    r#for: "dl_folder",
                    class: "block text-[12px] font-medium mb-1.5 text-beet-text-secondary",
                    "Select Target Folder"
                }
                select {
                    id: "dl_folder",
                    name: "dl_folder",
                    class: "w-full h-10 px-3 rounded-[10px] bg-beet-surface-low border border-beet-border-default focus:border-beet-border-strong text-[13px] text-beet-text-primary outline-none transition-colors",
                    value: "{selected_folder}",
                    onchange: move |e| selected_folder.set(e.value()),
                    option { value: "", disabled: true, "Select a folder" }
                    for folder in folders.read().iter() {
                        option { value: "{folder.path}", "{folder.name}" }
                    }
                }
            }

            // Live search status & candidate groups
            div { class: "space-y-3",
                if props.is_searching {
                    div { class: "relative overflow-hidden flex flex-col items-center justify-center p-4 bg-beet-surface-low border border-beet-border-subtle rounded-[12px] gap-2",
                        div { class: "size-6 animate-spin rounded-full border-2 border-beet-border-default border-t-beet-leaf-text" }
                        p { class: "text-[13px] text-beet-text-secondary text-center",
                            "Searching enabled sources… Results appear as each backend responds."
                        }
                        span {
                            class: "beet-seam-light absolute inset-x-0 bottom-0",
                            aria_hidden: "true",
                        }
                    }
                } else if visible_results.is_empty() {
                    div { class: "text-center text-beet-text-tertiary py-8 text-[13px]",
                        p { "No results found" }
                        if let Some(note) = &props.unanswered_note {
                            p { class: "text-[12px] mt-2 tabular-nums-mono", "{note}" }
                        }
                    }
                }
                GroupList {
                    backends: visible_results.iter().map(backend_of).collect::<Vec<_>>(),
                    groups: visible_results,
                    selected_tracks,
                    on_album_select_all: handle_album_select_all,
                    on_track_toggle: handle_track_toggle,
                }
            }

            // Floating Studio "Start download" button (positioned above the 84px Studio player bar)
            div { class: "fixed bottom-28 md:bottom-24 right-6 z-40",
                button {
                    r#type: "button",
                    class: "inline-flex items-center justify-center gap-2 h-12 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink font-semibold text-[14px] shadow-lg hover:brightness-105 transition-all disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer",
                    aria_label: "Start download",
                    disabled: selected_count == 0 || selected_folder.read().is_empty() || *is_downloading.read(),
                    onclick: handle_download,
                    if *is_downloading.read() {
                        div { class: "animate-spin rounded-full h-5 w-5 border-2 border-beet-leaf-ink/30 border-t-beet-leaf-ink" }
                        span { "Starting…" }
                    } else {
                        svg {
                            class: "w-5 h-5",
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
                        if selected_count > 0 {
                            span { "Download ({selected_count})" }
                        } else {
                            span { "Start download" }
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
    use shared::backends::{Switch, ARCHIVE, SLSKD};
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
        group_scored(backend, url, 0.9)
    }

    fn group_scored(backend: &str, url: Option<&str>, score: f64) -> DownloadableGroup {
        let mut facts = facts("flac");
        if let Some(url) = url {
            facts.backend_facts.insert("item_url".into(), url.into());
        }
        DownloadableGroup {
            source: format!("{backend}-source"),
            group_id: format!("{backend}/group-{score}"),
            title: format!("Come Over from {backend}"),
            artist: Some("Kowloon".into()),
            item_count: 1,
            total_size: 1,
            items: vec![item(backend, facts)],
            quality: "flac".into(),
            score,
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
    fn every_known_backend_shows_its_badge_and_source_page_when_https() {
        let html = render(
            vec![
                group("slskd", None),
                group("gdstudio", Some("https://music.163.com/song?id=1")),
                group("other", Some("http://example.invalid/item")),
            ],
            vec![Some(&SLSKD), Some(&GD), None],
        );
        assert!(html.contains(">Soulseek</span>"), "{html}");
        assert!(html.contains(">NetEase (GD Studio)</span>"), "{html}");
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

    #[test]
    fn sort_merged_groups_places_as_found_before_lossy_by_score_desc() {
        let mut groups = vec![
            group_scored("ytdlp", None, 0.99),
            group_scored("archive", None, 0.72),
            group_scored("slskd", None, 0.91),
            group_scored("ytdlp", None, 0.65),
        ];
        sort_merged_groups(&mut groups);
        let order: Vec<(&str, f64)> = groups
            .iter()
            .map(|g| (group_backend_id(g).unwrap_or(""), g.score))
            .collect();
        assert_eq!(
            order,
            vec![
                ("slskd", 0.91),
                ("archive", 0.72),
                ("ytdlp", 0.99),
                ("ytdlp", 0.65),
            ]
        );
    }

    #[test]
    fn backend_filter_entries_preserve_status_order_and_counts() {
        let groups = vec![
            group_scored("slskd", None, 0.9),
            group_scored("archive", None, 0.8),
            group_scored("archive", None, 0.7),
        ];
        let statuses = vec![
            BackendSearchStatus {
                id: SLSKD.id.into(),
                name: SLSKD.name.into(),
                delivery: SLSKD.delivery,
                state: BackendPollState::Searching,
                group_count: 1,
                warning: None,
            },
            BackendSearchStatus {
                id: ARCHIVE.id.into(),
                name: ARCHIVE.name.into(),
                delivery: ARCHIVE.delivery,
                state: BackendPollState::Done,
                group_count: 2,
                warning: None,
            },
        ];
        let entries = backend_filter_entries(&groups, &statuses);
        assert_eq!(
            entries,
            vec![
                (
                    "slskd".to_string(),
                    "Soulseek".to_string(),
                    1,
                    BackendPollState::Searching
                ),
                (
                    "archive".to_string(),
                    "Internet Archive".to_string(),
                    2,
                    BackendPollState::Done
                ),
            ]
        );
    }
}
