use std::fmt::Display;
use std::future::Future;
use std::ops::RangeInclusive;
use std::str::FromStr;

use api::{UpdateUserSettings, UserSettings};
use dioxus::prelude::*;

/// Saves a change made in a settings input, so the inputs then show what the server holds: the
/// saved settings, or after a refusal the stored ones loaded again, with the reason in `error`.
/// A change that could not be read (`Err`) is not sent.
pub(super) async fn save_setting<F>(
    mut stored: Resource<Result<UserSettings, ServerFnError>>,
    mut error: Signal<String>,
    change: Result<UpdateUserSettings, String>,
    save: impl FnOnce(UpdateUserSettings) -> F,
) where
    F: Future<Output = Result<UserSettings, ServerFnError>>,
{
    let saved = match change {
        Ok(change) => save(change).await.map_err(|e| crate::friendly_error(&e)),
        Err(unreadable) => Err(unreadable),
    };
    match saved {
        Ok(settings) => {
            error.set(String::new());
            stored.set(Some(Ok(settings)));
        }
        Err(why) => {
            error.set(why);
            stored.restart();
        }
    }
}

/// The number typed in a field that takes `range`, or why it cannot be saved. An empty field or
/// one holding anything else is refused, never saved as some default.
pub(super) fn typed_number<T>(text: &str, what: &str, range: RangeInclusive<T>) -> Result<T, String>
where
    T: FromStr + PartialOrd + Display,
{
    match text.trim().parse::<T>() {
        Ok(n) if range.contains(&n) => Ok(n),
        _ => Err(format!(
            "{what} must be a whole number from {} to {}",
            range.start(),
            range.end()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::components::simple::ToggleDriver;

    fn settings(threshold: u8) -> UserSettings {
        UserSettings {
            discovery_promote_threshold: threshold,
            ..Default::default()
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Server {
        Saves,
        Refuses,
    }

    /// A promote threshold field where a click types `typed` and saves it. The server holds 3 on
    /// the first load and 4 on every later one, as if another tab changed it, so a reload shows.
    fn threshold_field(typed: &'static str, server: Server) -> ToggleDriver {
        ToggleDriver::new(VirtualDom::new_with_props(
            |(typed, server): (&'static str, Server)| {
                let loads = use_hook(|| Rc::new(Cell::new(0u8)));
                let stored = use_resource(move || {
                    let loads = loads.clone();
                    async move {
                        loads.set(loads.get() + 1);
                        Ok::<_, ServerFnError>(settings(2 + loads.get().min(2)))
                    }
                });
                let error = use_signal(|| "an old error".to_string());
                let mut shown = use_signal(String::new);
                let mut asked = use_signal(|| false);
                use_effect(move || {
                    if let Some(Ok(s)) = &*stored.read() {
                        shown.set(s.discovery_promote_threshold.to_string());
                    }
                });
                rsx! {
                    button {
                        onclick: move |_| {
                            shown.set(typed.to_string());
                            let change = typed_number(typed, "The threshold", 1..=5u8).map(|n| {
                                UpdateUserSettings {
                                    discovery_promote_threshold: Some(n),
                                    ..Default::default()
                                }
                            });
                            save_setting(stored, error, change, move |update| {
                                asked.set(true);
                                async move {
                                    match server {
                                        Server::Saves => Ok(settings(
                                            update.discovery_promote_threshold.unwrap_or_default(),
                                        )),
                                        Server::Refuses => Err(ServerFnError::ServerError {
                                            message: "Folder not found".into(),
                                            code: api::REFUSAL_CODE,
                                            details: None,
                                        }),
                                    }
                                }
                            })
                        },
                    }
                    p { "shown={shown}" }
                    p { "error={error}" }
                    p { "asked={asked}" }
                }
            },
            (typed, server),
        ))
    }

    #[test]
    fn a_saved_value_shows_what_the_server_saved() {
        let mut field = threshold_field("5", Server::Saves);
        field.click();
        let html = field.html();
        assert!(html.contains("shown=5"), "{html}");
        assert!(!html.contains("an old error"), "{html}");
    }

    #[test]
    fn a_refused_value_shows_why_and_the_stored_value_again() {
        let mut field = threshold_field("5", Server::Refuses);
        field.click();
        let html = field.html();
        assert!(html.contains("shown=4"), "{html}");
        assert!(html.contains("error=Folder not found"), "{html}");
    }

    #[test]
    fn a_number_that_cannot_be_read_is_not_sent() {
        for text in ["", "three", "300", "0"] {
            let mut field = threshold_field(text, Server::Saves);
            field.click();
            let html = field.html();
            assert!(html.contains("asked=false"), "{text}: {html}");
            assert!(html.contains("shown=4"), "{text}: {html}");
            assert!(
                html.contains("error=The threshold must be a whole number from 1 to 5"),
                "{text}: {html}"
            );
        }
    }

    #[test]
    fn typed_numbers_are_read_inside_their_range() {
        assert_eq!(typed_number(" 7 ", "The count", 1..=50u32), Ok(7));
        assert_eq!(
            typed_number("51", "The count", 1..=50u32),
            Err("The count must be a whole number from 1 to 50".to_string())
        );
    }
}
