#[cfg(feature = "server")]
use crate::db::DB;
use serde::{Deserialize, Serialize};
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

#[cfg(feature = "server")]
impl Folder {
    pub async fn create(user_id: &str, name: &str, path: &str) -> Result<Folder, String> {
        let id = Uuid::new_v4().to_string();

        let folder = sqlx::query_as::<_, Folder>(
            "INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?) RETURNING *",
        )
        .bind(&id)
        .bind(user_id)
        .bind(name)
        .bind(path)
        .fetch_one(&*DB)
        .await
        .map_err(|e| e.to_string())?;

        Ok(folder)
    }

    pub async fn get_all_by_user(user_id: &str) -> Result<Vec<Folder>, String> {
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&*DB)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn update(id: &str, name: &str, path: &str) -> Result<(), String> {
        sqlx::query("UPDATE folders SET name = ?, path = ? WHERE id = ?")
            .bind(name)
            .bind(path)
            .bind(id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM folders WHERE id = ?")
            .bind(id)
            .execute(&*DB)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn get_by_id(id: &str) -> Result<Option<Folder>, String> {
        sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE id = ?")
            .bind(id)
            .fetch_optional(&*DB)
            .await
            .map_err(|e| e.to_string())
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
        Ok(settings.discovery_enabled && settings.discovery_folder_id.as_deref() == Some(&self.id))
    }
}
