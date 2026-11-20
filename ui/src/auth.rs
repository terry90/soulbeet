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

    /// Follows a change of the account's Navidrome connection made while
    /// signed in.
    pub fn set_navidrome_status(&mut self, status: shared::system::NavidromeStatus) {
        if let Some(state) = self.state.write().as_mut() {
            state.navidrome_status = status;
        }
    }

    /// Signs out on the server, then here. When the server could not end
    /// the session, its token still works, so the user stays signed in here
    /// too and the error is returned for display.
    pub async fn logout(&mut self) -> Result<(), ServerFnError> {
        self.signed_out(api::logout().await)
    }

    fn signed_out(&mut self, server: Result<(), ServerFnError>) -> Result<(), ServerFnError> {
        server?;
        self.state.set(None);
        Ok(())
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

    pub fn is_admin(&self) -> bool {
        self.state
            .read()
            .as_ref()
            .is_some_and(|a| a.role.is_admin())
    }

    pub fn rename_allowed(&self) -> bool {
        self.state.read().as_ref().is_some_and(|a| a.rename_allowed)
    }

    pub fn is_logged_in(&self) -> bool {
        self.state.read().is_some()
    }

    pub fn navidrome_status(&self) -> shared::system::NavidromeStatus {
        self.state
            .read()
            .as_ref()
            .map(|a| a.navidrome_status)
            .unwrap_or_default()
    }
}

pub fn use_auth() -> Auth {
    use_context::<Auth>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connecting_navidrome_updates_the_status_of_a_signed_in_user_only() {
        let dom = VirtualDom::new(|| rsx! {});
        dom.in_scope(ScopeId::ROOT, || {
            use shared::system::NavidromeStatus;
            let mut auth = Auth::new(Signal::new(Some(AuthResponse {
                username: "ann".to_string(),
                user_id: "u1".to_string(),
                navidrome_status: NavidromeStatus::NotLinked,
                role: Default::default(),
                rename_allowed: false,
            })));
            auth.set_navidrome_status(NavidromeStatus::Connected);
            assert_eq!(auth.navidrome_status(), NavidromeStatus::Connected);

            let mut signed_out = Auth::new(Signal::new(None));
            signed_out.set_navidrome_status(NavidromeStatus::Connected);
            assert!(!signed_out.is_logged_in());
        });
    }

    #[test]
    fn a_failed_sign_out_keeps_the_user_signed_in() {
        let dom = VirtualDom::new(|| rsx! {});
        dom.in_scope(ScopeId::ROOT, || {
            let mut auth = Auth::new(Signal::new(Some(AuthResponse {
                username: "ann".to_string(),
                user_id: "u1".to_string(),
                navidrome_status: Default::default(),
                role: Default::default(),
                rename_allowed: false,
            })));

            let failed = auth.signed_out(Err(ServerFnError::ServerError {
                message: "database is locked".to_string(),
                code: 500,
                details: None,
            }));
            assert!(failed.is_err());
            assert!(auth.is_logged_in());

            auth.signed_out(Ok(())).unwrap();
            assert!(!auth.is_logged_in());
        });
    }
}
