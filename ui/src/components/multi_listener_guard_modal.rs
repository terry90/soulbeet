use dioxus::prelude::*;
use shared::client::{DeleteGuardPreview, ListenerProtectionKind};

use crate::{IconShieldAlert, IconTrash2, Modal};

/// Explicit modal decision mode derived from [`DeleteGuardPreview`] aligned with
/// `api/src/core/curation.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardModalMode {
    /// Staged discovery track: removes from discovery staging and records rejection.
    PendingDiscovery,
    /// Permanent library track owned solely by caller and deletable from disk.
    PermanentDelete,
    /// Permanent library track protected by another household listener's star/rating/playlist.
    ListenerProtected,
    /// Hard-blocked deletion (e.g. outside library, unresolved path, in use, unreadable listener).
    HardBlocked,
}

/// Classifies a [`DeleteGuardPreview`] into one of the 4 explicit curation guard modes.
pub fn classify_guard_mode(preview: &DeleteGuardPreview) -> GuardModalMode {
    if preview.is_pending_discovery {
        return GuardModalMode::PendingDiscovery;
    }
    if preview.sole_owner && preview.can_delete && preview.other_listeners.is_empty() {
        return GuardModalMode::PermanentDelete;
    }
    let has_listener_protection = preview.other_listeners.iter().any(|p| {
        matches!(
            p.kind,
            ListenerProtectionKind::OtherListenerMark
                | ListenerProtectionKind::OtherListenerPlaylist
        )
    });
    if has_listener_protection {
        return GuardModalMode::ListenerProtected;
    }
    GuardModalMode::HardBlocked
}

/// Human-readable short badge for a [`ListenerProtectionKind`].
pub fn protection_kind_label(kind: ListenerProtectionKind) -> &'static str {
    match kind {
        ListenerProtectionKind::OtherListenerMark => "Listener favorite",
        ListenerProtectionKind::OtherListenerPlaylist => "Shared playlist",
        ListenerProtectionKind::UnreadableListener => "Protected account",
        ListenerProtectionKind::OutsideLibrary => "Outside library",
        ListenerProtectionKind::PathUnresolved => "Path unresolved",
        ListenerProtectionKind::InUse => "In active use",
        ListenerProtectionKind::Other => "Protected",
    }
}

