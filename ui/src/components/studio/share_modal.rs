//! Modal for sharing a personal library track or album with other household users
//! via zero-copy hardlinks (or reflinks / copies across filesystems).

use std::collections::HashSet;

use api::{ShareResultEntryDto, ShareTracksInput, ShareTracksResponseDto};
use dioxus::prelude::*;

use crate::{
    friendly_error, use_auth, IconCheck, IconDisc3, IconLoader, IconMusic, IconShare2, Modal,
};

/// What the user is sharing from the Studio UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareModalTarget {
    Track {
        song_id: String,
        title: String,
        artist: String,
        album: Option<String>,
    },
    Album {
        album_id: String,
        title: String,
        artist: String,
        track_count: usize,
    },
}

/// Formats a single share entry into a human-friendly outcome message.
pub fn format_share_entry_summary(entry: &ShareResultEntryDto) -> String {
    match entry.status.as_str() {
        "shared" => {
            let mode_detail = match entry.link_mode.as_deref() {
                Some("hardlink") => "hardlink, 0 B used",
                Some("reflink") => "reflink, 0 B used",
                Some("copy") => "copy",
                Some(other) => other,
                None => "linked",
            };
            format!("Shared with {} ({mode_detail})", entry.recipient_username)
        }
        "would_share" => format!("Would share with {}", entry.recipient_username),
        "already_in_library" => format!("Already in {}'s library", entry.recipient_username),
        _ => {
            let reason = entry
                .error
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Could not share track");
            format!("{}: {reason}", entry.recipient_username)
        }
    }
}

/// Aggregated per-recipient summary when sharing multiple tracks (e.g. a whole album).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecipientBatchSummary {
    username: String,
    shared: usize,
    already_in_library: usize,
    failed: usize,
    link_modes: Vec<String>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

fn summarize_by_recipient(entries: &[ShareResultEntryDto]) -> Vec<RecipientBatchSummary> {
    let mut out: Vec<RecipientBatchSummary> = Vec::new();
    for entry in entries {
        let bucket = match out
            .iter_mut()
            .find(|b| b.username == entry.recipient_username)
        {
            Some(existing) => existing,
            None => {
                out.push(RecipientBatchSummary {
                    username: entry.recipient_username.clone(),
                    shared: 0,
                    already_in_library: 0,
                    failed: 0,
                    link_modes: Vec::new(),
                    errors: Vec::new(),
                    warnings: Vec::new(),
                });
                out.last_mut().expect("just pushed")
            }
        };
        match entry.status.as_str() {
            "shared" | "would_share" => {
                bucket.shared += 1;
                if let Some(ref mode) = entry.link_mode {
                    if !bucket.link_modes.iter().any(|m| m == mode) {
                        bucket.link_modes.push(mode.clone());
                    }
                }
            }
            "already_in_library" => bucket.already_in_library += 1,
            _ => {
                bucket.failed += 1;
                if let Some(ref err) = entry.error {
                    let msg = format!("{}: {err}", entry.title);
                    if !bucket.errors.iter().any(|e| e == &msg) {
                        bucket.errors.push(msg);
                    }
                }
            }
        }
        for w in &entry.warnings {
            if !bucket.warnings.iter().any(|existing| existing == w) {
                bucket.warnings.push(w.clone());
            }
        }
    }
    out
}

fn format_recipient_batch_line(summary: &RecipientBatchSummary) -> String {
    let mut parts = Vec::new();
    if summary.shared > 0 {
        let mode_label = match summary.link_modes.as_slice() {
            [only] if only == "hardlink" => " (hardlink, 0 B used)".to_string(),
            [only] if only == "reflink" => " (reflink, 0 B used)".to_string(),
            [only] if only == "copy" => " (copy)".to_string(),
            [] => String::new(),
            modes => format!(" ({})", modes.join(", ")),
        };
        let noun = if summary.shared == 1 { "track" } else { "tracks" };
        parts.push(format!("{} {noun} shared{mode_label}", summary.shared));
    }
    if summary.already_in_library > 0 {
        parts.push(format!("{} already in library", summary.already_in_library));
    }
    if summary.failed > 0 {
        parts.push(format!("{} failed", summary.failed));
    }
    if parts.is_empty() {
        format!("{}: no changes", summary.username)
    } else {
        format!("{}: {}", summary.username, parts.join(", "))
    }
}

