use api::auth::AuthResponse;
use dioxus::prelude::*;

#[derive(Clone, Copy, Debug)]
pub struct Auth {
    state: Signal<Option<AuthResponse>>,
}

impl Auth {
    pub fn new(state: Signal<Option<AuthResponse>>) -> Self {
        Self { state }
    }

    pub fn login(&mut self, response: AuthResponse) {
        self.state.set(Some(response));
    }

    pub async fn logout(&mut self) {
        let _ = api::logout().await;
        self.state.set(None);
    }

    /// Check if a server error is an authentication error.
    /// If it is, logs the user out locally.
    /// Returns true if the error was handled (user logged out), false otherwise.
    pub fn handle_error(&mut self, error: &ServerFnError) -> bool {
        if let ServerFnError::ServerError { code: 401, .. } = error {
            self.state.set(None);
            return true;
        }
        false
    }

    /// Wraps a server function call to automatically handle authentication errors.
    pub async fn call<T>(
        mut self,
        fut: impl std::future::Future<Output = Result<T, ServerFnError>>,
    ) -> Result<T, ServerFnError> {
        match fut.await {
            Ok(val) => Ok(val),
            Err(e) => {
                self.handle_error(&e);
                Err(e)
            }
        }
    }

    pub fn user_id(&self) -> Option<String> {
        self.state.read().as_ref().map(|a| a.user_id.clone())
    }

    pub fn username(&self) -> Option<String> {
        self.state.read().as_ref().map(|a| a.username.clone())
    }

    pub fn is_logged_in(&self) -> bool {
        self.state.read().is_some()
    }
}

pub fn use_auth() -> Auth {
    use_context::<Auth>()
}
