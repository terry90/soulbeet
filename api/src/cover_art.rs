//! Authenticated Web cover-art proxy (`GET /api/cover-art/{id}?size={size}`).
//!
//! Proxies Navidrome Subsonic `getCoverArt` requests using the signed-in user's
//! server-side Navidrome credentials, verifies the response is a real image (not a
//! Subsonic JSON/XML error envelope), and rejects Navidrome's built-in blue vinyl
//! and gray star placeholders (both raw and resized) with `404 Not Found` so the
//! frontend renders Soulbeet's own Midnight Studio fallback artwork.

use std::sync::LazyLock;
use std::time::Duration;

use axum::{
    extract::{Path, Query},
    http::{
        header::{CACHE_CONTROL, CONTENT_TYPE, VARY},
        HeaderName, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;

use crate::server_fns::AuthSession;

const ROUTE_PATH: &str = "/api/cover-art/{id}";
const MAX_COVER_ART_ID_LEN: usize = 256;
const MAX_COVER_ART_BYTES: usize = 10 * 1024 * 1024;
const MAX_DECODE_DIMENSION: u32 = 2048;
const MIN_DECODE_DIMENSION: u32 = 32;
const MAX_DECODE_ALLOC_BYTES: u64 = 32 * 1024 * 1024;
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");
const VARY_AUTH: HeaderValue = HeaderValue::from_static("Cookie, Authorization");

const NAVIDROME_ALBUM_PLACEHOLDER_LEN: u64 = 69_228;
const NAVIDROME_ALBUM_PLACEHOLDER_FNV1A: u64 = 0xbed0_ba17_fa90_9643;
const NAVIDROME_ARTIST_PLACEHOLDER_LEN: u64 = 546;
const NAVIDROME_ARTIST_PLACEHOLDER_FNV1A: u64 = 0xa027_5d7a_1708_302f;

static COVER_ART_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("cover art proxy HTTP client")
});

#[derive(Debug, Deserialize)]
pub struct CoverArtQuery {
    pub size: Option<u32>,
}

/// Builds the cover-art proxy router to merge into the Axum app router.
pub fn router() -> Router {
    Router::new().route(ROUTE_PATH, get(get_cover_art))
}

/// Validates a cover-art identifier against empty values, excessive length,
/// path traversal sequences, slashes, and control characters.
pub fn validate_cover_art_id(raw: &str) -> Option<&str> {
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f)
        || raw.contains('/')
        || raw.contains('\\')
        || raw.contains("..")
    {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_COVER_ART_ID_LEN {
        return None;
    }
    Some(trimmed)
}

fn sanitize_image_content_type(raw: &str) -> Option<&'static str> {
    let media_type = raw.split(';').next()?.trim().to_ascii_lowercase();
    match media_type.as_str() {
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/png" => Some("image/png"),
        "image/webp" => Some("image/webp"),
        "image/gif" => Some("image/gif"),
        "image/avif" => Some("image/avif"),
        _ => None,
    }
}

fn placeholder_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (CACHE_CONTROL, HeaderValue::from_static("private, max-age=300")),
            (VARY, VARY_AUTH),
        ],
    )
        .into_response()
}

fn unconfigured_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (VARY, VARY_AUTH),
        ],
    )
        .into_response()
}

fn bad_gateway_no_store() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (VARY, VARY_AUTH),
        ],
    )
        .into_response()
}

fn cache_control_for_id(id: &str) -> &'static str {
    if id.starts_with("pl-") || id.starts_with("ar-") {
        "private, max-age=3600"
    } else {
        "private, max-age=86400"
    }
}

