use std::fmt::Display;
use std::future::Future;
use std::ops::RangeInclusive;
use std::str::FromStr;

use api::{UpdateUserSettings, UserSettings};
use dioxus::prelude::*;

/// Where a settings panel's inputs save: `stored` holds the settings the panel loaded, `error`
/// shows why a save failed.
#[derive(Clone, Copy)]
pub(super) struct SettingSaver {
    pub(super) stored: Resource<Result<UserSettings, ServerFnError>>,
    pub(super) error: Signal<String>,
}

impl SettingSaver {
    /// save_setting with the server's own save and load.
    pub(super) async fn save(
        self,
        change: Result<UpdateUserSettings, String>,
        show: impl FnOnce(&UserSettings),
    ) {
        save_setting(
            self.stored,
            self.error,
            change,
            api::update_user_settings,
            api::get_user_settings,
            show,
        )
        .await
    }
}

/// Saves a change made in one settings input, then has `show` put what the server holds into
/// that input only: the saved settings, or after a refusal the stored ones loaded again, with the
/// reason in `error`. Only that input is refilled, so a reply never overwrites another field the
/// user is typing in. When the stored settings cannot be loaded, `stored` takes that error and
/// the panel shows its load error. A change that could not be read (`Err`) is not sent.
pub(super) async fn save_setting<S, L>(
    mut stored: Resource<Result<UserSettings, ServerFnError>>,
    mut error: Signal<String>,
    change: Result<UpdateUserSettings, String>,
    save: impl FnOnce(UpdateUserSettings) -> S,
    load: impl FnOnce() -> L,
    show: impl FnOnce(&UserSettings),
) where
    S: Future<Output = Result<UserSettings, ServerFnError>>,
    L: Future<Output = Result<UserSettings, ServerFnError>>,
{
    let saved = match change {
        Ok(change) => save(change).await.map_err(|e| crate::friendly_error(&e)),
        Err(unreadable) => Err(unreadable),
    };
    match saved {
        Ok(settings) => {
            error.set(String::new());
            show(&settings);
        }
        Err(why) => {
            error.set(why);
            match load().await {
                Ok(settings) => show(&settings),
                Err(e) => stored.set(Some(Err(e))),
            }
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
        RefusesAndCannotLoad,
    }

    /// A promote threshold field where a click types `typed` and saves it, next to another field
    /// the user is typing in meanwhile. The panel loaded 3; a later load finds 4, as if another
    /// tab changed it, so a reload shows.
    fn threshold_field(typed: &'static str, server: Server) -> ToggleDriver {
        ToggleDriver::new(VirtualDom::new_with_props(
            |(typed, server): (&'static str, Server)| {
                let stored = use_resource(|| async { Ok::<_, ServerFnError>(settings(3)) });
                let error = use_signal(|| "an old error".to_string());
                let mut shown = use_signal(String::new);
                let mut other = use_signal(String::new);
                let mut asked = use_signal(|| false);
                use_effect(move || {
                    if let Some(Ok(s)) = &*stored.read() {
                        shown.set(s.discovery_promote_threshold.to_string());
                        other.set(s.discovery_promote_threshold.to_string());
                    }
                });
                let loaded = match &*stored.read() {
                    Some(Ok(s)) => s.discovery_promote_threshold.to_string(),
                    Some(Err(_)) => "a load error".to_string(),
                    None => "loading".to_string(),
                };
                rsx! {
                    button {
                        onclick: move |_| {
                            shown.set(typed.to_string());
                            other.set("being typed".to_string());
                            let change = typed_number(typed, "The threshold", 1..=5u8).map(|n| {
                                UpdateUserSettings {
                                    discovery_promote_threshold: Some(n),
                                    ..Default::default()
                                }
                            });
                            save_setting(
                                stored,
                                error,
                                change,
                                move |update| {
                                    asked.set(true);
                                    async move {
                                        match server {
                                            Server::Saves => Ok(settings(
                                                update.discovery_promote_threshold.unwrap_or_default(),
                                            )),
                                            Server::Refuses | Server::RefusesAndCannotLoad => {
                                                Err(ServerFnError::ServerError {
                                                    message: "Folder not found".into(),
                                                    code: api::REFUSAL_CODE,
                                                    details: None,
                                                })
                                            }
                                        }
                                    }
                                },
                                move || async move {
                                    match server {
                                        Server::RefusesAndCannotLoad => {
                                            Err(ServerFnError::ServerError {
                                                message: "Database is locked".into(),
                                                code: 500,
                                                details: None,
                                            })
                                        }
                                        Server::Saves | Server::Refuses => Ok(settings(4)),
                                    }
                                },
                                move |s| shown.set(s.discovery_promote_threshold.to_string()),
                            )
                        },
                    }
                    p { "shown={shown}" }
                    p { "other={other}" }
                    p { "error={error}" }
                    p { "asked={asked}" }
                    p { "stored={loaded}" }
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
        assert!(html.contains("other=being typed"), "{html}");
    }

    #[test]
    fn a_refused_value_shows_why_and_the_stored_value_again() {
        let mut field = threshold_field("5", Server::Refuses);
        field.click();
        let html = field.html();
        assert!(html.contains("shown=4"), "{html}");
        assert!(html.contains("error=Folder not found"), "{html}");
        assert!(html.contains("other=being typed"), "{html}");
    }

    #[test]
    fn a_refusal_whose_reload_fails_shows_the_load_error() {
        let mut field = threshold_field("5", Server::RefusesAndCannotLoad);
        field.click();
        let html = field.html();
        assert!(html.contains("stored=a load error"), "{html}");
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
        assert_eq!(typed_number(" 7 ", "The count", 1..=50i64), Ok(7));
        assert_eq!(
            typed_number("51", "The count", 1..=50i64),
            Err("The count must be a whole number from 1 to 50".to_string())
        );
    }
}
