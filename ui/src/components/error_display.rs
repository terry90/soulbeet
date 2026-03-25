use dioxus::logger::tracing::warn;
use dioxus::prelude::ServerFnError;

/// Maps a ServerFnError to a short, user-facing message.
/// Logs the full technical error via tracing::warn for debugging.
pub fn friendly_error(error: &ServerFnError) -> String {
    warn!("Server error: {error:?}");

    match error {
        ServerFnError::ServerError { code: 401, .. } => {
            "Session expired. Please log in again.".to_string()
        }
        ServerFnError::ServerError { code: 500, message, .. } => {
            if message.contains("not found") {
                "The requested item was not found.".to_string()
            } else if message.contains("not authorized") || message.contains("Not authorized") {
                "You don't have permission for this action.".to_string()
            } else if message.contains("already") {
                message.clone()
            } else {
                "Something went wrong. Try again.".to_string()
            }
        }
        ServerFnError::ServerError { code, .. } => {
            format!("Server error ({code}). Try again.")
        }
        ServerFnError::Request(_) => "Could not reach the server. Check your connection.".to_string(),
        _ => "Something went wrong. Try again.".to_string(),
    }
}
