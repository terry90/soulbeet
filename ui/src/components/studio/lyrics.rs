//! Synced karaoke lyrics parsing, active-line binary lookup, and reactive [`LyricsPanel`].

use dioxus::prelude::*;
use shared::client::TrackLyrics;

use crate::{use_player, IconLoader, IconMic2, IconRefreshCw};

/// Normalized lyric line ready for synced or plain display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedLyricLine {
    /// Timestamp in milliseconds from track start when synced.
    pub start_ms: Option<i64>,
    pub text: String,
}

/// Parses an LRC timestamp prefix (`[mm:ss.xx]` or `[mm:ss.xxx]` or `[mm:ss]`) from a line.
fn parse_lrc_timestamp_ms(tag: &str) -> Option<i64> {
    let (min_str, sec_part) = tag.split_once(':')?;
    let mins: i64 = min_str.trim().parse().ok()?;
    let (secs_str, frac_str) = match sec_part.split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => match sec_part.split_once(':') {
            Some((s, f)) => (s, Some(f)),
            None => (sec_part, None),
        },
    };
    let secs: i64 = secs_str.trim().parse().ok()?;
    if mins < 0 || !(0..60).contains(&secs) {
        return None;
    }
    let frac_ms: i64 = match frac_str {
        None => 0,
        Some(f) => {
            let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
            match digits.len() {
                0 => 0,
                1 => digits.parse::<i64>().ok()? * 100,
                2 => digits.parse::<i64>().ok()? * 10,
                _ => digits[..3].parse::<i64>().ok()?,
            }
        }
    };
    Some(mins * 60_000 + secs * 1_000 + frac_ms)
}

/// Parses LRC-formatted text (supporting multiple timestamps per line, e.g. `[00:12.50][01:02.00]Chorus`)
/// or falls back to unsynced plain lines.
pub fn parse_lrc_lines(raw: &str) -> (bool, Vec<NormalizedLyricLine>) {
    let mut synced_lines = Vec::new();
    let mut plain_lines = Vec::new();

    for raw_line in raw.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut rest = trimmed;
        let mut timestamps = Vec::new();
        while let Some(after_open) = rest.strip_prefix('[') {
            if let Some((tag, remainder)) = after_open.split_once(']') {
                if let Some(ms) = parse_lrc_timestamp_ms(tag) {
                    timestamps.push(ms);
                    rest = remainder.trim_start();
                    continue;
                }
            }
            break;
        }
        if !timestamps.is_empty() {
            let text = rest.trim().to_string();
            for ts in timestamps {
                synced_lines.push(NormalizedLyricLine {
                    start_ms: Some(ts),
                    text: text.clone(),
                });
            }
        } else if !(trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.contains(':')) {
            plain_lines.push(NormalizedLyricLine {
                start_ms: None,
                text: trimmed.to_string(),
            });
        }
    }

    if !synced_lines.is_empty() {
        synced_lines.sort_by_key(|l| l.start_ms.unwrap_or(i64::MAX));
        (true, synced_lines)
    } else {
        (false, plain_lines)
    }
}

