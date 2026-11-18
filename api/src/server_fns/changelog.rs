use dioxus::prelude::*;
use shared::changelog::Version;

#[cfg(feature = "server")]
use super::server_error;
#[cfg(feature = "server")]
use crate::{db::DB, models::user::User, AuthSession};

/// The version whose release notes the signed-in user last closed.
#[get("/api/changelog/seen", auth: AuthSession)]
pub async fn get_changelog_seen() -> Result<Version, ServerFnError> {
    User::changelog_seen_in(&DB, &auth.0.user_id)
        .await
        .map_err(server_error)
}

/// Records that the signed-in user closed the release notes up to `running`,
/// the version of the page that showed them, on every device they use.
#[post("/api/changelog/mark-seen", auth: AuthSession)]
pub async fn mark_changelog_seen(running: Version) -> Result<(), ServerFnError> {
    User::mark_changelog_seen_in(&DB, &auth.0.user_id, version_to_record(running))
        .await
        .map_err(server_error)
}

/// A page left open across an upgrade runs the older version and showed only
/// its notes, so the newer ones stay unread; no page records past this server.
#[cfg(feature = "server")]
fn version_to_record(page_running: Version) -> Version {
    page_running.min(shared::changelog::running_version())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use shared::changelog::running_version;

    #[test]
    fn the_older_of_the_page_and_the_server_is_recorded() {
        let running = running_version();
        assert_eq!(version_to_record(running), running);
        let older = Version::parse("0.0.1").unwrap();
        assert_eq!(version_to_record(older), older);
        let newer = Version {
            major: running.major + 1,
            ..running
        };
        assert_eq!(version_to_record(newer), running);
    }
}
