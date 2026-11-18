//! The build this code was compiled in. The server and the WASM bundle built
//! with it carry the same id, so a tab can tell that its bundle came from
//! another build and cannot be trusted to read the server's answers.

/// Set by `api/build.rs`.
pub const BUILD_ID: &str = env!("SOULFUL_BUILD_ID");

/// Whether a server that names `server_build` runs the build this code came from.
pub fn is_current(server_build: &str) -> bool {
    server_build == BUILD_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_of_this_build_is_current() {
        assert!(is_current(BUILD_ID));
    }

    #[test]
    fn a_server_of_another_build_is_not_current() {
        assert!(!is_current(&format!("{BUILD_ID}-other")));
        assert!(!is_current(""));
    }
}
