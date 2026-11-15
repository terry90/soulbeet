//! The OAuth authorize path, shared by the server and the login page that
//! sends the browser back to it after sign-in.

/// Where an app sends the browser to ask the user for access.
pub const AUTHORIZE_PATH: &str = "/oauth/authorize";

/// `raw` when it is AUTHORIZE_PATH alone or followed by "?query", with no ASCII control
/// character, space, '#' or '\'. Anything else could leave Soulful.
pub fn accepted_return_to(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix(AUTHORIZE_PATH)?;
    if !(rest.is_empty() || rest.starts_with('?')) {
        return None;
    }
    let unsafe_byte = |b: u8| b.is_ascii_control() || matches!(b, b' ' | b'#' | b'\\');
    if raw.bytes().any(unsafe_byte) {
        return None;
    }
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_authorize_path_is_a_return_target() {
        for ok in [
            "/oauth/authorize",
            "/oauth/authorize?client_id=dcr_x&state=a%26b",
        ] {
            assert_eq!(accepted_return_to(ok), Some(ok));
        }
        for bad in [
            "",
            "/",
            "/settings",
            "//evil.example/oauth/authorize",
            "https://evil.example/oauth/authorize",
            "/oauth/authorize/../settings",
            "/oauth/authorizex",
            "/oauth/authorize#x",
            "/\\evil.example",
            " /oauth/authorize",
            "/oauth/authorize\n",
            "/oauth/authorize?a=\t",
            "/oauth%2Fauthorize",
            "/oauth/authorize?x=\\y",
        ] {
            assert_eq!(accepted_return_to(bad), None, "{bad:?}");
        }
    }
}
