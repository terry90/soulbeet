use dioxus::prelude::*;

pub mod auth;
pub mod changelog;
pub mod connections;
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
pub use changelog::*;
pub use connections::*;
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

/// A refusal that also tells the client, as `details`, what it changed on
/// the server, so the client can follow it instead of guessing.
pub fn refusal_error_with<E: std::fmt::Display, D: serde::Serialize>(
    e: E,
    details: D,
) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: REFUSAL_CODE,
        details: Some(serde_json::json!(details)),
    }
}

/// A typed failure as the browser gets it: a refusal whose message is the
/// failure's and whose details are the failure.
pub fn failure_error(failure: shared::failure::Failure) -> ServerFnError {
    refusal_error_with(&failure.message, &failure)
}

/// A refused retry or matches request as the browser gets it: the failure's
/// message, and as details the failure with what stays kept for the row.
pub fn import_refusal_error(refusal: shared::download::ImportRefusal) -> ServerFnError {
    refusal_error_with(&refusal.failure.message, &refusal)
}

/// Status of a request refused because of the caller's role.
pub const FORBIDDEN_CODE: u16 = 403;

pub fn forbidden_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: FORBIDDEN_CODE,
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

/// Status of a request refused because of too many failed sign-ins.
pub const RATE_LIMITED_CODE: u16 = 429;

pub fn rate_limited_error<E: std::fmt::Display>(e: E) -> ServerFnError {
    ServerFnError::ServerError {
        message: e.to_string(),
        code: RATE_LIMITED_CODE,
        details: None,
    }
}

/// Remove `dir` if empty, then each empty parent above it, stopping below
/// `root`. `root` itself is never removed, and a `dir` outside `root` is left
/// alone, so the walk can never climb out of the tree the caller owns (the
/// download directory, a library folder, a Discovery profile). Both are
/// compared by their real paths, so neither `..` nor a symlink leads out.
#[cfg(feature = "server")]
pub async fn cleanup_empty_ancestors(
    dir: &std::path::Path,
    root: &std::path::Path,
) -> Result<(), std::io::Error> {
    let root = tokio::fs::canonicalize(root).await?;
    let mut dir = tokio::fs::canonicalize(dir).await?;
    while dir != root && dir.starts_with(&root) {
        if tokio::fs::read_dir(&dir)
            .await?
            .next_entry()
            .await?
            .is_some()
        {
            break;
        }
        tokio::fs::remove_dir(&dir).await?;
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::{cleanup_empty_ancestors, failure_error, REFUSAL_CODE};
    use dioxus::prelude::ServerFnError;
    use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};

    #[test]
    fn a_failure_reaches_the_browser_as_a_refusal_carrying_it() {
        let f = Failure::new(
            FailureCode::UnknownHandle,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "Folder not found",
        );
        let sent: ServerFnError =
            serde_json::from_value(serde_json::to_value(failure_error(f.clone())).unwrap())
                .unwrap();
        let ServerFnError::ServerError {
            code,
            message,
            details,
        } = sent
        else {
            panic!("not a server error")
        };
        assert_eq!((code, message.as_str()), (REFUSAL_CODE, "Folder not found"));
        assert_eq!(
            serde_json::from_value::<Failure>(details.unwrap()).unwrap(),
            f
        );
    }

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

        // Lexically under the root, really outside it.
        let stepped = root.join("../elsewhere/empty");
        cleanup_empty_ancestors(&stepped, &root).await.unwrap();
        assert!(outside.exists());

        std::os::unix::fs::symlink(base.path().join("elsewhere"), root.join("link")).unwrap();
        cleanup_empty_ancestors(&root.join("link/empty"), &root)
            .await
            .unwrap();
        assert!(outside.exists());
    }
}
