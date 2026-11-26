use dioxus::prelude::*;
use shared::client::{DeleteGuardPreview, ListenerProtectionKind};

use crate::{IconShieldAlert, IconTrash2, Modal};

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
/// - When `sole_owner && can_delete && other_listeners.is_empty()`: explains permanent
///   file removal and offers `Cancel` vs crimson `Delete Permanently`.
/// - When other listeners protect the track (or disk deletion is blocked): lists each
///   protecting listener calmly, explains the shared audio file stays intact on disk,
///   and offers `Cancel` vs `Unstar & Hide From My Mixes`.
#[component]
pub fn MultiListenerGuardModal(
    preview: DeleteGuardPreview,
    track_title: String,
    #[props(default)] track_artist: Option<String>,
    #[props(default)] busy: bool,
    on_confirm: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let is_permanent_delete =
        preview.sole_owner && preview.can_delete && preview.other_listeners.is_empty();

    let track_label = match track_artist.as_deref().filter(|a| !a.trim().is_empty()) {
        Some(artist) => format!("{track_title} — {artist}"),
        None => track_title.clone(),
    };

    let heading = if is_permanent_delete {
        "Delete permanently?"
    } else if !preview.other_listeners.is_empty() {
        "Protected by other listeners"
    } else {
        "Shared file kept on disk"
    };

    let confirm_label = if busy {
        "Applying…"
    } else if is_permanent_delete {
        "Delete Permanently"
    } else {
        "Unstar & Hide From My Mixes"
    };

    let confirm_class = if is_permanent_delete {
        "h-9 px-4 rounded-[8px] bg-beet-crimson-solid text-white text-[13px] font-semibold hover:brightness-110 disabled:opacity-50 transition-[filter,opacity] duration-150 cursor-pointer"
    } else {
        "h-9 px-4 rounded-[8px] bg-beet-surface-highest border border-beet-border-default text-beet-text-primary hover:bg-beet-crimson-surface hover:text-beet-crimson-text hover:border-beet-crimson-text/30 disabled:opacity-50 text-[13px] font-semibold transition-colors duration-150 cursor-pointer"
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
                        class: if is_permanent_delete {
                            "w-9 h-9 rounded-[8px] bg-beet-crimson-surface text-beet-crimson-text flex items-center justify-center shrink-0"
                        } else {
                            "w-9 h-9 rounded-[8px] bg-beet-gold-surface text-beet-gold flex items-center justify-center shrink-0"
                        },
                        if is_permanent_delete {
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
                    button {
                        r#type: "button",
                        disabled: busy,
                        class: "h-9 px-4 rounded-[8px] text-[13px] font-medium text-beet-text-secondary hover:text-beet-text-primary hover:bg-beet-surface-high disabled:opacity-50 transition-colors duration-150 cursor-pointer",
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
            },

            // Modal body
            div { class: "space-y-4 text-[14px] leading-[1.5]",
                if is_permanent_delete {
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
                } else {
                    if !preview.other_listeners.is_empty() {
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
                    } else if let Some(reason) = preview.block_reason.as_deref() {
                        div { class: "p-3 rounded-[8px] bg-beet-surface-high border border-beet-border-subtle text-[13px] text-beet-text-secondary",
                            "{reason}"
                        }
                        p { class: "text-beet-text-secondary",
                            "The audio file will remain on disk. Confirming will unstar this track and exclude it from your personal mixes."
                        }
                    } else {
                        p { class: "text-beet-text-secondary",
                            "The shared audio file will remain on disk. Confirming will unstar this track and hide it from your personal mixes."
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
    use shared::client::ListenerProtection;

    #[test]
    fn maps_all_protection_kinds_to_calm_labels() {
        assert_eq!(
            protection_kind_label(ListenerProtectionKind::OtherListenerMark),
            "Listener favorite"
        );
        assert_eq!(
            protection_kind_label(ListenerProtectionKind::OtherListenerPlaylist),
            "Shared playlist"
        );
        assert_eq!(
            protection_kind_label(ListenerProtectionKind::InUse),
            "In active use"
        );
    }

    #[test]
    fn renders_sole_owner_vs_protected_variants_in_ssr() {
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
            sole,
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Delete permanently?"));
        assert!(html.contains("Delete Permanently"));
        assert!(html.contains("Late Night"));

        let shared = DeleteGuardPreview {
            song_id: "s2".into(),
            is_pending_discovery: false,
            discovery_track_id: None,
            can_delete: false,
            sole_owner: false,
            other_listeners: vec![ListenerProtection {
                kind: ListenerProtectionKind::OtherListenerMark,
                summary: "Marie starred & rated 5★".into(),
            }],
            own_playlists: vec![],
            block_reason: None,
        };
        let mut dom2 = VirtualDom::new_with_props(
            |preview: DeleteGuardPreview| {
                rsx! {
                    MultiListenerGuardModal {
                        preview,
                        track_title: "Veridis Quo".to_string(),
                        on_confirm: |_| {},
                        on_cancel: |_| {},
                    }
                }
            },
            shared,
        );
        dom2.rebuild_in_place();
        let html2 = dioxus_ssr::render(&dom2);
        assert!(html2.contains("Protected by other listeners"));
        assert!(html2.contains("Marie starred"));
        assert!(
            html2.contains("Unstar &#38; Hide From My Mixes")
                || html2.contains("Unstar &amp; Hide From My Mixes"),
            "{html2}"
        );
    }
}