async fn get_cover_art(
    auth: AuthSession,
    Path(id): Path<String>,
    Query(query): Query<CoverArtQuery>,
) -> Response {
    let Some(valid_id) = validate_cover_art_id(&id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let size = query.size.map(|s| s.clamp(64, 1024));

    let navidrome = match crate::services::navidrome_client_for_user(&auth.0.user_id).await {
        Ok(client) => client,
        Err(_) => return unconfigured_not_found(),
    };

    let Ok(url) = navidrome.cover_art_url(valid_id, size) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let mut resp = match COVER_ART_CLIENT.get(url).send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!("Cover art upstream fetch failed for {valid_id}: {err}");
            return bad_gateway_no_store();
        }
    };

    let status = resp.status();
    if !status.is_success() {
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tracing::warn!("Cover art upstream returned transient status {status} for {valid_id}");
            return bad_gateway_no_store();
        }
        return placeholder_not_found();
    }

    let Some(content_type) = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(sanitize_image_content_type)
    else {
        return placeholder_not_found();
    };

    if let Some(len) = resp.content_length() {
        if len == 0 || len > MAX_COVER_ART_BYTES as u64 {
            return placeholder_not_found();
        }
    }

    let mut bytes = Vec::with_capacity(
        resp.content_length()
            .map(|n| (n as usize).min(256 * 1024))
            .unwrap_or(16 * 1024),
    );
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > MAX_COVER_ART_BYTES {
                    return placeholder_not_found();
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(err) => {
                tracing::warn!("Cover art upstream stream read failed for {valid_id}: {err}");
                return bad_gateway_no_store();
            }
        }
    }

    if bytes.is_empty() {
        return placeholder_not_found();
    }

    let cache_control = cache_control_for_id(valid_id);

    // Fast-path byte match before offloading image decoding to a blocking thread.
    if is_exact_navidrome_placeholder_bytes(&bytes) {
        return placeholder_not_found();
    }

    let check_bytes = bytes;
    let (bytes, is_placeholder) = match tokio::task::spawn_blocking(move || {
        let placeholder = is_decoded_placeholder(&check_bytes);
        (check_bytes, placeholder)
    })
    .await
    {
        Ok(res) => res,
        Err(err) => {
            tracing::warn!("Cover art placeholder check task failed for {valid_id}: {err}");
            return bad_gateway_no_store();
        }
    };

    if is_placeholder {
        return placeholder_not_found();
    }

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (CACHE_CONTROL, HeaderValue::from_static(cache_control)),
            (VARY, VARY_AUTH),
        ],
        bytes,
    )
        .into_response()
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn is_exact_navidrome_placeholder_bytes(bytes: &[u8]) -> bool {
    let len = bytes.len() as u64;
    if len != NAVIDROME_ALBUM_PLACEHOLDER_LEN && len != NAVIDROME_ARTIST_PLACEHOLDER_LEN {
        return false;
    }
    let hash = fnv1a_64(bytes);
    (len == NAVIDROME_ALBUM_PLACEHOLDER_LEN && hash == NAVIDROME_ALBUM_PLACEHOLDER_FNV1A)
        || (len == NAVIDROME_ARTIST_PLACEHOLDER_LEN && hash == NAVIDROME_ARTIST_PLACEHOLDER_FNV1A)
}

/// Detects Navidrome's built-in `album-placeholder.webp` (blue vinyl logo) and
/// `artist-placeholder.webp` / Last.fm gray star placeholders, both in their raw
/// form and after server-side resizing/transcoding to JPEG, PNG, or WebP.
pub fn is_navidrome_placeholder_bytes(bytes: &[u8]) -> bool {
    if is_exact_navidrome_placeholder_bytes(bytes) {
        return true;
    }
    is_decoded_placeholder(bytes)
}

fn is_decoded_placeholder(bytes: &[u8]) -> bool {
    let Ok(reader) = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()
    else {
        return false;
    };
    let Ok((w, h)) = reader.into_dimensions() else {
        return false;
    };
    if w < MIN_DECODE_DIMENSION
        || h < MIN_DECODE_DIMENSION
        || w > MAX_DECODE_DIMENSION
        || h > MAX_DECODE_DIMENSION
    {
        return false;
    }
    let min_dim = u64::from(w.min(h));
    let max_dim = u64::from(w.max(h));
    if max_dim * 100 > min_dim * 105 {
        return false;
    }

    let Ok(mut reader) =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()
    else {
        return false;
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);

    let Ok(img) = reader.decode() else {
        return false;
    };
    let rgba = img.to_rgba8();
    is_blue_vinyl_placeholder(&rgba) || is_gray_star_placeholder(&rgba)
}

fn sample_frac(rgba: &image::RgbaImage, fx: f32, fy: f32) -> [u8; 4] {
    let w = rgba.width();
    let h = rgba.height();
    let x = ((w as f32 * fx) as u32).min(w.saturating_sub(1));
    let y = ((h as f32 * fy) as u32).min(h.saturating_sub(1));
    rgba.get_pixel(x, y).0
}

