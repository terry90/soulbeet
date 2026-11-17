use api::{get_deletion_blocks, get_navidrome_music_path_problem, get_unreadable_listeners};
use dioxus::prelude::*;
use shared::navidrome::{BlockedDeletion, UnreadableListener};

use super::panel::{LoadError, Loading, Panel, SectionTitle};
use crate::auth::use_auth;
use crate::friendly_error;

/// What auto-delete kept and why; for admins also a NAVIDROME_MUSIC_PATH that is no folder and
/// the accounts the deletion check cannot read.
#[component]
pub fn DeletionChecksPanel() -> Element {
    let auth = use_auth();
    let music_path = use_resource(move || async move {
        if auth.is_admin() {
            auth.call(get_navidrome_music_path_problem())
                .await
                .ok()
                .flatten()
        } else {
            None
        }
    });
    let mut kept = use_resource(move || async move { auth.call(get_deletion_blocks()).await });
    let mut unreadable = use_resource(move || async move {
        if auth.is_admin() {
            auth.call(get_unreadable_listeners()).await.map(Some)
        } else {
            Ok(None)
        }
    });

    rsx! {
        Panel { title: "Deletion Checks",
            MusicPathProblem { problem: music_path.read().clone().flatten() }
            match &*kept.read() {
                None => rsx! { Loading {} },
                Some(Err(e)) => rsx! {
                    LoadError { message: friendly_error(e), on_retry: move |_| kept.restart() }
                },
                Some(Ok(k)) => rsx! { KeptList { kept: k.clone() } },
            }
            match &*unreadable.read() {
                None => rsx! {},
                Some(Err(e)) => rsx! {
                    LoadError { message: friendly_error(e), on_retry: move |_| unreadable.restart() }
                },
                Some(Ok(None)) => rsx! {},
                Some(Ok(Some(l))) => rsx! { UnreadableList { listeners: l.clone() } },
            }
        }
    }
}

#[component]
fn MusicPathProblem(problem: Option<String>) -> Element {
    rsx! {
        if let Some(problem) = problem {
            p { class: "text-sm font-mono text-yellow-400 mb-4", "{problem}" }
        }
    }
}

#[component]
fn KeptList(kept: Vec<BlockedDeletion>) -> Element {
    rsx! {
        SectionTitle { title: "Kept by auto-delete" }
        if kept.is_empty() {
            p { class: "text-sm font-mono text-gray-300 mb-4",
                "Auto-delete is not keeping any track."
            }
        } else {
            ul { class: "list-disc pl-5 space-y-1 text-sm font-mono text-yellow-400 mb-4",
                for row in kept {
                    li { "{row.title} - {row.artist}: {row.message}" }
                }
            }
        }
    }
}

#[component]
fn UnreadableList(listeners: Vec<UnreadableListener>) -> Element {
    rsx! {
        SectionTitle { title: "Accounts Soulbeet cannot read" }
        if listeners.is_empty() {
            p { class: "text-sm font-mono text-gray-300",
                "Soulbeet can read every linked account."
            }
        } else {
            p { class: "text-sm font-mono text-gray-300 mb-3",
                "While an account cannot be read, no file it might have rated is deleted."
            }
            ul { class: "list-disc pl-5 space-y-1 text-sm font-mono text-yellow-400",
                for l in listeners {
                    li { "{l.username}: {l.error} (since {l.since})" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_kept(kept: Vec<BlockedDeletion>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |kept: Vec<BlockedDeletion>| rsx! { KeptList { kept } },
            kept,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn render_unreadable(listeners: Vec<UnreadableListener>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |listeners: Vec<UnreadableListener>| rsx! { UnreadableList { listeners } },
            listeners,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_music_path_problem_is_shown_when_there_is_one() {
        let render = |problem: Option<String>| {
            let mut dom = VirtualDom::new_with_props(
                |problem: Option<String>| rsx! { MusicPathProblem { problem } },
                problem,
            );
            dom.rebuild_in_place();
            dioxus_ssr::render(&dom)
        };
        assert!(render(Some("NAVIDROME_MUSIC_PATH is /media/music".into()))
            .contains("NAVIDROME_MUSIC_PATH is /media/music"));
        assert!(!render(None).contains("NAVIDROME_MUSIC_PATH"));
    }

    #[test]
    fn each_kept_track_shows_its_reason() {
        let kept = vec![BlockedDeletion {
            title: "Silent Night".into(),
            artist: "Marrow Choir".into(),
            message: "Another listener rated, starred or kept this track in a playlist".into(),
        }];
        let html = render_kept(kept);
        assert!(
            html.contains(
                "Silent Night - Marrow Choir: Another listener rated, starred or kept this track in a playlist"
            ),
            "{html}"
        );
        assert!(render_kept(vec![]).contains("Auto-delete is not keeping any track."));
    }

    #[test]
    fn admins_see_each_account_that_cannot_be_read() {
        let html = render_unreadable(vec![UnreadableListener {
            username: "bob".into(),
            error: "Navidrome appears unavailable (circuit breaker open)".into(),
            since: "2026-11-15 20:58:00".into(),
        }]);
        assert!(
            html.contains(
                "bob: Navidrome appears unavailable (circuit breaker open) (since 2026-11-15 20:58:00)"
            ),
            "{html}"
        );
        assert!(html
            .contains("While an account cannot be read, no file it might have rated is deleted."));
        assert!(render_unreadable(vec![]).contains("Soulbeet can read every linked account."));
    }
}
