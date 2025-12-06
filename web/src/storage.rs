use api::auth::AuthResponse;
#[cfg(target_arch = "wasm32")]
use ui::AUTH_SESSION_KEY;

/// Helper to abstract WebSys LocalStorage interactions
pub struct Storage;

impl Storage {
    pub fn get() -> Option<AuthResponse> {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = web_sys::window()?.local_storage().ok().flatten() {
            if let Ok(Some(json)) = storage.get_item(AUTH_SESSION_KEY) {
                return serde_json::from_str(&json).ok();
            }
        }
        None
    }

    pub fn set(_auth: &AuthResponse) {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            if let Ok(json) = serde_json::to_string(_auth) {
                let _ = storage.set_item(AUTH_SESSION_KEY, &json);
            }
        }
    }

    pub fn remove() {
        #[cfg(target_arch = "wasm32")]
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.remove_item(AUTH_SESSION_KEY);
        }
    }
}
