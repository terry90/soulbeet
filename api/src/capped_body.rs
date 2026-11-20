//! Reading an answer's body with a size limit, so a server Soulbeet asks
//! for a small document cannot make it hold a large one.

/// Why [`read_capped`] gave up on a body.
#[derive(Debug)]
pub(crate) enum CappedBodyError {
    /// The body is larger than `cap` bytes.
    TooLarge {
        cap: usize,
    },
    Read(reqwest::Error),
}

impl std::fmt::Display for CappedBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CappedBodyError::TooLarge { cap } => {
                write!(f, "the body is larger than {cap} bytes")
            }
            CappedBodyError::Read(e) => write!(f, "cannot read the body: {e}"),
        }
    }
}

/// The body of `response`, refused when its Content-Length is over `cap`
/// bytes, or as soon as the chunks read pass it.
pub(crate) async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, CappedBodyError> {
    let too_large = CappedBodyError::TooLarge { cap };
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err(too_large);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(CappedBodyError::Read)? {
        if body.len() + chunk.len() > cap {
            return Err(too_large);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::header::CONTENT_LENGTH;
    use axum::{routing::get, Router};
    use futures::StreamExt;

    use super::*;
    use crate::connection::test_util::{announced_never_sent, serve_on_loopback, streamed};

    const CAP: usize = 100 * 1024;

    async fn read(app: Router) -> Result<Vec<u8>, CappedBodyError> {
        let (addr, _server) = serve_on_loopback(app).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client.get(format!("http://{addr}/")).send().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), read_capped(response, CAP))
            .await
            .expect("the body is read past its announced length")
    }

    #[tokio::test]
    async fn a_body_up_to_the_cap_is_read_whole() {
        let at_cap = vec![b'a'; CAP];
        let body = at_cap.clone();
        let read = read(Router::new().route("/", get(move || async move { streamed(body) })))
            .await
            .unwrap();
        assert_eq!(read, at_cap);
    }

    #[tokio::test]
    async fn a_body_past_the_cap_is_refused_once_its_chunks_pass_it() {
        let refused =
            read(Router::new().route("/", get(|| async { streamed(vec![b'a'; CAP + 1]) })))
                .await
                .unwrap_err();
        assert!(
            matches!(refused, CappedBodyError::TooLarge { cap: CAP }),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn an_announced_length_past_the_cap_is_refused_unread() {
        let refused =
            read(Router::new().route("/", get(|| async { announced_never_sent(CAP + 1) })))
                .await
                .unwrap_err();
        assert_eq!(
            refused.to_string(),
            format!("the body is larger than {CAP} bytes")
        );
    }

    #[tokio::test]
    async fn a_body_cut_short_is_a_read_error() {
        // The connection drops after the first chunk has reached the reader.
        let cut = || async {
            let half = futures::stream::iter([Ok::<_, std::io::Error>(&b"half"[..])]);
            let gone = futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(std::io::Error::other("gone"))
            });
            ([(CONTENT_LENGTH, "8")], Body::from_stream(half.chain(gone)))
        };
        let refused = read(Router::new().route("/", get(cut))).await.unwrap_err();
        assert!(matches!(refused, CappedBodyError::Read(_)), "{refused}");
        assert!(
            refused.to_string().starts_with("cannot read the body: "),
            "{refused}"
        );
    }
}
