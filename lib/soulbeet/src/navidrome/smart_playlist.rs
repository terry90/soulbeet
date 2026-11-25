//! Native API helpers for custom Navidrome smart playlists (`POST /api/playlist`,
//! `PUT /api/playlist/{id}`).

use super::client::{native_failure, parse, NavidromeClient};
use super::error::{NavidromeError, Result};

impl NavidromeClient {
    /// Create a smart playlist via Navidrome's native API (`POST /api/playlist`) with arbitrary
    /// `.nsp` criteria rules (`model/criteria/criteria.go`).
    pub async fn create_custom_smart_playlist(
        &self,
        name: &str,
        comment: Option<&str>,
        public: bool,
        rules: &serde_json::Value,
    ) -> Result<String> {
        let url = self.base_url.join("api/playlist")?;

        let body = serde_json::json!({
            "name": name,
            "comment": comment.unwrap_or(""),
            "public": public,
            "rules": rules,
        });

        let resp = self
            .native_request(self.client.post(url).json(&body))
            .await?;

        if !resp.status().is_success() {
            return Err(native_failure("Create smart playlist", resp).await);
        }

        #[derive(serde::Deserialize)]
        struct PlaylistResp {
            id: String,
        }

        let pl: PlaylistResp = resp.json().await.map_err(parse)?;
        Ok(pl.id)
    }

    /// Update a smart playlist via Navidrome's native API (`PUT /api/playlist/{id}`), sending only
    /// the fields that are `Some`. Navidrome writes only the columns present in the JSON body
    /// (`deluan/rest` controller `Put` -> `persistence/playlist_repository.go` `Update`).
    /// When all fields are `None`, no request is sent.
    /// An `id` that is empty, `"."`, or `".."` returns a `Parse` error with no request.
    pub async fn update_custom_smart_playlist(
        &self,
        id: &str,
        name: Option<&str>,
        comment: Option<&str>,
        public: Option<bool>,
        rules: Option<&serde_json::Value>,
    ) -> Result<()> {
        if name.is_none() && comment.is_none() && public.is_none() && rules.is_none() {
            return Ok(());
        }
        if matches!(id, "" | "." | "..") {
            return Err(NavidromeError::Parse("No playlist id given".into()));
        }

        #[derive(serde::Serialize)]
        struct UpdateBody<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            comment: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            public: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            rules: Option<&'a serde_json::Value>,
        }

        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|()| NavidromeError::Config("base URL cannot hold a path".into()))?
            .pop_if_empty()
            .extend(["api", "playlist", id]);

        let body = UpdateBody {
            name,
            comment,
            public,
            rules,
        };

        let resp = self
            .native_request(self.client.put(url).json(&body))
            .await?;

        if !resp.status().is_success() {
            return Err(native_failure("Update smart playlist", resp).await);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::navidrome::NavidromeClientBuilder;
    use crate::test_http::{serve, Reply};

    fn client(base_url: &str) -> NavidromeClient {
        NavidromeClientBuilder::new()
            .base_url(base_url)
            .username("alice")
            .password("secret-password")
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn create_custom_smart_playlist_posts_rules_and_returns_id() {
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: r#"{"token":"jwt"}"#.into(),
            },
            Reply {
                status: 200,
                body: r#"{"id":"pl-smart-1"}"#.into(),
            },
        ]);
        let rules = json!({
            "all": [{"is": {"loved": true}}],
            "sort": "dateAdded",
            "order": "desc",
            "limit": 50
        });
        let id = client(&url)
            .create_custom_smart_playlist("Loved Tracks", Some("Auto-updated"), true, &rules)
            .await
            .unwrap();
        assert_eq!(id, "pl-smart-1");

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!((reqs[1].method.as_str(), reqs[1].target.as_str()), ("POST", "/api/playlist"));
        let sent: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert_eq!(
            sent,
            json!({
                "name": "Loved Tracks",
                "comment": "Auto-updated",
                "public": true,
                "rules": rules,
            })
        );
    }

    #[tokio::test]
    async fn update_custom_smart_playlist_puts_only_provided_fields() {
        let (url, seen) = serve(vec![
            Reply {
                status: 200,
                body: r#"{"token":"jwt"}"#.into(),
            },
            Reply {
                status: 200,
                body: r#"{"id":"pl-smart-1"}"#.into(),
            },
        ]);
        let rules = json!({"any": [{"gt": {"rating": 3}}]});
        client(&url)
            .update_custom_smart_playlist("pl-smart-1", None, Some("Top rated"), None, Some(&rules))
            .await
            .unwrap();

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            (reqs[1].method.as_str(), reqs[1].target.as_str()),
            ("PUT", "/api/playlist/pl-smart-1")
        );
        let sent: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert_eq!(
            sent,
            json!({
                "comment": "Top rated",
                "rules": rules,
            })
        );
    }

    #[tokio::test]
    async fn update_custom_smart_playlist_skips_empty_change_and_rejects_bad_id() {
        let (url, seen) = serve(vec![]);
        let c = client(&url);
        c.update_custom_smart_playlist("pl-smart-1", None, None, None, None)
            .await
            .unwrap();
        assert!(seen.lock().unwrap().is_empty());

        for bad in ["", ".", ".."] {
            let err = c
                .update_custom_smart_playlist(bad, Some("New"), None, None, None)
                .await
                .unwrap_err();
            assert!(matches!(err, NavidromeError::Parse(_)), "{bad}: {err:?}");
        }
        assert!(seen.lock().unwrap().is_empty());
    }
}