/// Selects the best [`TrackLyrics`] entry (preferring synced lyrics with lines),
/// applies `offset_ms`, and normalizes into `(is_synced, lines)`.
pub fn normalize_track_lyrics(entries: &[TrackLyrics]) -> (bool, Vec<NormalizedLyricLine>) {
    let chosen = entries
        .iter()
        .find(|e| e.synced && !e.lines.is_empty())
        .or_else(|| entries.iter().find(|e| !e.lines.is_empty()));

    let Some(entry) = chosen else {
        return (false, Vec::new());
    };

    let offset = entry.offset_ms.unwrap_or(0);
    let has_line_timestamps = entry.synced && entry.lines.iter().any(|l| l.start_ms.is_some());

    if has_line_timestamps {
        let mut lines: Vec<NormalizedLyricLine> = entry
            .lines
            .iter()
            .filter_map(|line| {
                let ts = line.start_ms.map(|ms| (ms + offset).max(0));
                if line.text.trim().is_empty() && ts.is_none() {
                    None
                } else {
                    Some(NormalizedLyricLine {
                        start_ms: ts,
                        text: line.text.clone(),
                    })
                }
            })
            .collect();
        lines.sort_by_key(|l| l.start_ms.unwrap_or(i64::MAX));
        return (true, lines);
    }

    // Plain lyrics might still carry inline LRC timestamps when sourced from legacy endpoints.
    if entry.lines.len() == 1 {
        let (is_lrc, parsed) = parse_lrc_lines(&entry.lines[0].text);
        if is_lrc {
            let shifted = parsed
                .into_iter()
                .map(|mut l| {
                    if let Some(ms) = l.start_ms {
                        l.start_ms = Some((ms + offset).max(0));
                    }
                    l
                })
                .collect();
            return (true, shifted);
        }
        return (false, parsed);
    }

    let plain: Vec<NormalizedLyricLine> = entry
        .lines
        .iter()
        .map(|l| NormalizedLyricLine {
            start_ms: None,
            text: l.text.clone(),
        })
        .collect();
    (false, plain)
}

/// Finds the active synced lyric line index for `position_ms` using binary search (`O(log n)`).
/// Returns `None` when `lines` has no timestamps or `position_ms` precedes the first timed line.
pub fn find_active_lyric_line(lines: &[NormalizedLyricLine], position_ms: i64) -> Option<usize> {
    if lines.is_empty() || position_ms < 0 {
        return None;
    }

    // Fast path for monotonically sorted synced lines (standard case from `normalize_track_lyrics`).
    let mut lo = 0usize;
    let mut hi = lines.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match lines[mid].start_ms {
            Some(ts) if ts <= position_ms => lo = mid + 1,
            _ => hi = mid,
        }
    }

    if lo == 0 {
        return None;
    }

    // Walk back past any untimed entries if present.
    let mut idx = lo - 1;
    loop {
        if let Some(ts) = lines[idx].start_ms {
            if ts <= position_ms {
                return Some(idx);
            }
        }
        if idx == 0 {
            return None;
        }
        idx -= 1;
    }
}

