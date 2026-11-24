//! Compression of what the server sends.
//!
//! dx writes a brotli copy of each bundled file at build time (`Dioxus.toml`)
//! and Dioxus sends it to clients that take br, so the wasm bundle is not
//! compressed again on every request. Everything else is gzipped here as it
//! is sent: pages, server function answers, and the bundled files for
//! clients that do not take br, which browsers often leave out on plain http.

use axum::http::header::{CONTENT_ENCODING, VARY};
use axum::http::HeaderValue;
use axum::middleware::map_response;
use axum::response::Response;
use axum::Router;
use tower_http::compression::CompressionLayer;

/// `router` with its answers gzipped for clients that take gzip.
///
/// Brotli at its default level is far too slow to run per request, so it is
/// left to the build. Event streams are never compressed, so no event waits
/// in the encoder; a WebSocket upgrade has an empty body and passes as is.
/// Pages render in full before they are sent (Dioxus streaming is off), so
/// gzip holds none of them back either.
pub(crate) fn compress(router: Router) -> Router {
    router
        .layer(map_response(vary_on_encoding))
        .layer(CompressionLayer::new().no_br())
}

/// Dioxus sends a file's brotli copy without `Vary: Accept-Encoding`, and a
/// shared cache would then hand brotli to a client that never asked for it.
async fn vary_on_encoding(mut response: Response) -> Response {
    let headers = response.headers_mut();
    let varies = headers.get_all(VARY).iter().any(|value| {
        value
            .to_str()
            .is_ok_and(|value| value.to_ascii_lowercase().contains("accept-encoding"))
    });
    if headers.contains_key(CONTENT_ENCODING) && !varies {
        headers.append(VARY, HeaderValue::from_static("accept-encoding"));
    }
    response
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::io::Read;
    use std::path::Path;
    use std::time::Duration;

    use axum::body::{to_bytes, Body, Bytes};
    use axum::http::header::ACCEPT_ENCODING;
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::response::sse::{Event, Sse};
    use axum::routing::get;
    use dioxus::prelude::*;
    use dioxus::server::{DioxusRouterExt, ServeConfig};
    use futures::{Stream, StreamExt};
    use tower::ServiceExt;
    use tower_http::services::ServeFile;

    use super::*;

    const CSS: &str = "/assets/tailwind.css";
    /// Long enough to be worth compressing, were it not already.
    const STORED: &[u8] = &[0x1b; 256];

    fn css_on_disk() -> Vec<u8> {
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/tailwind.css")).unwrap()
    }

    #[component]
    fn Page() -> Element {
        rsx! {
            main {
                for row in 0..40 {
                    p { "Row {row} of a page that is worth compressing" }
                }
            }
        }
    }

    /// One event, then a stream that stays open, like an MCP answer still
    /// running.
    fn one_event_then_open() -> impl Stream<Item = Result<Event, Infallible>> {
        futures::stream::once(async { Ok(Event::default().data("first")) })
            .chain(futures::stream::pending())
    }

    /// A page rendered by Dioxus, the stylesheet served the way Dioxus serves
    /// each bundled file (dioxus-server `serve_dir_cached`), an answer that
    /// is already brotli, and an event stream.
    fn app() -> Router {
        let css = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/tailwind.css");
        compress(
            Router::new()
                .route_service(CSS, ServeFile::new(css).precompressed_br())
                .route(
                    "/assets/copy.br",
                    get(|| async { ([(CONTENT_ENCODING, "br")], Bytes::from(STORED)) }),
                )
                .route(
                    "/assets/varies.br",
                    get(|| async {
                        (
                            [(CONTENT_ENCODING, "br"), (VARY, "Accept-Encoding")],
                            Bytes::from(STORED),
                        )
                    }),
                )
                .route("/events", get(|| async { Sse::new(one_event_then_open()) }))
                .merge(Router::new().serve_api_application(ServeConfig::new(), Page)),
        )
    }

    async fn get_with(path: &str, accept_encoding: Option<&str>) -> Response {
        let mut request = Request::get(path);
        if let Some(value) = accept_encoding {
            request = request.header(ACCEPT_ENCODING, value);
        }
        app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn encoding(headers: &HeaderMap) -> Option<&str> {
        headers.get(CONTENT_ENCODING).map(|v| v.to_str().unwrap())
    }

    fn varies_on_encoding(headers: &HeaderMap) -> bool {
        headers
            .get_all(VARY)
            .iter()
            .any(|v| v.to_str().unwrap().eq_ignore_ascii_case("accept-encoding"))
    }

    async fn gunzip(response: Response) -> Vec<u8> {
        let gzipped = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(&gzipped[..])
            .read_to_end(&mut plain)
            .unwrap();
        plain
    }

    #[tokio::test]
    async fn a_bundled_file_is_gzipped_for_a_client_that_takes_gzip() {
        let response = get_with(CSS, Some("gzip, deflate")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(encoding(response.headers()), Some("gzip"));
        assert!(varies_on_encoding(response.headers()));
        assert_eq!(gunzip(response).await, css_on_disk());
    }

    #[tokio::test]
    async fn a_bundled_file_is_sent_as_is_to_a_client_that_takes_no_encoding() {
        let response = get_with(CSS, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(encoding(response.headers()), None);
        assert!(varies_on_encoding(response.headers()));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, css_on_disk());
    }

    #[tokio::test]
    async fn a_page_is_gzipped_for_a_client_that_takes_gzip_and_sent_as_is_otherwise() {
        let gzipped = get_with("/", Some("gzip")).await;
        assert_eq!(gzipped.status(), StatusCode::OK);
        assert_eq!(encoding(gzipped.headers()), Some("gzip"));
        let page = String::from_utf8(gunzip(gzipped).await).unwrap();
        assert!(page.contains("Row 39 of a page"), "{page}");

        let plain = get_with("/", None).await;
        assert_eq!(encoding(plain.headers()), None);
        let body = to_bytes(plain.into_body(), usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), page);
    }

    #[tokio::test]
    async fn brotli_is_never_made_per_request() {
        let response = get_with("/", Some("br")).await;
        assert_eq!(encoding(response.headers()), None);
    }

    #[tokio::test]
    async fn an_answer_already_encoded_is_sent_as_stored_and_varies_on_accept_encoding() {
        let response = get_with("/assets/copy.br", Some("gzip, br")).await;
        assert_eq!(encoding(response.headers()), Some("br"));
        assert!(varies_on_encoding(response.headers()));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], STORED);
    }

    #[tokio::test]
    async fn an_answer_that_already_varies_on_accept_encoding_says_so_once() {
        let response = get_with("/assets/varies.br", Some("gzip, br")).await;
        assert_eq!(encoding(response.headers()), Some("br"));
        let vary: Vec<_> = response.headers().get_all(VARY).iter().collect();
        assert_eq!(vary, ["Accept-Encoding"]);
    }

    #[tokio::test]
    async fn an_event_stream_is_neither_encoded_nor_held_back() {
        let response = get_with("/events", Some("gzip, br")).await;
        assert_eq!(encoding(response.headers()), None);
        let mut body = response.into_body().into_data_stream();
        // The stream never ends, so the event arrives only if nothing waits
        // for more of it.
        let first = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("the first event arrives while the stream is open")
            .unwrap()
            .unwrap();
        assert_eq!(&first[..], b"data: first\n\n");
    }
}