/// Modal dialog for sharing a track or album with other Soulbeet users.
#[component]
pub fn ShareModal(target: ShareModalTarget, on_close: EventHandler<()>) -> Element {
    let auth = use_auth();

    let mut recipients_res =
        use_resource(move || async move { auth.call(api::get_share_recipients()).await });
    let mut selected_ids = use_signal(HashSet::<String>::new);
    let mut sharing = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut response = use_signal(|| None::<ShareTracksResponseDto>);

    let target_for_submit = target.clone();
    let is_album = matches!(target, ShareModalTarget::Album { .. });

    let (heading, subtitle) = match &target {
        ShareModalTarget::Track {
            title,
            artist,
            album,
            ..
        } => {
            let sub = match album.as_deref().filter(|a| !a.trim().is_empty()) {
                Some(alb) => format!("{artist} · {alb}"),
                None => artist.clone(),
            };
            (title.clone(), sub)
        }
        ShareModalTarget::Album {
            title,
            artist,
            track_count,
            ..
        } => {
            let noun = if *track_count == 1 { "track" } else { "tracks" };
            (title.clone(), format!("{artist} · {track_count} {noun}"))
        }
    };

    let selected_count = selected_ids.read().len();
    let can_share = selected_count > 0 && !sharing() && response.read().is_none();

    let run_share = move |_| {
        let recipient_user_ids: Vec<String> = selected_ids.peek().iter().cloned().collect();
        if recipient_user_ids.is_empty() || *sharing.peek() {
            return;
        }
        let input = match target_for_submit.clone() {
            ShareModalTarget::Track { song_id, .. } => ShareTracksInput {
                song_ids: vec![song_id],
                album_id: None,
                recipient_user_ids,
                dry_run: false,
            },
            ShareModalTarget::Album { album_id, .. } => ShareTracksInput {
                song_ids: Vec::new(),
                album_id: Some(album_id),
                recipient_user_ids,
                dry_run: false,
            },
        };
        sharing.set(true);
        error.set(None);
        spawn(async move {
            match auth.call(api::share_tracks_fn(input)).await {
                Ok(resp) => response.set(Some(resp)),
                Err(e) => error.set(Some(friendly_error(&e))),
            }
            sharing.set(false);
        });
    };

    let footer = if response.read().is_some() {
        rsx! {
            div { class: "flex items-center justify-end gap-2",
                button {
                    r#type: "button",
                    class: "h-9 px-4 rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold cursor-pointer hover:brightness-105 transition-[filter]",
                    onclick: move |_| on_close.call(()),
                    "Done"
                }
            }
        }
    } else {
        rsx! {
            div { class: "flex items-center justify-end gap-2",
                button {
                    r#type: "button",
                    disabled: sharing(),
                    class: "h-9 px-3.5 rounded-[10px] bg-beet-surface border border-beet-border-subtle text-[13px] text-beet-text-secondary hover:text-beet-text-primary cursor-pointer disabled:opacity-45",
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
                button {
                    r#type: "button",
                    disabled: !can_share,
                    class: "inline-flex items-center gap-1.5 h-9 px-4 rounded-[10px] bg-beet-leaf-solid text-beet-leaf-ink text-[13px] font-semibold hover:brightness-105 transition-[filter,opacity] cursor-pointer disabled:opacity-45 disabled:cursor-not-allowed",
                    onclick: run_share,
                    if sharing() {
                        IconLoader { size: 14 }
                        span { "Sharing..." }
                    } else {
                        IconShare2 { size: 14 }
                        if selected_count > 1 {
                            span { "Share with {selected_count} users" }
                        } else {
                            span { "Share" }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        Modal {
            on_close,
            header: rsx! {
                div { class: "space-y-0.5",
                    h2 { class: "text-[16px] font-semibold text-beet-text-primary flex items-center gap-2",
                        IconShare2 { size: 16, class: "text-beet-leaf-text".to_string() }
                        span { if is_album { "Share Album" } else { "Share Track" } }
                    }
                    p { class: "text-[12px] text-beet-text-tertiary",
                        "Links directly into recipients' personal libraries with zero duplicate storage."
                    }
                }
            },
            footer: Some(footer),

            div { class: "space-y-4",
                // Target summary card
                div { class: "flex items-center gap-3 p-3 rounded-[10px] bg-beet-surface border border-beet-border-subtle",
                    div { class: "w-10 h-10 rounded-[8px] bg-beet-surface-high border border-beet-border-subtle flex items-center justify-center shrink-0 text-beet-leaf-text",
                        if is_album {
                            IconDisc3 { size: 18 }
                        } else {
                            IconMusic { size: 18 }
                        }
                    }
                    div { class: "min-w-0 flex-1",
                        p { class: "text-[14px] font-semibold text-beet-text-primary truncate",
                            "{heading}"
                        }
                        p { class: "text-[12px] text-beet-text-secondary truncate",
                            "{subtitle}"
                        }
                    }
                }

                if let Some(err) = error() {
                    div { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 px-3.5 py-2.5 text-[12px] text-beet-crimson-text break-words",
                        "{err}"
                    }
                }

                // Result view or recipient selector
                if let Some(resp) = response.read().clone() {
                    div { class: "space-y-3",
                        div { class: "flex flex-wrap items-center gap-2",
                            if resp.shared_count > 0 {
                                span { class: "inline-flex items-center gap-1 px-2.5 py-1 rounded-full bg-beet-leaf-surface border border-beet-leaf-solid/30 text-beet-leaf-text text-[11px] font-medium tabular-nums-mono",
                                    IconCheck { size: 12 }
                                    "{resp.shared_count} shared"
                                }
                            }
                            if resp.already_in_library_count > 0 {
                                span { class: "inline-flex items-center gap-1 px-2.5 py-1 rounded-full bg-beet-surface-high border border-beet-border-default text-beet-text-secondary text-[11px] font-medium tabular-nums-mono",
                                    "{resp.already_in_library_count} already in library"
                                }
                            }
                            if resp.failed_count > 0 {
                                span { class: "inline-flex items-center gap-1 px-2.5 py-1 rounded-full bg-beet-crimson-surface border border-beet-crimson-text/30 text-beet-crimson-text text-[11px] font-medium tabular-nums-mono",
                                    "{resp.failed_count} failed"
                                }
                            }
                        }

                        if !is_album {
                            div { class: "space-y-1.5",
                                for entry in resp.entries.iter() {
                                    {
                                        let line = format_share_entry_summary(entry);
                                        let row_cls = match entry.status.as_str() {
                                            "shared" => "bg-beet-leaf-surface/60 border-beet-leaf-solid/30 text-beet-leaf-text",
                                            "already_in_library" => "bg-beet-surface border-beet-border-subtle text-beet-text-secondary",
                                            _ => "bg-beet-crimson-surface/60 border-beet-crimson-text/30 text-beet-crimson-text",
                                        };
                                        rsx! {
                                            div {
                                                key: "res-{entry.recipient_user_id}-{entry.song_id}",
                                                class: "px-3 py-2 rounded-[8px] border text-[12px] space-y-1 {row_cls}",
                                                p { class: "font-medium", "{line}" }
                                                for w in entry.warnings.iter() {
                                                    p { class: "text-[11px] text-beet-gold", "{w}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            {
                                let summaries = summarize_by_recipient(&resp.entries);
                                rsx! {
                                    div { class: "space-y-1.5 max-h-60 overflow-y-auto studio-scrollbar pr-1",
                                        for summary in summaries.iter() {
                                            {
                                                let line = format_recipient_batch_line(summary);
                                                let row_cls = if summary.failed > 0 && summary.shared == 0 {
                                                    "bg-beet-crimson-surface/60 border-beet-crimson-text/30 text-beet-crimson-text"
                                                } else if summary.shared > 0 {
                                                    "bg-beet-leaf-surface/60 border-beet-leaf-solid/30 text-beet-leaf-text"
                                                } else {
                                                    "bg-beet-surface border-beet-border-subtle text-beet-text-secondary"
                                                };
                                                rsx! {
                                                    div {
                                                        key: "sum-{summary.username}",
                                                        class: "px-3 py-2 rounded-[8px] border text-[12px] space-y-1 {row_cls}",
                                                        p { class: "font-medium", "{line}" }
                                                        for err_line in summary.errors.iter() {
                                                            p { class: "text-[11px] text-beet-crimson-text", "{err_line}" }
                                                        }
                                                        for w in summary.warnings.iter() {
                                                            p { class: "text-[11px] text-beet-gold", "{w}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if let Some(scan) = resp.scan_status.as_deref() {
                            p { class: "text-[11px] text-beet-text-tertiary tabular-nums-mono",
                                "Navidrome scan: {scan}"
                            }
                        }
                    }
                } else {
                    match &*recipients_res.read() {
                        None => rsx! {
                            div { class: "py-6 flex items-center justify-center gap-2 text-[13px] text-beet-text-tertiary",
                                IconLoader { size: 15 }
                                span { "Loading recipients..." }
                            }
                        },
                        Some(Err(e)) => rsx! {
                            div { class: "rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 p-3.5 text-[12px] text-beet-crimson-text space-y-2",
                                p { "{friendly_error(e)}" }
                                button {
                                    r#type: "button",
                                    class: "h-7 px-3 rounded-[8px] bg-beet-surface border border-beet-border-default text-beet-text-primary text-[11px] cursor-pointer",
                                    onclick: move |_| recipients_res.restart(),
                                    "Retry"
                                }
                            }
                        },
                        Some(Ok(recipients)) if recipients.is_empty() => rsx! {
                            div { class: "rounded-[10px] bg-beet-surface border border-beet-border-subtle p-5 text-center space-y-1.5",
                                p { class: "text-[13px] font-medium text-beet-text-primary",
                                    "No recipients available"
                                }
                                p { class: "text-[12px] text-beet-text-tertiary leading-relaxed",
                                    "No other household users currently have a music library configured with Track Sharing enabled. Other users can enable it in Settings > Library."
                                }
                            }
                        },
                        Some(Ok(recipients)) => {
                            let all_ids: Vec<String> = recipients.iter().map(|r| r.user_id.clone()).collect();
                            let all_selected = !all_ids.is_empty() && selected_ids.read().len() == all_ids.len();
                            rsx! {
                                div { class: "space-y-2.5",
                                    div { class: "flex items-center justify-between text-[12px]",
                                        span { class: "font-medium text-beet-text-secondary",
                                            "Share with"
                                        }
                                        if recipients.len() > 1 {
                                            button {
                                                r#type: "button",
                                                class: "text-beet-accent-text hover:underline cursor-pointer text-[11px]",
                                                onclick: move |_| {
                                                    if all_selected {
                                                        selected_ids.write().clear();
                                                    } else {
                                                        selected_ids.set(all_ids.iter().cloned().collect());
                                                    }
                                                },
                                                if all_selected { "Clear all" } else { "Select all" }
                                            }
                                        }
                                    }

                                    div { class: "space-y-1.5 max-h-56 overflow-y-auto studio-scrollbar pr-1",
                                        for recip in recipients.iter() {
                                            {
                                                let uid = recip.user_id.clone();
                                                let is_checked = selected_ids.read().contains(&uid);
                                                let row_cls = if is_checked {
                                                    "bg-beet-leaf-surface/50 border-beet-leaf-solid/40 text-beet-text-primary"
                                                } else {
                                                    "bg-beet-surface hover:bg-beet-surface-high border-beet-border-subtle text-beet-text-secondary"
                                                };
                                                rsx! {
                                                    label {
                                                        key: "share-recip-{recip.user_id}",
                                                        class: "flex items-center justify-between gap-3 px-3 py-2.5 rounded-[10px] border transition-colors cursor-pointer {row_cls}",
                                                        div { class: "flex items-center gap-2.5 min-w-0",
                                                            input {
                                                                r#type: "checkbox",
                                                                class: "rounded accent-beet-leaf-solid cursor-pointer shrink-0",
                                                                checked: is_checked,
                                                                disabled: sharing(),
                                                                onchange: move |_| {
                                                                    let mut set = selected_ids.write();
                                                                    if set.contains(&uid) {
                                                                        set.remove(&uid);
                                                                    } else {
                                                                        set.insert(uid.clone());
                                                                    }
                                                                },
                                                            }
                                                            span { class: "text-[13px] font-medium truncate",
                                                                "{recip.username}"
                                                            }
                                                        }
                                                        if is_checked {
                                                            IconCheck { size: 14, class: "text-beet-leaf-text shrink-0".to_string() }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
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

    fn sample_entry(
        recipient: &str,
        status: &str,
        link_mode: Option<&str>,
        error: Option<&str>,
    ) -> ShareResultEntryDto {
        ShareResultEntryDto {
            song_id: "s1".into(),
            title: "Song One".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            recipient_user_id: format!("u-{recipient}"),
            recipient_username: recipient.into(),
            status: status.into(),
            link_mode: link_mode.map(str::to_string),
            target_path: None,
            target_song_id: None,
            error: error.map(str::to_string),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn formats_single_share_outcomes_clearly() {
        assert_eq!(
            format_share_entry_summary(&sample_entry("bob", "shared", Some("hardlink"), None)),
            "Shared with bob (hardlink, 0 B used)"
        );
        assert_eq!(
            format_share_entry_summary(&sample_entry(
                "carol",
                "already_in_library",
                None,
                None
            )),
            "Already in carol's library"
        );
        assert_eq!(
            format_share_entry_summary(&sample_entry(
                "dave",
                "failed",
                None,
                Some("Recipient disabled sharing")
            )),
            "dave: Recipient disabled sharing"
        );
    }

    #[test]
    fn aggregates_album_share_entries_per_recipient() {
        let entries = vec![
            sample_entry("bob", "shared", Some("hardlink"), None),
            sample_entry("bob", "shared", Some("hardlink"), None),
            sample_entry("bob", "already_in_library", None, None),
            sample_entry("carol", "failed", None, Some("Disk full")),
        ];
        let sums = summarize_by_recipient(&entries);
        assert_eq!(sums.len(), 2);
        assert_eq!(
            format_recipient_batch_line(&sums[0]),
            "bob: 2 tracks shared (hardlink, 0 B used), 1 already in library"
        );
        assert_eq!(format_recipient_batch_line(&sums[1]), "carol: 1 failed");
    }
}
