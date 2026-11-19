use crate::polite::{Pace, PoliteClient, RemoteError};

use super::client::encode;
use super::models::{LabsNeighbour, SimilarTag};

/// ListenBrainz Labs, which needs no token. A retired similarity algorithm
/// answers HTTP 400 with an HTML page naming the permitted ones.
pub struct LabsClient {
    http: PoliteClient,
}

impl LabsClient {
    pub fn new(base_url: &str, pace: Pace) -> Result<Self, RemoteError> {
        let http = PoliteClient::new(base_url, "ListenBrainz Labs", pace)?;
        Ok(Self { http })
    }

    /// Neighbours in Labs order, unnamed ones included.
    pub async fn similar_artists(
        &self,
        artist_mbid: &str,
        algorithm: &str,
    ) -> Result<Vec<LabsNeighbour>, RemoteError> {
        let path =
            format!("/similar-artists/json?artist_mbids={artist_mbid}&algorithm={algorithm}");
        Ok(self.http.get(&path).await?.unwrap_or_default())
    }

    pub async fn tag_similarity(&self, tag: &str) -> Result<Vec<SimilarTag>, RemoteError> {
        let path = format!("/tag-similarity/json?tag={}", encode(tag));
        Ok(self.http.get(&path).await?.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn similar_artists_keep_unnamed_neighbours_for_the_caller_to_judge() {
        let (base, seen) = serve(vec![ok(include_str!(
            "fixtures/similar_artists_null_name.json"
        ))]);
        let labs = LabsClient::new(&base, Pace::immediate()).unwrap();
        let n = labs
            .similar_artists("b9e82c61-c1ec-4b42-9233-ca3567d2f2da", "algo")
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/similar-artists/json?artist_mbids=b9e82c61-c1ec-4b42-9233-ca3567d2f2da&algorithm=algo"
        );
        assert_eq!((n.len(), n[2].name.as_deref(), n[2].score), (3, None, 319));
        assert_eq!(
            (n[0].name.as_deref(), n[0].score, n[0].artist_mbid.as_str()),
            (
                Some("Ben Howard"),
                1284,
                "534dda3c-b73f-408b-8889-bd68eae84df6"
            )
        );
    }

    #[tokio::test]
    async fn a_retired_algorithm_is_a_400_with_the_page() {
        let (base, _) = serve(vec![Reply {
            status: 400,
            body: include_str!("fixtures/similar_artists_retired.html").into(),
        }]);
        let labs = LabsClient::new(&base, Pace::immediate()).unwrap();
        match labs
            .similar_artists("d0f25f16-7782-42e8-8047-c16a1cb2b450", "retired")
            .await
        {
            Err(RemoteError::Status {
                service: "ListenBrainz Labs",
                status: 400,
                body,
            }) => assert!(body.contains("permitted"), "{body}"),
            other => panic!("expected a 400, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tag_similarity_reads_its_list() {
        let (base, seen) = serve(vec![ok(include_str!(
            "fixtures/tag_similarity_desert_blues.json"
        ))]);
        let labs = LabsClient::new(&base, Pace::immediate()).unwrap();
        let tags = labs.tag_similarity("desert blues").await.unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/tag-similarity/json?tag=desert+blues"
        );
        assert_eq!(tags.len(), 8);
        assert_eq!(
            (tags[0].similar_tag.as_str(), tags[0].count),
            ("desert rock", 50)
        );
    }
}
