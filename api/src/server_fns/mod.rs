use dioxus::prelude::*;

pub mod auth;
pub mod discovery;
pub mod download;
pub mod folder;
pub mod guard;
pub mod navidrome;
pub mod search;
pub mod settings;
pub mod system;
pub mod user;

pub use auth::*;
pub use discovery::*;
pub use download::*;
pub use folder::*;
pub use guard::*;
pub use navidrome::*;
pub use search::*;
pub use settings::*;
pub use system::*;
pub use user::*;

pub fn server_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}

/// Status of a request refused for a reason the user can act on. Its message
/// is written for display, unlike a 500's.
pub const REFUSAL_CODE: u16 = 409;

pub fn refusal_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: REFUSAL_CODE,
        details: None,
    }
}

pub fn unauthorized_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: 401,
        details: None,
    }
}

/// Remove `dir` if empty, then each empty parent above it, stopping below
/// `root`. `root` itself is never removed, and a `dir` outside `root` is left
/// alone, so the walk can never climb out of the tree the caller owns (the
/// download directory, a library folder, a Discovery profile).
#[cfg(feature = "server")]
pub async fn cleanup_empty_ancestors(
    dir: &std::path::Path,
    root: &std::path::Path,
) -> Result<(), std::io::Error> {
    let mut dir = dir;
    while dir != root && dir.starts_with(root) {
        if tokio::fs::read_dir(dir)
            .await?
            .next_entry()
            .await?
            .is_some()
        {
            break;
        }
        tokio::fs::remove_dir(dir).await?;
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::cleanup_empty_ancestors;

    #[tokio::test]
    async fn removes_empty_dirs_up_to_but_not_including_the_root() {
        let root = tempfile::tempdir().unwrap();
        let leaf = root.path().join("Artist/Album");
        std::fs::create_dir_all(&leaf).unwrap();

        cleanup_empty_ancestors(&leaf, root.path()).await.unwrap();

        assert!(!root.path().join("Artist").exists());
        assert!(root.path().exists());
    }

    #[tokio::test]
    async fn stops_at_the_first_dir_that_still_holds_something() {
        let root = tempfile::tempdir().unwrap();
        let leaf = root.path().join("Artist/Album");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(root.path().join("Artist/cover.jpg"), b"").unwrap();

        cleanup_empty_ancestors(&leaf, root.path()).await.unwrap();

        assert!(!leaf.exists());
        assert!(root.path().join("Artist/cover.jpg").exists());
    }

    #[tokio::test]
    async fn never_removes_the_root_even_when_it_is_empty() {
        let root = tempfile::tempdir().unwrap();

        cleanup_empty_ancestors(root.path(), root.path())
            .await
            .unwrap();

        assert!(root.path().exists());
    }

    #[tokio::test]
    async fn leaves_a_dir_outside_the_root_alone() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("downloads");
        let outside = base.path().join("elsewhere/empty");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        cleanup_empty_ancestors(&outside, &root).await.unwrap();

        assert!(outside.exists());
    }
}