fn is_nearly_square(w: u32, h: u32) -> bool {
    if w < 16 || h < 16 {
        return false;
    }
    let max_dim = w.max(h);
    w.abs_diff(h) <= (max_dim / 20).max(1)
}

fn channel_spread(p: [u8; 4]) -> u8 {
    let max_c = p[0].max(p[1]).max(p[2]);
    let min_c = p[0].min(p[1]).min(p[2]);
    max_c - min_c
}

fn is_navidrome_blue(p: [u8; 4]) -> bool {
    let [r, g, b, a] = p;
    a >= 200
        && b >= 150
        && r <= 85
        && b >= g.saturating_add(50)
        && b >= r.saturating_add(100)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VinylCornerBg {
    Transparent,
    Black,
    White,
}

fn classify_vinyl_corner(p: [u8; 4]) -> Option<VinylCornerBg> {
    let [r, g, b, a] = p;
    if a < 32 {
        Some(VinylCornerBg::Transparent)
    } else if r <= 25 && g <= 25 && b <= 25 {
        Some(VinylCornerBg::Black)
    } else if r >= 230 && g >= 230 && b >= 230 {
        Some(VinylCornerBg::White)
    } else {
        None
    }
}

fn matches_vinyl_bg(p: [u8; 4], bg: VinylCornerBg) -> bool {
    let [r, g, b, a] = p;
    match bg {
        VinylCornerBg::Transparent => a < 64,
        VinylCornerBg::Black => a >= 200 && r <= 30 && g <= 30 && b <= 30,
        VinylCornerBg::White => a >= 200 && r >= 225 && g >= 225 && b >= 225,
    }
}

/// Detects Navidrome's blue vinyl logo (`album-placeholder.webp`) raw or resized
/// (JPEG on black/white background, PNG, or WebP with alpha).
fn is_blue_vinyl_placeholder(rgba: &image::RgbaImage) -> bool {
    let (w, h) = rgba.dimensions();
    if !is_nearly_square(w, h) {
        return false;
    }

    let corners = [
        sample_frac(rgba, 0.02, 0.02),
        sample_frac(rgba, 0.98, 0.02),
        sample_frac(rgba, 0.02, 0.98),
        sample_frac(rgba, 0.98, 0.98),
    ];
    let Some(bg) = classify_vinyl_corner(corners[0]) else {
        return false;
    };
    if !corners.iter().all(|&c| classify_vinyl_corner(c) == Some(bg)) {
        return false;
    }

    // Center spindle area is dark charcoal neutral.
    let center = sample_frac(rgba, 0.50, 0.50);
    if center[3] < 200
        || center[0] > 75
        || center[1] > 75
        || center[2] > 75
        || channel_spread(center) > 30
    {
        return false;
    }

    // Symmetric blue vinyl ring points around the center.
    let ring_points = [
        sample_frac(rgba, 0.25, 0.50),
        sample_frac(rgba, 0.75, 0.50),
        sample_frac(rgba, 0.50, 0.25),
        sample_frac(rgba, 0.50, 0.75),
        sample_frac(rgba, 0.25, 0.25),
        sample_frac(rgba, 0.75, 0.25),
        sample_frac(rgba, 0.25, 0.75),
        sample_frac(rgba, 0.75, 0.75),
    ];
    let blue_ring_hits = ring_points.iter().filter(|&&p| is_navidrome_blue(p)).count();
    if blue_ring_hits < 7 {
        return false;
    }

    // Sample a 32x32 grid to verify overall color distribution and reject normal blue covers.
    const GRID: u32 = 32;
    let total = (GRID * GRID) as usize;
    let mut blue_count = 0usize;
    let mut bg_count = 0usize;
    let mut other_chromatic_count = 0usize;

    for gy in 0..GRID {
        let y = ((gy as f32 + 0.5) / GRID as f32 * h as f32) as u32;
        let y = y.min(h - 1);
        for gx in 0..GRID {
            let x = ((gx as f32 + 0.5) / GRID as f32 * w as f32) as u32;
            let x = x.min(w - 1);
            let p = rgba.get_pixel(x, y).0;
            if matches_vinyl_bg(p, bg) {
                bg_count += 1;
            } else if is_navidrome_blue(p) {
                blue_count += 1;
            } else if p[3] >= 128 && channel_spread(p) > 40 {
                // Allow anti-aliased blue edge pixels (blue still dominant).
                let is_blue_edge = p[2] > p[0].saturating_add(30) && p[2] > p[1].saturating_add(15);
                if !is_blue_edge {
                    other_chromatic_count += 1;
                }
            }
        }
    }

    let blue_pct = blue_count * 100 / total;
    let bg_pct = bg_count * 100 / total;
    let other_pct = other_chromatic_count * 100 / total;

    (32..=68).contains(&blue_pct) && (22..=58).contains(&bg_pct) && other_pct <= 3
}

/// Detects Navidrome's `artist-placeholder.webp` and Last.fm's gray star placeholder
/// (uniform light-gray canvas with a centered white star, zero dark photographic contrast).
fn is_gray_star_placeholder(rgba: &image::RgbaImage) -> bool {
    let (w, h) = rgba.dimensions();
    if !is_nearly_square(w, h) {
        return false;
    }

    let corners = [
        sample_frac(rgba, 0.02, 0.02),
        sample_frac(rgba, 0.98, 0.02),
        sample_frac(rgba, 0.02, 0.98),
        sample_frac(rgba, 0.98, 0.98),
    ];
    let mut bg_sum = 0u32;
    for &c in &corners {
        if c[3] < 200 || channel_spread(c) > 14 {
            return false;
        }
        let lum = (u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2])) / 3;
        if !(150..=245).contains(&lum) {
            return false;
        }
        bg_sum += lum;
    }
    let bg = (bg_sum / 4) as u8;
    for &c in &corners {
        let lum = ((u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2])) / 3) as u8;
        if lum.abs_diff(bg) > 14 {
            return false;
        }
    }

    // Center and upper star tip must be bright near-white above the gray background.
    let center = sample_frac(rgba, 0.50, 0.50);
    let upper_tip = sample_frac(rgba, 0.50, 0.25);
    for p in [center, upper_tip] {
        let lum = ((u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3) as u8;
        if p[3] < 200 || channel_spread(p) > 14 || lum < 235 || lum < bg.saturating_add(10) {
            return false;
        }
    }

    // Outer diagonal quadrants sit on the flat gray canvas outside the star.
    let outer_diags = [
        sample_frac(rgba, 0.20, 0.20),
        sample_frac(rgba, 0.80, 0.20),
        sample_frac(rgba, 0.20, 0.80),
        sample_frac(rgba, 0.80, 0.80),
    ];
    for p in outer_diags {
        let lum = ((u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3) as u8;
        if p[3] < 200 || channel_spread(p) > 16 || lum.abs_diff(bg) > 16 {
            return false;
        }
    }

    const GRID: u32 = 32;
    let total = (GRID * GRID) as usize;
    let mut achromatic = 0usize;
    let mut bg_pixels = 0usize;
    let mut star_pixels = 0usize;
    let mut darker_than_bg = 0usize;

    for gy in 0..GRID {
        let y = ((gy as f32 + 0.5) / GRID as f32 * h as f32) as u32;
        let y = y.min(h - 1);
        for gx in 0..GRID {
            let x = ((gx as f32 + 0.5) / GRID as f32 * w as f32) as u32;
            let x = x.min(w - 1);
            let p = rgba.get_pixel(x, y).0;
            if p[3] < 200 {
                return false;
            }
            if channel_spread(p) <= 16 {
                achromatic += 1;
            }
            let lum = ((u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3) as u8;
            if lum.abs_diff(bg) <= 18 {
                bg_pixels += 1;
            } else if lum >= 235 && lum >= bg.saturating_add(10) {
                star_pixels += 1;
            } else if lum < bg.saturating_sub(25) {
                darker_than_bg += 1;
            }
        }
    }

    let achromatic_pct = achromatic * 100 / total;
    let bg_pct = bg_pixels * 100 / total;
    let star_pct = star_pixels * 100 / total;
    let dark_pct = darker_than_bg * 100 / total;

    achromatic_pct >= 97 && bg_pct >= 45 && (10..=50).contains(&star_pct) && dark_pct == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    fn encode_image(img: &RgbaImage, fmt: ImageFormat) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let dyn_img = if fmt == ImageFormat::Jpeg {
            image::DynamicImage::ImageRgba8(img.clone()).into_rgb8().into()
        } else {
            image::DynamicImage::ImageRgba8(img.clone())
        };
        dyn_img.write_to(&mut buf, fmt).unwrap();
        buf.into_inner()
    }

    /// Synthesizes a 256x256 image matching Navidrome's blue vinyl logo proportions.
    fn synthetic_blue_vinyl(bg: Rgba<u8>) -> RgbaImage {
        let size = 256u32;
        let center = (size as f32) / 2.0;
        let outer_r = (size as f32) * 0.42;
        let inner_r = (size as f32) * 0.14;
        RgbaImage::from_fn(size, size, |x, y| {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= inner_r {
                Rgba([32, 31, 27, 255])
            } else if dist <= outer_r {
                Rgba([2, 95, 215, 255])
            } else {
                bg
            }
        })
    }

    /// Synthesizes a 256x256 image matching Navidrome/Last.fm's gray star placeholder.
    fn synthetic_gray_star(bg_gray: u8) -> RgbaImage {
        let size = 256u32;
        let cx = (size as f32) / 2.0;
        let cy = (size as f32) / 2.0;
        let outer_r = (size as f32) * 0.34;
        let inner_r = (size as f32) * 0.14;
        // Build 5-pointed star polygon vertices (pointing straight up at -90 deg).
        let mut verts = [(0.0f32, 0.0f32); 10];
        for (i, slot) in verts.iter_mut().enumerate() {
            let angle = -std::f32::consts::FRAC_PI_2 + (i as f32) * std::f32::consts::PI / 5.0;
            let r = if i % 2 == 0 { outer_r } else { inner_r };
            *slot = (cx + r * angle.cos(), cy + r * angle.sin());
        }
        RgbaImage::from_fn(size, size, |x, y| {
            let px = x as f32;
            let py = y as f32;
            let mut inside = false;
            let mut j = verts.len() - 1;
            for i in 0..verts.len() {
                let (xi, yi) = verts[i];
                let (xj, yj) = verts[j];
                let intersect = ((yi > py) != (yj > py))
                    && (px < (xj - xi) * (py - yi) / (yj - yi) + xi);
                if intersect {
                    inside = !inside;
                }
                j = i;
            }
            if inside {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([bg_gray, bg_gray, bg_gray, 255])
            }
        })
    }

    #[test]
    fn detects_resized_blue_vinyl_placeholder_in_jpeg_and_png() {
        let on_black = synthetic_blue_vinyl(Rgba([0, 0, 0, 255]));
        let jpeg_black = encode_image(&on_black, ImageFormat::Jpeg);
        assert!(is_navidrome_placeholder_bytes(&jpeg_black));

        let on_white = synthetic_blue_vinyl(Rgba([255, 255, 255, 255]));
        let jpeg_white = encode_image(&on_white, ImageFormat::Jpeg);
        assert!(is_navidrome_placeholder_bytes(&jpeg_white));

        let transparent = synthetic_blue_vinyl(Rgba([0, 0, 0, 0]));
        let png_alpha = encode_image(&transparent, ImageFormat::Png);
        assert!(is_navidrome_placeholder_bytes(&png_alpha));
    }

    #[test]
    fn detects_gray_star_placeholder_in_jpeg_and_png() {
        for bg in [235u8, 214u8, 200u8] {
            let star = synthetic_gray_star(bg);
            let jpeg = encode_image(&star, ImageFormat::Jpeg);
            let png = encode_image(&star, ImageFormat::Png);
            assert!(
                is_navidrome_placeholder_bytes(&jpeg),
                "expected JPEG gray star (bg={bg}) to be detected"
            );
            assert!(
                is_navidrome_placeholder_bytes(&png),
                "expected PNG gray star (bg={bg}) to be detected"
            );
        }
    }

    #[test]
    fn allows_normal_colorful_and_monochrome_cover_art() {
        // Colorful warm gradient cover art.
        let colorful = RgbaImage::from_fn(256, 256, |x, y| {
            Rgba([
                ((x * 255) / 256) as u8,
                ((y * 180) / 256) as u8,
                140,
                255,
            ])
        });
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &colorful,
            ImageFormat::Jpeg
        )));

        // Full-bleed blue album cover (blue corners, subject in center).
        let blue_album = RgbaImage::from_fn(256, 256, |x, y| {
            if (96..160).contains(&x) && (80..220).contains(&y) {
                Rgba([220, 175, 145, 255])
            } else {
                Rgba([10, 110, 210, 255])
            }
        });
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &blue_album,
            ImageFormat::Jpeg
        )));

        // High-contrast monochrome portrait (light background, dark subject).
        let bw_portrait = RgbaImage::from_fn(256, 256, |x, y| {
            let dx = x as f32 - 128.0;
            let dy = y as f32 - 128.0;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist < 70.0 {
                Rgba([35, 35, 35, 255])
            } else {
                Rgba([230, 230, 230, 255])
            }
        });
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &bw_portrait,
            ImageFormat::Jpeg
        )));
    }

    #[test]
    fn validates_cover_art_ids_and_rejects_traversal_and_control_chars() {
        assert_eq!(validate_cover_art_id("al-12345"), Some("al-12345"));
        assert_eq!(validate_cover_art_id("pl-abc_123"), Some("pl-abc_123"));
        assert_eq!(validate_cover_art_id("  mf-99  "), Some("mf-99"));

        assert_eq!(validate_cover_art_id(""), None);
        assert_eq!(validate_cover_art_id("   "), None);
        assert_eq!(validate_cover_art_id("../etc/passwd"), None);
        assert_eq!(validate_cover_art_id("al..123"), None);
        assert_eq!(validate_cover_art_id("al/123"), None);
        assert_eq!(validate_cover_art_id("al\\123"), None);
        assert_eq!(validate_cover_art_id("al-1\0"), None);
        assert_eq!(validate_cover_art_id("al-1\n"), None);
        assert_eq!(validate_cover_art_id(&"a".repeat(257)), None);
        assert_eq!(
            validate_cover_art_id(&"a".repeat(256)).map(str::len),
            Some(256)
        );
    }

    #[test]
    fn cache_control_differentiates_mutable_and_immutable_ids() {
        assert_eq!(cache_control_for_id("pl-123"), "private, max-age=3600");
        assert_eq!(cache_control_for_id("ar-456"), "private, max-age=3600");
        assert_eq!(cache_control_for_id("al-789"), "private, max-age=86400");
        assert_eq!(cache_control_for_id("mf-001"), "private, max-age=86400");
    }

    #[test]
    fn sanitizes_image_content_types_and_rejects_non_images() {
        assert_eq!(
            sanitize_image_content_type("image/jpeg; charset=binary"),
            Some("image/jpeg")
        );
        assert_eq!(sanitize_image_content_type("image/jpg"), Some("image/jpeg"));
        assert_eq!(sanitize_image_content_type("image/png"), Some("image/png"));
        assert_eq!(sanitize_image_content_type("image/webp"), Some("image/webp"));
        assert_eq!(sanitize_image_content_type("image/gif"), Some("image/gif"));
        assert_eq!(sanitize_image_content_type("image/avif"), Some("image/avif"));
        assert_eq!(sanitize_image_content_type("image/svg+xml"), None);
        assert_eq!(sanitize_image_content_type("image/bmp"), None);
        assert_eq!(sanitize_image_content_type("application/json"), None);
        assert_eq!(sanitize_image_content_type("text/xml; charset=utf-8"), None);
    }

    #[test]
    fn skips_decode_for_tiny_non_square_or_oversized_images() {
        let tiny = RgbaImage::from_pixel(16, 16, Rgba([235, 235, 235, 255]));
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &tiny,
            ImageFormat::Png
        )));

        let wide = RgbaImage::from_pixel(256, 128, Rgba([235, 235, 235, 255]));
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &wide,
            ImageFormat::Png
        )));

        let oversized = RgbaImage::from_pixel(2049, 2049, Rgba([235, 235, 235, 255]));
        assert!(!is_navidrome_placeholder_bytes(&encode_image(
            &oversized,
            ImageFormat::Png
        )));
    }

    #[test]
    fn response_helpers_set_expected_status_cache_and_vary_headers() {
        let not_found = placeholder_not_found();
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            not_found.headers().get(CACHE_CONTROL).and_then(|v| v.to_str().ok()),
            Some("private, max-age=300")
        );
        assert_eq!(
            not_found.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie, Authorization")
        );

        let bad_gw = bad_gateway_no_store();
        assert_eq!(bad_gw.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            bad_gw.headers().get(CACHE_CONTROL).and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(
            bad_gw.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie, Authorization")
        );
    }
}