/// Accessible multi-listener deletion guard dialog powered by [`DeleteGuardPreview`].
///
/// Handles all 4 curation guard states cleanly:
/// 1. `PendingDiscovery` — drops staged track from discovery queue (`Drop Track`).
/// 2. `PermanentDelete` — sole-owner permanent removal from disk (`Delete Permanently`).
/// 3. `ListenerProtected` — lists protecting listeners and preserves disk file (`Unstar & Hide From My Mixes`).
/// 4. `HardBlocked` — displays blocking reason calmly with a single `Close` button.
#[component]
pub fn MultiListenerGuardModal(
    preview: DeleteGuardPreview,
    track_title: String,
    #[props(default)] track_artist: Option<String>,
    #[props(default)] busy: bool,
    on_confirm: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mode = classify_guard_mode(&preview);

    let track_label = match track_artist.as_deref().filter(|a| !a.trim().is_empty()) {
        Some(artist) => format!("{track_title} · {artist}"),
        None => track_title.clone(),
    };

    let heading = match mode {
        GuardModalMode::PendingDiscovery => "Drop from discovery?",
        GuardModalMode::PermanentDelete => "Delete permanently?",
        GuardModalMode::ListenerProtected => "Protected by other listeners",
        GuardModalMode::HardBlocked => "Deletion unavailable",
    };

    let is_destructive = matches!(
        mode,
        GuardModalMode::PendingDiscovery | GuardModalMode::PermanentDelete
    );

    let confirm_label = if busy {
        "Applying…"
    } else {
        match mode {
            GuardModalMode::PendingDiscovery => "Drop Track",
            GuardModalMode::PermanentDelete => "Delete Permanently",
            GuardModalMode::ListenerProtected => "Unstar & Hide From My Mixes",
            GuardModalMode::HardBlocked => "",
        }
    };

    let confirm_class = if is_destructive {
        "h-9 px-4 rounded-[8px] bg-beet-crimson-solid text-white text-[13px] font-semibold hover:brightness-110 disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-[color,background-color,border-color,filter] duration-150 cursor-pointer"
    } else {
        "h-9 px-4 rounded-[8px] bg-beet-surface-highest border border-beet-border-default text-beet-text-primary hover:bg-beet-crimson-surface hover:text-beet-crimson-text hover:border-beet-crimson-text/30 disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none text-[13px] font-semibold transition-colors duration-150 cursor-pointer"
    };

    rsx! {
        Modal {
            on_close: move |_| {
                if !busy {
                    on_cancel.call(());
                }
            },
            header: rsx! {
                div { class: "flex items-center gap-3",
                    div {
                        class: if is_destructive {
                            "w-9 h-9 rounded-[8px] bg-beet-crimson-surface text-beet-crimson-text flex items-center justify-center shrink-0"
                        } else {
                            "w-9 h-9 rounded-[8px] bg-beet-gold-surface text-beet-gold flex items-center justify-center shrink-0"
                        },
                        if is_destructive {
                            IconTrash2 { size: 18 }
                        } else {
                            IconShieldAlert { size: 18 }
                        }
                    }
                    div { class: "min-w-0",
                        h2 { class: "text-[17px] font-semibold text-beet-text-primary leading-tight",
                            "{heading}"
                        }
                        p { class: "text-[13px] text-beet-text-secondary truncate mt-0.5",
                            "{track_label}"
                        }
                    }
                }
            },
            footer: rsx! {
                div { class: "flex flex-wrap items-center justify-end gap-2",
                    if mode == GuardModalMode::HardBlocked {
                        button {
                            r#type: "button",
                            disabled: busy,
                            class: "h-9 px-4 rounded-[8px] bg-beet-surface-highest border border-beet-border-default text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none text-[13px] font-semibold transition-colors duration-150 cursor-pointer",
                            onclick: move |_| on_cancel.call(()),
                            "Close"
                        }
                    } else {
                        button {
                            r#type: "button",
                            disabled: busy,
                            class: "h-9 px-4 rounded-[8px] text-[13px] font-medium text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-50 disabled:cursor-not-allowed disabled:pointer-events-none transition-colors duration-150 cursor-pointer",
                            onclick: move |_| on_cancel.call(()),
                            "Cancel"
                        }
                        button {
                            r#type: "button",
                            disabled: busy,
                            class: "{confirm_class}",
                            onclick: move |_| on_confirm.call(()),
                            "{confirm_label}"
                        }
                    }
                }
            },

            // Modal body
            div { class: "space-y-4 text-[14px] leading-[1.5]",
                match mode {
                    GuardModalMode::PendingDiscovery => rsx! {
                        p { class: "text-beet-text-secondary",
                            "This staged discovery track will be removed from your discovery queue and marked as skipped so it won't be recommended again. Your permanent library will not be touched."
                        }
                    },
                    GuardModalMode::PermanentDelete => rsx! {
                        p { class: "text-beet-text-secondary",
                            "No other household listeners have starred, rated, or added this track to a playlist. Confirming will permanently remove the audio file from your library on disk."
                        }
                        if !preview.own_playlists.is_empty() {
                            div { class: "p-3 rounded-[8px] bg-beet-surface-high border border-beet-border-subtle space-y-1.5",
                                p { class: "text-[11px] tabular-nums-mono font-semibold uppercase tracking-wider text-beet-text-tertiary",
                                    "Also in your playlists"
                                }
                                ul { class: "space-y-1 text-[13px] text-beet-text-primary",
                                    for playlist in preview.own_playlists.iter() {
                                        li { key: "{playlist}", "• {playlist}" }
                                    }
                                }
                            }
                        }
                    },
                    GuardModalMode::ListenerProtected => rsx! {
                        p { class: "text-beet-text-secondary",
                            "This file is shared with other listeners in your library and will be kept intact on disk. You can safely unstar it and hide it from your personal mixes."
                        }
                        ul {
                            class: "space-y-2",
                            aria_label: "Listeners protecting this track",
                            for (idx, item) in preview.other_listeners.iter().enumerate() {
                                li {
                                    key: "{idx}-{item.summary}",
                                    class: "flex items-start justify-between gap-3 p-3 rounded-[8px] bg-beet-surface-high border border-beet-border-subtle",
                                    span { class: "text-[13px] text-beet-text-primary",
                                        "{item.summary}"
                                    }
                                    span { class: "shrink-0 px-2 py-0.5 rounded-full bg-beet-surface-highest text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                        "{protection_kind_label(item.kind)}"
                                    }
                                }
                            }
                        }
                    },
                    GuardModalMode::HardBlocked => rsx! {
                        p { class: "text-beet-text-secondary",
                            "This track cannot be deleted right now. The audio file remains untouched on disk."
                        }
                        if let Some(reason) = preview.block_reason.as_deref().filter(|r| !r.trim().is_empty()) {
                            div { class: "p-3 rounded-[8px] bg-beet-surface-high border border-beet-border-subtle text-[13px] text-beet-text-primary",
                                "{reason}"
                            }
                        }
                        if !preview.other_listeners.is_empty() {
                            ul {
                                class: "space-y-2",
                                aria_label: "Deletion guard reasons",
                                for (idx, item) in preview.other_listeners.iter().enumerate() {
                                    li {
                                        key: "{idx}-{item.summary}",
                                        class: "flex items-start justify-between gap-3 p-3 rounded-[8px] bg-beet-surface-high border border-beet-border-subtle",
                                        span { class: "text-[13px] text-beet-text-primary",
                                            "{item.summary}"
                                        }
                                        span { class: "shrink-0 px-2 py-0.5 rounded-full bg-beet-surface-highest text-beet-text-tertiary tabular-nums-mono text-[10px] font-semibold uppercase tracking-wider",
                                            "{protection_kind_label(item.kind)}"
                                        }
                                    }
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::client::ListenerProtection;

    fn render_modal(preview: DeleteGuardPreview) -> String {
        let mut dom = VirtualDom::new_with_props(
            |preview: DeleteGuardPreview| {
                rsx! {
                    MultiListenerGuardModal {
                        preview,
                        track_title: "Midnight City".to_string(),
                        track_artist: Some("M83".to_string()),
                        on_confirm: |_| {},
                        on_cancel: |_| {},
                    }
                }
            },
            preview,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn classifies_all_four_guard_modes_accurately() {
        let discovery = DeleteGuardPreview {
            song_id: "d1".into(),
            is_pending_discovery: true,
            discovery_track_id: Some("dt1".into()),
            can_delete: true,
            sole_owner: true,
            other_listeners: vec![],
            own_playlists: vec![],
            block_reason: None,
        };
        assert_eq!(
            classify_guard_mode(&discovery),
            GuardModalMode::PendingDiscovery
        );

        let sole = DeleteGuardPreview {
            song_id: "s1".into(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: true,
            sole_owner: true,
            other_listeners: vec![],
            own_playlists: vec!["Late Night".into()],
            block_reason: None,
        };
        assert_eq!(classify_guard_mode(&sole), GuardModalMode::PermanentDelete);

        let listener_protected = DeleteGuardPreview {
            song_id: "s2".into(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: false,
            sole_owner: false,
            other_listeners: vec![ListenerProtection {
                kind: ListenerProtectionKind::OtherListenerPlaylist,
                summary: "In Marie's Road Trip".into(),
            }],
            own_playlists: vec![],
            block_reason: None,
        };
        assert_eq!(
            classify_guard_mode(&listener_protected),
            GuardModalMode::ListenerProtected
        );

        let hard_blocked = DeleteGuardPreview {
            song_id: "s3".into(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: false,
            sole_owner: false,
            other_listeners: vec![ListenerProtection {
                kind: ListenerProtectionKind::OutsideLibrary,
                summary: "File sits outside managed library root".into(),
            }],
            own_playlists: vec![],
            block_reason: Some("Outside managed library".into()),
        };
        assert_eq!(
            classify_guard_mode(&hard_blocked),
            GuardModalMode::HardBlocked
        );
    }

    #[test]
    fn renders_all_four_guard_variants_cleanly_in_ssr() {
        // 1. Pending discovery
        let html_disc = render_modal(DeleteGuardPreview {
            song_id: "d1".into(),
            is_pending_discovery: true,
            discovery_track_id: Some("dt1".into()),
            can_delete: true,
            sole_owner: true,
            other_listeners: vec![],
            own_playlists: vec![],
            block_reason: None,
        });
        assert!(html_disc.contains("Drop from discovery?"), "{html_disc}");
        assert!(html_disc.contains("Drop Track"), "{html_disc}");

        // 2. Permanent delete
        let html_perm = render_modal(DeleteGuardPreview {
            song_id: "s1".into(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: true,
            sole_owner: true,
            other_listeners: vec![],
            own_playlists: vec!["Late Night".into()],
            block_reason: None,
        });
        assert!(html_perm.contains("Delete permanently?"), "{html_perm}");
        assert!(html_perm.contains("Delete Permanently"), "{html_perm}");
        assert!(html_perm.contains("Late Night"), "{html_perm}");

        // 3. Listener protected
        let html_shared = render_modal(DeleteGuardPreview {
            song_id: "s2".into(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: false,
            sole_owner: false,
            other_listeners: vec![ListenerProtection {
                kind: ListenerProtectionKind::OtherListenerMark,
                summary: "Marie starred and rated 5★".into(),
            }],
            own_playlists: vec![],
            block_reason: None,
        });
        assert!(
            html_shared.contains("Protected by other listeners"),
            "{html_shared}"
        );
        assert!(
            html_shared.contains("Marie starred and rated 5★"),
            "{html_shared}"
        );
        assert!(
            html_shared.contains("Unstar &#38; Hide From My Mixes")
                || html_shared.contains("Unstar &amp; Hide From My Mixes"),
            "{html_shared}"
        );

        // 4. Hard-blocked (Close only, no confirm action)
        let html_blocked = render_modal(DeleteGuardPreview {
            song_id: "s3".into(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: false,
            sole_owner: false,
            other_listeners: vec![ListenerProtection {
                kind: ListenerProtectionKind::PathUnresolved,
                summary: "Navidrome path could not be mapped".into(),
            }],
            own_playlists: vec![],
            block_reason: Some("Enable ReportRealPath in Navidrome".into()),
        });
        assert!(
            html_blocked.contains("Deletion unavailable"),
            "{html_blocked}"
        );
        assert!(
            html_blocked.contains("Enable ReportRealPath in Navidrome"),
            "{html_blocked}"
        );
        assert!(html_blocked.contains(">Close</button>"), "{html_blocked}");
        assert!(
            !html_blocked.contains("Delete Permanently"),
            "{html_blocked}"
        );
        assert!(
            !html_blocked.contains("Hide From My Mixes"),
            "{html_blocked}"
        );
    }
}
