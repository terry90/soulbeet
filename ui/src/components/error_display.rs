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
        ServerFnError::ServerError {
            code: 500, message, ..
        } => {
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
        ServerFnError::ServerError {
            code: api::REFUSAL_CODE,
            message,
            ..
        } => message.clone(),
        ServerFnError::ServerError { code, .. } => {
            format!("Server error ({code}). Try again.")
        }
        ServerFnError::Request(_) => {
            "Could not reach the server. Check your connection.".to_string()
        }
        _ => "Something went wrong. Try again.".to_string(),
    }
}

/// Whether the server refused the request for a reason it gave for display.
pub fn is_refusal(error: &ServerFnError) -> bool {
    matches!(
        error,
        ServerFnError::ServerError {
            code: api::REFUSAL_CODE,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_error(code: u16, message: &str) -> ServerFnError {
        ServerFnError::ServerError {
            message: message.to_string(),
            code,
            details: None,
        }
    }

    #[test]
    fn refusals_show_their_message() {
        let e = server_error(api::REFUSAL_CODE, "beets cannot start");
        assert_eq!(friendly_error(&e), "beets cannot start");
        let e = server_error(api::REFUSAL_CODE, "Folder not found");
        assert_eq!(friendly_error(&e), "Folder not found");
        assert!(is_refusal(&e));
        assert!(!is_refusal(&server_error(500, "Nothing to retry")));
    }

    #[test]
    fn raw_server_errors_stay_generic() {
        let e = server_error(
            500,
            "error returned from database: (code: 14) unable to open",
        );
        assert_eq!(friendly_error(&e), "Something went wrong. Try again.");
    }
}
