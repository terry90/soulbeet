//! Global Studio keyboard shortcut listener (`#[cfg(feature = "web")]`).
//!
//! Respects [`ui::should_ignore_shortcut_target`] so typing inside `<input>`, `<textarea>`,
//! `<select>`, `[contenteditable]`, or an open `<dialog>` never triggers playback or curation
//! shortcuts (except `Escape` and `Ctrl+K` / `Cmd+K`).

#[cfg(feature = "web")]
use dioxus::prelude::*;
#[cfg(feature = "web")]
use ui::{use_player, DrawerTab};

#[cfg(feature = "web")]
use crate::auth::use_auth;
#[cfg(feature = "web")]
use crate::web_player::use_studio;

/// Installs the window-level keyboard shortcut listener for the Studio shell.
#[cfg(feature = "web")]
pub fn use_studio_shortcuts() {
    let auth = use_auth();
    let player = use_player();
    let mut studio = use_studio();

    use_hook(move || {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        let window = web_sys::window()?;
        let win_for_cb = window.clone();

        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |evt: web_sys::KeyboardEvent| {
            let key = evt.key();
            let ctrl_or_meta = evt.ctrl_key() || evt.meta_key();
            let shift = evt.shift_key();

            let (tag_name, is_editable, dialog_open) = inspect_dom_focus(&win_for_cb);
            if ui::should_ignore_shortcut_target(
                &tag_name,
                is_editable,
                dialog_open,
                &key,
                ctrl_or_meta,
            ) {
                return;
            }

            // Ctrl+K / Cmd+K -> Toggle Command Palette
            if ctrl_or_meta && key.eq_ignore_ascii_case("k") {
                evt.prevent_default();
                studio.cmdk_open.toggle();
                return;
            }

            if key == "Escape" {
                if *studio.cmdk_open.peek() {
                    evt.prevent_default();
                    studio.cmdk_open.set(false);
                }
                return;
            }

            // Ignore other modified chords (Ctrl+C, Ctrl+R, Alt+Left, etc.)
            if ctrl_or_meta || evt.alt_key() {
                return;
            }

            match key.as_str() {
                " " => {
                    evt.prevent_default();
                    player.toggle_play_pause();
                }
                "ArrowLeft" => {
                    evt.prevent_default();
                    if shift {
                        player.previous();
                    } else {
                        let cur = *player.position_signal().peek();
                        player.seek((cur - 5.0).max(0.0));
                    }
                }
                "ArrowRight" => {
                    evt.prevent_default();
                    if shift {
                        player.next();
                    } else {
                        let cur = *player.position_signal().peek();
                        player.seek(cur + 5.0);
                    }
                }
                "m" | "M" => {
                    evt.prevent_default();
                    player.toggle_mute();
                }
                "l" | "L" => {
                    evt.prevent_default();
                    studio.toggle_drawer(DrawerTab::Lyrics);
                }
                "q" | "Q" => {
                    evt.prevent_default();
                    studio.toggle_drawer(DrawerTab::Queue);
                }
                "i" | "I" => {
                    evt.prevent_default();
                    studio.toggle_drawer(DrawerTab::Inspector);
                }
                "s" | "S" => {
                    if let Some(track) = player.state_signal().peek().current_track.clone() {
                        evt.prevent_default();
                        let next_star = !track.starred;
                        studio.star_track(auth, player, track, next_star);
                    }
                }
                "p" | "P" => {
                    if let Some(track) = player.state_signal().peek().current_track.clone() {
                        if track.is_discovery {
                            evt.prevent_default();
                            studio.keep_track(auth, player, track);
                        }
                    }
                }
                "x" | "X" => {
                    if let Some(track) = player.state_signal().peek().current_track.clone() {
                        evt.prevent_default();
                        studio.drop_track(auth, player, track);
                    }
                }
                "r" | "R" => {
                    if let Some(track) = player.state_signal().peek().current_track.clone() {
                        evt.prevent_default();
                        studio.start_radio(auth, player, track);
                    }
                }
                "1" | "2" | "3" | "4" | "5" => {
                    if let Some(track) = player.state_signal().peek().current_track.clone() {
                        if let Ok(rating) = key.parse::<u8>() {
                            evt.prevent_default();
                            studio.rate_track(auth, player, track, rating);
                        }
                    }
                }
                _ => {}
            }
        });

        let _ = window.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        Some(std::rc::Rc::new(ShortcutListenerGuard { window, cb }))
    });
}

#[cfg(feature = "web")]
struct ShortcutListenerGuard {
    window: web_sys::Window,
    cb: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::KeyboardEvent)>,
}

#[cfg(feature = "web")]
impl Drop for ShortcutListenerGuard {
    fn drop(&mut self) {
        use wasm_bindgen::JsCast;
        let _ = self
            .window
            .remove_event_listener_with_callback("keydown", self.cb.as_ref().unchecked_ref());
    }
}

#[cfg(feature = "web")]
fn inspect_dom_focus(window: &web_sys::Window) -> (String, bool, bool) {
    use wasm_bindgen::JsCast;

    let Some(doc) = window.document() else {
        return (String::new(), false, false);
    };
    let dialog_open = doc
        .query_selector("dialog[open]")
        .ok()
        .flatten()
        .is_some();
    let Some(active) = doc.active_element() else {
        return (String::new(), false, dialog_open);
    };
    let tag = active.tag_name();
    let is_editable = active
        .dyn_ref::<web_sys::HtmlElement>()
        .is_some_and(|el| el.is_content_editable());
    (tag, is_editable, dialog_open)
}

#[cfg(not(feature = "web"))]
pub fn use_studio_shortcuts() {}
