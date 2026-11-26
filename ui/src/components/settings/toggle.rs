use std::future::Future;

use dioxus::prelude::*;

/// A sliding switch; `on_class` is the track colour while it is on.
#[component]
pub fn Toggle(on: bool, on_class: &'static str, onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button {
            class: format!(
                "relative w-11 h-6 rounded-full cursor-pointer transition-colors shrink-0 {}",
                if on { on_class } else { "bg-beet-surface-highest border border-beet-border-default" }
            ),
            onclick: move |e| onclick.call(e),
            span {
                class: format!(
                    "absolute top-0.5 left-0.5 w-5 h-5 bg-beet-text-primary rounded-full transition-transform {}",
                    if on { "translate-x-5" } else { "" }
                ),
            }
        }
    }
}

/// Flips a switch and saves it. A failed save puts the switch back and shows
/// why, so the switch never shows a state the server does not act on.
pub(super) async fn save_switch<T, F>(
    mut on: Signal<bool>,
    mut error: Signal<String>,
    save: impl FnOnce(bool) -> F,
) where
    F: Future<Output = Result<T, ServerFnError>>,
{
    let new_val = !on();
    on.set(new_val);
    match save(new_val).await {
        Ok(_) => error.set(String::new()),
        Err(e) => {
            on.set(!new_val);
            error.set(crate::friendly_error(&e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::simple::ToggleDriver;

    fn render(app: fn() -> Element) -> String {
        let mut dom = VirtualDom::new(app);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_toggle_shows_its_state() {
        let on = render(|| {
            rsx! { Toggle { on: true, on_class: "bg-beet-crimson-solid", onclick: |_| {} } }
        });
        assert!(on.contains("bg-beet-crimson-solid") && on.contains("translate-x-5"));
        let off = render(|| {
            rsx! { Toggle { on: false, on_class: "bg-beet-crimson-solid", onclick: |_| {} } }
        });
        assert!(off.contains("bg-beet-surface-highest") && !off.contains("translate-x-5"));
    }

    /// A switch that starts on, saved by a save that fails when `fails` says so.
    fn saved_switch(fails: bool) -> ToggleDriver {
        ToggleDriver::new(VirtualDom::new_with_props(
            |fails: bool| {
                let on = use_signal(|| true);
                let error = use_signal(|| "an old error".to_string());
                rsx! {
                    Toggle {
                        on: on(),
                        on_class: "bg-beet-crimson-solid",
                        onclick: move |_| save_switch(on, error, move |_| async move {
                            if fails {
                                Err(ServerFnError::ServerError {
                                    message: "Session expired".into(),
                                    code: 401,
                                    details: None,
                                })
                            } else {
                                Ok(())
                            }
                        }),
                    }
                    p { "{error}" }
                }
            },
            fails,
        ))
    }

    #[test]
    fn a_failed_save_puts_the_switch_back_and_shows_why() {
        let mut switch = saved_switch(true);
        switch.click();
        let html = switch.html();
        assert!(html.contains("bg-beet-crimson-solid"), "{html}");
        assert!(
            html.contains("Session expired. Please log in again."),
            "{html}"
        );
    }

    #[test]
    fn a_saved_switch_keeps_its_new_state_and_clears_the_error() {
        let mut switch = saved_switch(false);
        switch.click();
        let html = switch.html();
        assert!(html.contains("bg-beet-surface-highest"), "{html}");
        assert!(!html.contains("an old error"), "{html}");
    }
}
