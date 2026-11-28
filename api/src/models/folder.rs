#[cfg(feature = "server")]
use crate::auth::Role;
#[cfg(feature = "server")]
use crate::db::DB;
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use sqlx::SqlitePool;
#[cfg(feature = "server")]
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct Folder {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub path: String,
}

/// A folder with its owner's name and role, as the folder rules read it.
#[cfg(feature = "server")]
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct OwnedFolder {
    #[sqlx(flatten)]
    pub folder: Folder,
    pub owner: String,
    pub owner_role: Role,
}

#[cfg(feature = "server")]
impl Folder {
    pub async fn create(user_id: &str, name: &str, path: &str) -> Result<Folder, String> {
        Self::create_in(&DB, user_id, name, path).await
    }

    pub(crate) async fn create_in(
        db: &SqlitePool,
        user_id: &str,
        name: &str,
        path: &str,
    ) -> Result<Folder, String> {
        let id = Uuid::new_v4().to_string();

        let folder = sqlx::query_as::<_, Folder>(
            "INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?) RETURNING *",
        )
        .bind(&id)
        .bind(user_id)
        .bind(name)
        .bind(path)
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())?;

        Ok(folder)
    }

    /// The single library folder owned by `user_id`, if configured.
    pub async fn get_for_user(user_id: &str) -> Result<Option<Folder>, String> {
        Self::get_for_user_in(&DB, user_id).await
    }

    pub(crate) async fn get_for_user_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Option<Folder>, String> {
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE user_id = ? LIMIT 1")
            .bind(user_id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())
    }

    /// Every folder with its owner, ordered by owner and name.
    pub(crate) async fn get_all_with_owners_in(
        db: &SqlitePool,
    ) -> Result<Vec<OwnedFolder>, String> {
        sqlx::query_as::<_, OwnedFolder>(
            "SELECT f.*, u.username AS owner, u.role AS owner_role \
             FROM folders f JOIN users u ON u.id = f.user_id ORDER BY u.username, f.name",
        )
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn get_all_by_user(user_id: &str) -> Result<Vec<Folder>, String> {
        Self::get_all_by_user_in(&DB, user_id).await
    }

    /// Returns the user's single folder (0 or 1 elements) in stable order.
    pub(crate) async fn get_all_by_user_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Vec<Folder>, String> {
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE user_id = ? ORDER BY rowid")
            .bind(user_id)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())
    }

    pub(crate) async fn update_in(
        db: &SqlitePool,
        id: &str,
        name: &str,
        path: &str,
    ) -> Result<(), String> {
        sqlx::query("UPDATE folders SET name = ?, path = ? WHERE id = ?")
            .bind(name)
            .bind(path)
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(id: &str) -> Result<(), String> {
        Self::delete_in(&DB, id).await
    }

    pub(crate) async fn delete_in(db: &SqlitePool, id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM folders WHERE id = ?")
            .bind(id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn get_by_id(id: &str) -> Result<Option<Folder>, String> {
        Self::get_by_id_in(&DB, id).await
    }

    pub(crate) async fn get_by_id_in(db: &SqlitePool, id: &str) -> Result<Option<Folder>, String> {
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())
    }

    /// The user's own folder at `path`. Paths compare by component, so a
    /// trailing slash does not matter and `..` never matches.
    pub async fn find_for_user_by_path(
        user_id: &str,
        path: &str,
    ) -> Result<Option<Folder>, String> {
        let folders = Self::get_all_by_user(user_id).await?;
        Ok(match_folder(&folders, path).cloned())
    }

    pub fn discovery_path(&self) -> String {
        format!("{}/Discovery", self.path)
    }

    pub fn discovery_profile_path(&self, profile: &str) -> String {
        format!("{}/Discovery/{}", self.path, profile)
    }

    pub async fn has_discovery_enabled(&self) -> Result<bool, String> {
        use crate::models::user_settings::UserSettings;
        let settings = UserSettings::get(&self.user_id).await?;
        Ok(settings.discovery_enabled)
    }
}

#[cfg(feature = "server")]
fn match_folder<'a>(folders: &'a [Folder], path: &str) -> Option<&'a Folder> {
    let wanted = std::path::Path::new(path);
    folders
        .iter()
        .find(|f| std::path::Path::new(&f.path) == wanted)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[test]
    fn folder_matching_is_component_wise() {
        let f = |p: &str| Folder {
            id: "1".into(),
            user_id: "u".into(),
            name: "n".into(),
            path: p.into(),
        };
        let folders = vec![f("/music/rock"), f("/music/jazz/")];
        assert_eq!(
            match_folder(&folders, "/music/rock/").map(|f| &f.path[..]),
            Some("/music/rock")
        );
        assert_eq!(
            match_folder(&folders, "/music/jazz").map(|f| &f.path[..]),
            Some("/music/jazz/")
        );
        assert!(match_folder(&folders, "/music/rock/../../etc").is_none());
        assert!(match_folder(&folders, "/elsewhere").is_none());
    }

    #[tokio::test]
    async fn a_user_may_hold_only_one_folder() {
        let db = test_db::migrated().await;
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1', 'alice', 'x')")
            .execute(&db)
            .await
            .unwrap();
        Folder::create_in(&db, "u1", "Music", "/music/alice")
            .await
            .unwrap();
        assert!(Folder::create_in(&db, "u1", "Archive", "/music/alice2")
            .await
            .is_err());
        let folder = Folder::get_for_user_in(&db, "u1").await.unwrap().unwrap();
        assert_eq!(folder.path, "/music/alice");
    }
}