/// Interactive karaoke / plain lyrics drawer panel.
/// Subscribes directly to `player.position_signal()` so 4–10Hz audio ticks only re-render
/// this panel when visible, never the outer studio shell.
#[component]
pub fn LyricsPanel(
    #[props(into)] lyrics: ReadSignal<Vec<TrackLyrics>>,
    #[props(default)] loading: bool,
    #[props(default)] error: Option<String>,
    #[props(default)] has_track: bool,
    #[props(default)] on_fetch: Option<EventHandler<()>>,
) -> Element {
    let player = use_player();
    // Memoized so 4–10Hz `position_signal()` ticks never re-parse or re-sort lyrics.
    let normalized = use_memo(move || normalize_track_lyrics(&lyrics.read()));
    let (is_synced, lines) = normalized.read().clone();

    // Only subscribe to high-frequency position updates when synced lyrics are actually displayed.
    let active_idx = if is_synced && !lines.is_empty() {
        let pos_ms = (*player.position_signal().read() * 1000.0).round() as i64;
        find_active_lyric_line(&lines, pos_ms)
    } else {
        None
    };

    if !has_track {
        return rsx! {
            div { class: "h-full flex flex-col items-center justify-center text-center px-6 py-12 text-beet-text-tertiary space-y-2",
                IconMic2 { size: 28, class: "text-beet-text-disabled".to_string() }
                p { class: "text-[14px] font-medium text-beet-text-secondary",
                    "No track playing"
                }
                p { class: "text-[12px]",
                    "Play a track to follow synced karaoke lyrics here."
                }
            }
        };
    }

    if loading {
        return rsx! {
            div { class: "p-5 space-y-3",
                div { class: "flex items-center gap-2 text-[12px] tabular-nums-mono text-beet-accent-text",
                    IconLoader { size: 14 }
                    span { "FETCHING LYRICS…" }
                }
                for i in 0..7 {
                    div {
                        key: "{i}",
                        class: "h-4 rounded bg-beet-surface-high animate-pulse",
                        style: format!("width: {}%;", 55 + (i * 11) % 35),
                    }
                }
            }
        };
    }

    if let Some(err) = error {
        return rsx! {
            div { class: "h-full flex flex-col items-center justify-center text-center px-6 py-10 space-y-3",
                IconMic2 { size: 26, class: "text-beet-crimson-text".to_string() }
                p { class: "text-[13px] text-beet-text-secondary", "{err}" }
                if let Some(fetch_cb) = on_fetch {
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-8 px-3.5 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-highest text-[12px] font-medium cursor-pointer transition-colors",
                        onclick: move |_| fetch_cb.call(()),
                        IconRefreshCw { size: 13 }
                        span { "Retry" }
                    }
                }
            }
        };
    }

    if lines.is_empty() {
        return rsx! {
            div { class: "h-full flex flex-col items-center justify-center text-center px-6 py-12 space-y-3",
                IconMic2 { size: 28, class: "text-beet-text-disabled".to_string() }
                div { class: "space-y-1",
                    p { class: "text-[14px] font-medium text-beet-text-primary",
                        "No synced lyrics saved yet"
                    }
                    p { class: "text-[12px] text-beet-text-tertiary",
                        "Check Navidrome for embedded or synced lyrics for this track."
                    }
                }
                if let Some(fetch_cb) = on_fetch {
                    button {
                        r#type: "button",
                        class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-primary hover:border-beet-accent/40 hover:text-beet-accent-text text-[13px] font-medium cursor-pointer transition-colors",
                        onclick: move |_| fetch_cb.call(()),
                        IconRefreshCw { size: 14 }
                        span { "Fetch Lyrics" }
                    }
                }
            }
        };
    }

    rsx! {
        div { class: "p-4 space-y-4",
            div { class: "flex items-center justify-between gap-2 pb-2 border-b border-beet-border-subtle",
                span { class: "inline-flex items-center gap-1.5 tabular-nums-mono text-[11px] font-semibold uppercase tracking-wider text-beet-text-tertiary",
                    if is_synced {
                        span { class: "w-1.5 h-1.5 rounded-full bg-beet-leaf-glow" }
                        "SYNCED · CLICK LINE TO SEEK"
                    } else {
                        "PLAIN LYRICS"
                    }
                }
                if let Some(fetch_cb) = on_fetch {
                    button {
                        r#type: "button",
                        title: "Refresh lyrics",
                        aria_label: "Refresh lyrics",
                        class: "w-7 h-7 rounded-md inline-flex items-center justify-center text-beet-text-tertiary hover:text-beet-text-primary hover:bg-beet-surface-high transition-colors cursor-pointer",
                        onclick: move |_| fetch_cb.call(()),
                        IconRefreshCw { size: 13 }
                    }
                }
            }

            div { class: "space-y-1.5 pb-12",
                for (idx, line) in lines.iter().enumerate() {
                    {
                        let is_active = active_idx == Some(idx);
                        let is_past = active_idx.is_some_and(|a| idx < a);
                        let display_text = if line.text.trim().is_empty() {
                            "···".to_string()
                        } else {
                            line.text.clone()
                        };
                        if let Some(start_ms) = line.start_ms {
                            let seek_secs = (start_ms as f64) / 1000.0;
                            let line_class = if is_active {
                                "text-beet-leaf-text font-semibold text-[16px] bg-beet-leaf-surface/70 border-l-2 border-beet-leaf-solid pl-3 py-2"
                            } else if is_past {
                                "text-beet-text-disabled hover:text-beet-text-secondary text-[14px] pl-3 py-1.5"
                            } else {
                                "text-beet-text-secondary hover:text-beet-text-primary text-[14px] pl-3 py-1.5"
                            };
                            rsx! {
                                button {
                                    key: "{idx}-{start_ms}",
                                    r#type: "button",
                                    class: "w-full text-left rounded-[6px] leading-[1.45] transition-colors duration-150 cursor-pointer {line_class}",
                                    onclick: move |_| player.seek(seek_secs),
                                    "{display_text}"
                                }
                            }
                        } else {
                            rsx! {
                                p {
                                    key: "{idx}",
                                    class: "text-[14px] text-beet-text-secondary leading-[1.55] px-2 py-0.5",
                                    "{display_text}"
                                }
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
    use shared::client::TrackLyricLine;

    #[test]
    fn parses_lrc_lines_including_multi_timestamp_and_metadata_tags() {
        let raw = "\
[ar:Daft Punk]
[ti:Instant Crush]
[00:10.50]I didn't want to be the one to forget
[00:15.00][01:15.25]And we will never be alone again
[00:22.125]Because it doesn't happen every day
";
        let (synced, lines) = parse_lrc_lines(raw);
        assert!(synced);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].start_ms, Some(10_500));
        assert_eq!(lines[1].start_ms, Some(15_000));
        assert_eq!(lines[2].start_ms, Some(22_125));
        assert_eq!(lines[3].start_ms, Some(75_250));
        assert_eq!(lines[1].text, lines[3].text);
    }

    #[test]
    fn normalizes_structured_lyrics_with_offset_and_prefers_synced() {
        let entries = vec![
            TrackLyrics {
                display_artist: None,
                display_title: None,
                lang: "en".into(),
                synced: false,
                offset_ms: None,
                lines: vec![TrackLyricLine {
                    start_ms: None,
                    text: "Plain line".into(),
                }],
            },
            TrackLyrics {
                display_artist: Some("Artist".into()),
                display_title: Some("Title".into()),
                lang: "en".into(),
                synced: true,
                offset_ms: Some(500),
                lines: vec![
                    TrackLyricLine {
                        start_ms: Some(5_000),
                        text: "Second".into(),
                    },
                    TrackLyricLine {
                        start_ms: Some(1_000),
                        text: "First".into(),
                    },
                ],
            },
        ];

        let (synced, lines) = normalize_track_lyrics(&entries);
        assert!(synced);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].start_ms, Some(1_500));
        assert_eq!(lines[0].text, "First");
        assert_eq!(lines[1].start_ms, Some(5_500));
        assert_eq!(lines[1].text, "Second");
    }

    #[test]
    fn finds_active_lyric_line_accurately_across_boundaries() {
        let lines = vec![
            NormalizedLyricLine {
                start_ms: Some(1_000),
                text: "Line 0".into(),
            },
            NormalizedLyricLine {
                start_ms: Some(5_000),
                text: "Line 1".into(),
            },
            NormalizedLyricLine {
                start_ms: Some(12_500),
                text: "Line 2".into(),
            },
        ];

        assert_eq!(find_active_lyric_line(&lines, -100), None);
        assert_eq!(find_active_lyric_line(&lines, 0), None);
        assert_eq!(find_active_lyric_line(&lines, 999), None);
        assert_eq!(find_active_lyric_line(&lines, 1_000), Some(0));
        assert_eq!(find_active_lyric_line(&lines, 4_999), Some(0));
        assert_eq!(find_active_lyric_line(&lines, 5_000), Some(1));
        assert_eq!(find_active_lyric_line(&lines, 12_499), Some(1));
        assert_eq!(find_active_lyric_line(&lines, 12_500), Some(2));
        assert_eq!(find_active_lyric_line(&lines, 99_000), Some(2));
    }
}
