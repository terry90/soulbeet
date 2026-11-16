//! The OAuth authorize path, shared by the server and the login page that
//! sends the browser back to it after sign-in.

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

/// Where an app sends the browser to ask the user for access.
pub const AUTHORIZE_PATH: &str = "/oauth/authorize";

/// The login page a signed-out browser is sent to from AUTHORIZE_PATH, with
/// the request to come back to as its query. It is not `/login` itself: the
/// router writes a `?` after any route that declares a query, even an empty
/// one, so a plain `/login` could not stay as it is.
pub const LOGIN_RETURN_PATH: &str = "/login/return";

const RETURN_TO: &str = "return_to=";

/// Everything but the RFC 3986 unreserved bytes.
const RETURN_TO_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// LOGIN_RETURN_PATH with `back` as the request to come back to.
pub fn login_return_location(back: &str) -> String {
    format!("{LOGIN_RETURN_PATH}?{}", return_to_query(back))
}

/// The query of LOGIN_RETURN_PATH for `back`. Every byte of `back` but the
/// unreserved ones is percent-encoded, so percent-decoding the whole query
/// once, as the router does before it hands the query over, gives
/// `return_to=` and `back` exactly, `&` and `%26` included.
pub fn return_to_query(back: &str) -> String {
    format!("{RETURN_TO}{}", utf8_percent_encode(back, RETURN_TO_VALUE))
}

/// The request to come back to in a LOGIN_RETURN_PATH query that has been
/// percent-decoded once: all that follows a leading `return_to=`. It is
/// not split on `&`, which may belong to the request.
pub fn return_to_in(decoded_query: &str) -> Option<&str> {
    decoded_query.strip_prefix(RETURN_TO)
}

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

    #[test]
    fn one_decoding_of_the_query_gives_the_request_back() {
        let back = "/oauth/authorize?client_id=dcr_e2e&state=a%26b";
        let query = return_to_query(back);
        assert_eq!(
            query,
            "return_to=%2Foauth%2Fauthorize%3Fclient_id%3Ddcr_e2e%26state%3Da%2526b"
        );
        let decoded = percent_encoding::percent_decode_str(&query)
            .decode_utf8()
            .unwrap();
        assert_eq!(return_to_in(&decoded), Some(back));
        assert_eq!(
            login_return_location(back),
            format!("/login/return?{query}")
        );
    }

    #[test]
    fn every_byte_survives_the_round_trip() {
        let back = "/oauth/authorize?a=%2B+ &b=#?/\\é%%25";
        let query = return_to_query(back);
        assert!(query[RETURN_TO.len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b)));
        let decoded = percent_encoding::percent_decode_str(&query)
            .decode_utf8()
            .unwrap();
        assert_eq!(return_to_in(&decoded), Some(back));
    }

    #[test]
    fn a_query_without_return_to_has_no_request() {
        assert_eq!(return_to_in(""), None);
        assert_eq!(return_to_in("x=1&return_to=/oauth/authorize"), None);
    }
}
