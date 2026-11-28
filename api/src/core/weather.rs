#![cfg(feature = "server")]

//! Local weather resolution for weather-reactive contextual mixes.
//!
//! Location resolution priority:
//! 1. Live coarse coordinates provided by an opted-in client (`client_coords`), rounded to `0.1`
//!    degrees, which skips both saved city coordinates and IP geolocation so weather follows a
//!    moving listener.
//! 2. Saved user city coordinates (`UserCultureProfile.weather_lat` / `weather_lon`),
//!    which skips IP geolocation entirely.
//! 3. Reverse-proxy client public IP (`X-Real-IP` -> rightmost public `X-Forwarded-For`
//!    -> `CF-Connecting-IP`) resolved via GeoJS (`https://get.geojs.io/v1/ip/geo/{ip}.json`), or
//!    server egress IP (`https://get.geojs.io/v1/ip/geo.json`) when on a LAN/private network.
//!
//! Live weather is fetched from Open-Meteo with coordinates rounded to `0.1` degrees for
//! privacy and cache sharing, under a strict 2.5s timeout so Discover never blocks or fails
//! when upstream services are unreachable.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde::Deserialize;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::recommendation::{classify_wmo_weather, UserCultureProfile, WeatherSnapshot};
use tracing::debug;

const GEOJS_BASE_URL: &str = "https://get.geojs.io";
const OPEN_METEO_FORECAST_BASE_URL: &str = "https://api.open-meteo.com";
const OPEN_METEO_GEOCODING_BASE_URL: &str = "https://geocoding-api.open-meteo.com";

const HTTP_TIMEOUT: Duration = Duration::from_millis(2_500);
const MAX_CACHE_ENTRIES: usize = 512;
const GEO_HIT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const WEATHER_HIT_TTL: Duration = Duration::from_secs(30 * 60);
const NEGATIVE_TTL: Duration = Duration::from_secs(5 * 60);

pub(crate) const MAX_WEATHER_CITY_CHARS: usize = 100;

static WEATHER_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(HTTP_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .user_agent("Soulbeet/1.0")
        .build()
        .expect("failed to build weather HTTP client")
});

#[derive(Debug, Clone)]
struct CacheEntry<T> {
    value: T,
    inserted_at: Instant,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WeatherLocation {
    pub lat: f64,
    pub lon: f64,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct RawWeather {
    temperature_c: f32,
    weather_code: u16,
    is_day: bool,
}

type BoundedCache<K, V> = RwLock<HashMap<K, CacheEntry<V>>>;

static GEO_CACHE: LazyLock<BoundedCache<Option<IpAddr>, Option<WeatherLocation>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

static WEATHER_CACHE: LazyLock<BoundedCache<(i32, i32), Option<RawWeather>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn cache_get<K, V>(cache: &BoundedCache<K, V>, key: &K) -> Option<V>
where
    K: Eq + std::hash::Hash,
    V: Clone,
{
    let now = Instant::now();
    let guard = cache.read().ok()?;
    let entry = guard.get(key)?;
    if now < entry.expires_at {
        Some(entry.value.clone())
    } else {
        None
    }
}

fn cache_insert<K, V>(cache: &BoundedCache<K, V>, key: K, value: V, ttl: Duration)
where
    K: Eq + std::hash::Hash + Clone,
{
    let now = Instant::now();
    let Ok(mut guard) = cache.write() else {
        return;
    };
    if !guard.contains_key(&key) && guard.len() >= MAX_CACHE_ENTRIES {
        guard.retain(|_, entry| now < entry.expires_at);
        while guard.len() >= MAX_CACHE_ENTRIES {
            let oldest_key = guard
                .iter()
                .min_by_key(|(_, entry)| entry.inserted_at)
                .map(|(k, _)| k.clone());
            match oldest_key {
                Some(k) => {
                    guard.remove(&k);
                }
                None => break,
            }
        }
    }
    guard.insert(
        key,
        CacheEntry {
            value,
            inserted_at: now,
            expires_at: now + ttl,
        },
    );
}

fn valid_coords(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)
}

/// Validates optional client-supplied coordinates. Both `lat` and `lon` must either be omitted
/// together or be finite WGS84 coordinates in `[-90, 90]` and `[-180, 180]`.
pub(crate) fn validate_client_coords(
    lat: Option<f64>,
    lon: Option<f64>,
) -> Result<Option<(f64, f64)>, Failure> {
    match (lat, lon) {
        (None, None) => Ok(None),
        (Some(la), Some(lo)) if valid_coords(la, lo) => Ok(Some((la, lo))),
        _ => Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "lat and lon must both be finite coordinates in [-90, 90] and [-180, 180]",
        )),
    }
}

fn format_location_label(city: Option<&str>, country_code: Option<&str>) -> Option<String> {
    let city = city.map(str::trim).filter(|s| !s.is_empty());
    let country = country_code
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_uppercase);
    match (city, country) {
        (Some(c), Some(cc)) => Some(format!("{c}, {cc}")),
        (Some(c), None) => Some(c.to_string()),
        (None, Some(cc)) => Some(cc),
        (None, None) => None,
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let is_cgnat = a == 100 && (64..=127).contains(&b);
    let is_this_network = a == 0;
    let is_benchmarking = a == 198 && (b == 18 || b == 19);
    let is_reserved = a >= 240;
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_cgnat
        || is_this_network
        || is_benchmarking
        || is_reserved)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(v4);
    }
    let segs = ip.segments();
    let is_unique_local = (segs[0] & 0xfe00) == 0xfc00;
    let is_link_local = (segs[0] & 0xffc0) == 0xfe80;
    let is_documentation = segs[0] == 0x2001 && segs[1] == 0x0db8;
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_unique_local
        || is_link_local
        || is_documentation)
}

/// Returns `true` only for globally routable unicast IP addresses, rejecting loopback,
/// RFC1918 private, link-local, CGNAT, broadcast, documentation, unspecified, multicast,
/// IPv6 unique-local (`fc00::/7`), and IPv4-mapped private addresses.
pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

fn header_public_ip(headers: &HeaderMap, name: &str, rightmost_csv: bool) -> Option<IpAddr> {
    if rightmost_csv {
        // Reverse proxies append the peer IP on the right of X-Forwarded-For, so reading from
        // the rightmost header/entry prevents clients from spoofing arbitrary IPs by prepending.
        for val in headers.get_all(name).iter().rev() {
            let Ok(raw) = val.to_str() else {
                continue;
            };
            for part in raw.rsplit(',') {
                if let Ok(ip) = IpAddr::from_str(part.trim()) {
                    let ip = normalize_ip(ip);
                    if is_public_ip(ip) {
                        return Some(ip);
                    }
                }
            }
        }
        return None;
    }

    for val in headers.get_all(name) {
        let Ok(raw) = val.to_str() else {
            continue;
        };
        if let Ok(ip) = IpAddr::from_str(raw.trim()) {
            let ip = normalize_ip(ip);
            if is_public_ip(ip) {
                return Some(ip);
            }
        }
    }
    None
}

/// Extracts the caller's public IP address from standard reverse-proxy headers:
/// `x-real-ip` (set by nginx/NPM), then the rightmost valid public IP in `x-forwarded-for`
/// (appended by the trusted reverse proxy), then `cf-connecting-ip`. Strictly parses via
/// `IpAddr::from_str` and normalizes IPv4-mapped IPv6 addresses.
pub(crate) fn extract_public_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    if let Some(ip) = header_public_ip(headers, "x-real-ip", false) {
        return Some(ip);
    }
    if let Some(ip) = header_public_ip(headers, "x-forwarded-for", true) {
        return Some(ip);
    }
    header_public_ip(headers, "cf-connecting-ip", false)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrFloat {
    Float(f64),
    String(String),
}

impl StringOrFloat {
    fn as_f64(&self) -> Option<f64> {
        match self {
            StringOrFloat::Float(v) => Some(*v),
            StringOrFloat::String(s) => s.trim().parse::<f64>().ok(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GeoJsResponse {
    latitude: Option<StringOrFloat>,
    longitude: Option<StringOrFloat>,
    city: Option<String>,
    country_code: Option<String>,
}

async fn fetch_geo_location_from(
    client: &reqwest::Client,
    geo_base: &str,
    client_ip: Option<IpAddr>,
) -> Option<WeatherLocation> {
    let base = geo_base.trim_end_matches('/');
    let url = match client_ip {
        Some(ip) => format!("{base}/v1/ip/geo/{ip}.json"),
        None => format!("{base}/v1/ip/geo.json"),
    };
    let resp = match client.get(&url).timeout(HTTP_TIMEOUT).send().await {
        Ok(r) => r,
        Err(e) => {
            debug!("GeoJS lookup failed for {client_ip:?}: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        debug!(
            "GeoJS lookup returned HTTP {} for {client_ip:?}",
            resp.status()
        );
        return None;
    }
    let body: GeoJsResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            debug!("GeoJS JSON parse failed for {client_ip:?}: {e}");
            return None;
        }
    };
    let lat = body.latitude.as_ref().and_then(StringOrFloat::as_f64)?;
    let lon = body.longitude.as_ref().and_then(StringOrFloat::as_f64)?;
    if !valid_coords(lat, lon) {
        return None;
    }
    let label = format_location_label(body.city.as_deref(), body.country_code.as_deref());
    Some(WeatherLocation { lat, lon, label })
}

async fn cached_geo_location(client_ip: Option<IpAddr>) -> Option<WeatherLocation> {
    if let Some(cached) = cache_get(&GEO_CACHE, &client_ip) {
        return cached;
    }
    let fetched = fetch_geo_location_from(&WEATHER_HTTP_CLIENT, GEOJS_BASE_URL, client_ip).await;
    let ttl = if fetched.is_some() {
        GEO_HIT_TTL
    } else {
        NEGATIVE_TTL
    };
    cache_insert(&GEO_CACHE, client_ip, fetched.clone(), ttl);
    fetched
}

#[derive(Debug, Deserialize)]
struct OpenMeteoForecastResponse {
    current: Option<OpenMeteoCurrent>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoCurrent {
    temperature_2m: f32,
    weather_code: u16,
    is_day: i32,
}

async fn fetch_raw_weather_from(
    client: &reqwest::Client,
    forecast_base: &str,
    lat_tenths: i32,
    lon_tenths: i32,
) -> Option<RawWeather> {
    let lat = f64::from(lat_tenths) / 10.0;
    let lon = f64::from(lon_tenths) / 10.0;
    let url = format!("{}/v1/forecast", forecast_base.trim_end_matches('/'));
    let resp = match client
        .get(&url)
        .query(&[
            ("latitude", format!("{lat:.1}")),
            ("longitude", format!("{lon:.1}")),
            ("current", "temperature_2m,weather_code,is_day".to_string()),
        ])
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!("Open-Meteo forecast failed for ({lat:.1}, {lon:.1}): {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        debug!(
            "Open-Meteo forecast returned HTTP {} for ({lat:.1}, {lon:.1})",
            resp.status()
        );
        return None;
    }
    let body: OpenMeteoForecastResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            debug!("Open-Meteo forecast JSON parse failed: {e}");
            return None;
        }
    };
    let current = body.current?;
    if !current.temperature_2m.is_finite() {
        return None;
    }
    Some(RawWeather {
        temperature_c: current.temperature_2m,
        weather_code: current.weather_code,
        is_day: current.is_day != 0,
    })
}

async fn cached_raw_weather(lat_tenths: i32, lon_tenths: i32) -> Option<RawWeather> {
    let key = (lat_tenths, lon_tenths);
    if let Some(cached) = cache_get(&WEATHER_CACHE, &key) {
        return cached;
    }
    let fetched = fetch_raw_weather_from(
        &WEATHER_HTTP_CLIENT,
        OPEN_METEO_FORECAST_BASE_URL,
        lat_tenths,
        lon_tenths,
    )
    .await;
    let ttl = if fetched.is_some() {
        WEATHER_HIT_TTL
    } else {
        NEGATIVE_TTL
    };
    cache_insert(&WEATHER_CACHE, key, fetched, ttl);
    fetched
}

fn round_coords_tenths(lat: f64, lon: f64) -> (i32, i32) {
    (
        (lat * 10.0).round() as i32,
        (lon * 10.0).round() as i32,
    )
}

fn saved_profile_location(profile: &UserCultureProfile) -> Option<WeatherLocation> {
    let (Some(lat), Some(lon)) = (profile.weather_lat, profile.weather_lon) else {
        return None;
    };
    if !valid_coords(lat, lon) {
        return None;
    }
    let label = profile
        .weather_city
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(WeatherLocation { lat, lon, label })
}

fn snapshot_from_raw(raw: RawWeather, label: Option<String>) -> Option<WeatherSnapshot> {
    let condition = classify_wmo_weather(raw.weather_code, raw.temperature_c, raw.is_day)?;
    Some(WeatherSnapshot {
        condition,
        temperature_c: raw.temperature_c,
        weather_code: raw.weather_code,
        is_day: raw.is_day,
        location_label: label,
    })
}

/// Resolves live local weather for `profile` (using opted-in live `client_coords` first, then
/// saved city coordinates, then cached GeoJS IP geolocation) and classifies it via
/// `classify_wmo_weather`. Enforces a strict combined `HTTP_TIMEOUT` (2.5s) ceiling across both
/// hops and returns `None` cleanly if weather is neutral or upstream services are slow/unreachable.
pub(crate) async fn resolve_weather_for_profile(
    profile: &UserCultureProfile,
    client_coords: Option<(f64, f64)>,
    client_ip: Option<IpAddr>,
) -> Option<WeatherSnapshot> {
    tokio::time::timeout(HTTP_TIMEOUT, async move {
        let loc = if let Some((lat, lon)) = client_coords.filter(|&(la, lo)| valid_coords(la, lo)) {
            WeatherLocation {
                lat,
                lon,
                label: None,
            }
        } else if let Some(saved) = saved_profile_location(profile) {
            saved
        } else {
            cached_geo_location(client_ip).await?
        };
        let (lat_tenths, lon_tenths) = round_coords_tenths(loc.lat, loc.lon);
        let raw = cached_raw_weather(lat_tenths, lon_tenths).await?;
        snapshot_from_raw(raw, loc.label)
    })
    .await
    .ok()
    .flatten()
}

/// Uncached variant of [`resolve_weather_for_profile`] with injectable upstream base URLs
/// for deterministic offline testing.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn resolve_weather_with_bases(
    profile: &UserCultureProfile,
    client_coords: Option<(f64, f64)>,
    client_ip: Option<IpAddr>,
    geo_base: &str,
    forecast_base: &str,
) -> Option<WeatherSnapshot> {
    let loc = if let Some((lat, lon)) = client_coords.filter(|&(la, lo)| valid_coords(la, lo)) {
        WeatherLocation {
            lat,
            lon,
            label: None,
        }
    } else if let Some(saved) = saved_profile_location(profile) {
        saved
    } else {
        fetch_geo_location_from(&WEATHER_HTTP_CLIENT, geo_base, client_ip).await?
    };
    let (lat_tenths, lon_tenths) = round_coords_tenths(loc.lat, loc.lon);
    let raw =
        fetch_raw_weather_from(&WEATHER_HTTP_CLIENT, forecast_base, lat_tenths, lon_tenths).await?;
    snapshot_from_raw(raw, loc.label)
}

#[derive(Debug, Deserialize)]
struct OpenMeteoGeocodingResponse {
    #[serde(default)]
    results: Vec<OpenMeteoGeocodingResult>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoGeocodingResult {
    name: String,
    latitude: f64,
    longitude: f64,
    country_code: Option<String>,
}

/// Geocodes a city query via Open-Meteo's geocoding API, preferring a result matching
/// `preferred_country` (case-insensitive) when provided.
pub(crate) async fn geocode_city(
    query: &str,
    preferred_country: Option<&str>,
) -> Result<WeatherLocation, Failure> {
    geocode_city_with_base(query, preferred_country, OPEN_METEO_GEOCODING_BASE_URL).await
}

/// Splits an optional trailing `, <CC>` (2-letter ISO country code) from a city query such as
/// `"Lyon, FR"` so Open-Meteo's `name` search receives `"Lyon"` while `"FR"` guides country
/// disambiguation.
fn split_city_and_country(trimmed: &str) -> (&str, Option<&str>) {
    if let Some((city, suffix)) = trimmed.rsplit_once(',') {
        let city = city.trim();
        let cc = suffix.trim();
        if !city.is_empty() && cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_alphabetic()) {
            return (city, Some(cc));
        }
    }
    (trimmed, None)
}

/// Variant of [`geocode_city`] with an injectable geocoding base URL for offline tests.
pub(crate) async fn geocode_city_with_base(
    query: &str,
    preferred_country: Option<&str>,
    geocoding_base: &str,
) -> Result<WeatherLocation, Failure> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            "City name cannot be empty",
        ));
    }
    if trimmed.chars().count() > MAX_WEATHER_CITY_CHARS {
        return Err(Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NotRetryable,
            format!("City name must be at most {MAX_WEATHER_CITY_CHARS} characters"),
        ));
    }

    let (search_name, query_country) = split_city_and_country(trimmed);
    let effective_country = query_country.or_else(|| {
        preferred_country
            .map(str::trim)
            .filter(|s| !s.is_empty())
    });

    let url = format!("{}/v1/search", geocoding_base.trim_end_matches('/'));
    let resp = WEATHER_HTTP_CLIENT
        .get(&url)
        .query(&[
            ("name", search_name),
            ("count", "5"),
            ("language", "en"),
            ("format", "json"),
        ])
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            Failure::new(
                FailureCode::MetadataUnavailable,
                Stage::Input,
                RetryAdvice::SameSourceLater,
                format!("Could not reach geocoding service for '{trimmed}': {e}"),
            )
        })?;

    if !resp.status().is_success() {
        return Err(Failure::new(
            FailureCode::MetadataUnavailable,
            Stage::Input,
            RetryAdvice::SameSourceLater,
            format!(
                "Geocoding service returned HTTP {} for '{trimmed}'",
                resp.status()
            ),
        ));
    }

    let body: OpenMeteoGeocodingResponse = resp.json().await.map_err(|e| {
        Failure::new(
            FailureCode::MetadataUnavailable,
            Stage::Input,
            RetryAdvice::SameSourceLater,
            format!("Could not parse geocoding response for '{trimmed}': {e}"),
        )
    })?;

    let valid_results: Vec<OpenMeteoGeocodingResult> = body
        .results
        .into_iter()
        .filter(|r| !r.name.trim().is_empty() && valid_coords(r.latitude, r.longitude))
        .collect();

    if valid_results.is_empty() {
        return Err(Failure::new(
            FailureCode::MetadataNotFound,
            Stage::Input,
            RetryAdvice::OtherQuery,
            format!("Could not find a city matching '{trimmed}'"),
        ));
    }

    let chosen = effective_country
        .and_then(|cc| {
            valid_results.iter().find(|r| {
                r.country_code
                    .as_deref()
                    .is_some_and(|code| code.eq_ignore_ascii_case(cc))
            })
        })
        .unwrap_or(&valid_results[0]);

    let label = format_location_label(Some(&chosen.name), chosen.country_code.as_deref());
    Ok(WeatherLocation {
        lat: chosen.latitude,
        lon: chosen.longitude,
        label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};
    use shared::recommendation::WeatherCondition;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                HeaderName::from_str(k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    struct StubHttpServer {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl StubHttpServer {
        async fn start(status: &'static str, body: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let reqs_clone = requests.clone();

            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = [0u8; 4096];
                    if let Ok(n) = stream.read(&mut buf).await {
                        let req_text = String::from_utf8_lossy(&buf[..n]);
                        if let Some(first_line) = req_text.lines().next() {
                            let target = first_line
                                .split_whitespace()
                                .nth(1)
                                .unwrap_or("")
                                .to_string();
                            reqs_clone.lock().unwrap().push(target);
                        }
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    }
                }
            });

            Self {
                base_url: format!("http://{addr}"),
                requests,
            }
        }

        fn recorded_requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[test]
    fn extract_public_client_ip_precedence_and_sanitization() {
        // X-Real-IP (set by nginx/NPM) wins over spoofable client headers
        let h = headers(&[
            ("cf-connecting-ip", "82.64.12.34"),
            ("x-forwarded-for", "1.1.1.1, 8.8.8.8"),
            ("x-real-ip", "9.9.9.9"),
        ]);
        assert_eq!(
            extract_public_client_ip(&h),
            Some(IpAddr::from_str("9.9.9.9").unwrap())
        );

        // X-Forwarded-For picks rightmost public IP (appended by reverse proxy, skipping private hops)
        let h = headers(&[(
            "x-forwarded-for",
            "1.1.1.1, 82.64.12.34, 192.168.1.5, 10.0.0.2, 100.64.1.2",
        )]);
        assert_eq!(
            extract_public_client_ip(&h),
            Some(IpAddr::from_str("82.64.12.34").unwrap())
        );

        // Normalizes IPv4-mapped IPv6 public addresses to IPv4 so caches stay unified
        let h = headers(&[("x-real-ip", "::ffff:8.8.8.8")]);
        assert_eq!(
            extract_public_client_ip(&h),
            Some(IpAddr::from_str("8.8.8.8").unwrap())
        );

        // Falls back to CF-Connecting-IP when X-Real-IP and XFF only hold private or invalid values
        let h = headers(&[
            ("x-real-ip", "../../etc/passwd"),
            ("x-forwarded-for", "127.0.0.1, 172.16.0.4, 8.8.8.8/evil"),
            ("cf-connecting-ip", "2606:4700:4700::1111"),
        ]);
        assert_eq!(
            extract_public_client_ip(&h),
            Some(IpAddr::from_str("2606:4700:4700::1111").unwrap())
        );

        // Rejects all non-public, malformed, and path-injection values
        for rejected in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "0.0.0.0",
            "255.255.255.255",
            "192.0.2.1",
            "198.51.100.5",
            "203.0.113.9",
            "224.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.1.10",
            "8.8.8.8/../geo",
            "8.8.8.8?foo=bar",
            "not-an-ip",
            "",
        ] {
            let h = headers(&[("x-real-ip", rejected)]);
            assert_eq!(
                extract_public_client_ip(&h),
                None,
                "expected '{rejected}' to be rejected"
            );
        }

        // Just outside CGNAT (100.128.0.1) is public
        let h = headers(&[("x-real-ip", "100.128.0.1")]);
        assert_eq!(
            extract_public_client_ip(&h),
            Some(IpAddr::from_str("100.128.0.1").unwrap())
        );
    }

    #[test]
    fn classify_wmo_weather_maps_conditions_and_neutral_weather() {
        assert_eq!(classify_wmo_weather(51, 14.0, true), Some(WeatherCondition::Rainy));
        assert_eq!(classify_wmo_weather(63, 10.0, false), Some(WeatherCondition::Rainy));
        assert_eq!(classify_wmo_weather(80, 18.0, true), Some(WeatherCondition::Rainy));

        assert_eq!(classify_wmo_weather(71, -2.0, true), Some(WeatherCondition::Snowy));
        assert_eq!(classify_wmo_weather(75, -5.0, false), Some(WeatherCondition::Snowy));
        assert_eq!(classify_wmo_weather(86, 0.0, true), Some(WeatherCondition::Snowy));

        assert_eq!(classify_wmo_weather(95, 19.0, false), Some(WeatherCondition::Stormy));
        assert_eq!(classify_wmo_weather(99, 22.0, true), Some(WeatherCondition::Stormy));

        assert_eq!(classify_wmo_weather(0, 20.0, true), Some(WeatherCondition::Sunny));
        assert_eq!(classify_wmo_weather(1, 27.5, true), Some(WeatherCondition::Sunny));

        // Neutral conditions must return None
        assert_eq!(classify_wmo_weather(0, 19.9, true), None); // cool clear day
        assert_eq!(classify_wmo_weather(0, 25.0, false), None); // warm clear night
        assert_eq!(classify_wmo_weather(2, 24.0, true), None); // partly cloudy
        assert_eq!(classify_wmo_weather(3, 22.0, true), None); // overcast
        assert_eq!(classify_wmo_weather(45, 12.0, true), None); // fog
    }

    #[test]
    fn validate_client_coords_checks_pairing_finiteness_and_bounds() {
        assert_eq!(validate_client_coords(None, None).unwrap(), None);
        assert_eq!(
            validate_client_coords(Some(45.75), Some(4.85)).unwrap(),
            Some((45.75, 4.85))
        );
        assert_eq!(
            validate_client_coords(Some(-90.0), Some(-180.0)).unwrap(),
            Some((-90.0, -180.0))
        );
        assert_eq!(
            validate_client_coords(Some(90.0), Some(180.0)).unwrap(),
            Some((90.0, 180.0))
        );

        for (lat, lon) in [
            (Some(45.0), None),
            (None, Some(4.0)),
            (Some(f64::NAN), Some(4.0)),
            (Some(45.0), Some(f64::NAN)),
            (Some(f64::INFINITY), Some(4.0)),
            (Some(45.0), Some(f64::NEG_INFINITY)),
            (Some(90.01), Some(0.0)),
            (Some(-90.01), Some(0.0)),
            (Some(0.0), Some(180.01)),
            (Some(0.0), Some(-180.01)),
        ] {
            let err = validate_client_coords(lat, lon)
                .expect_err("invalid client coords should return Failure");
            assert_eq!(err.code, FailureCode::InvalidInput);
            assert_eq!(err.stage, Stage::Input);
            assert_eq!(err.retry, RetryAdvice::NotRetryable);
        }
    }

    #[tokio::test]
    async fn offline_geojs_string_lat_lon_and_open_meteo_resolution() {
        let geo = StubHttpServer::start(
            "200 OK",
            r#"{"latitude":"48.8582","longitude":"2.3387","city":"Paris","country_code":"FR"}"#,
        )
        .await;
        let forecast = StubHttpServer::start(
            "200 OK",
            r#"{"current":{"temperature_2m":12.4,"weather_code":61,"is_day":1}}"#,
        )
        .await;

        let profile = UserCultureProfile::default();
        let ip = IpAddr::from_str("82.64.12.34").unwrap();
        let snap = resolve_weather_with_bases(
            &profile,
            None,
            Some(ip),
            &geo.base_url,
            &forecast.base_url,
        )
        .await
        .expect("rainy snapshot should resolve");

        assert_eq!(snap.condition, WeatherCondition::Rainy);
        assert!((snap.temperature_c - 12.4).abs() < 0.01);
        assert_eq!(snap.weather_code, 61);
        assert!(snap.is_day);
        assert_eq!(snap.location_label.as_deref(), Some("Paris, FR"));

        assert_eq!(geo.recorded_requests(), vec!["/v1/ip/geo/82.64.12.34.json"]);
        let f_reqs = forecast.recorded_requests();
        assert_eq!(f_reqs.len(), 1);
        assert!(
            f_reqs[0].contains("latitude=48.9") && f_reqs[0].contains("longitude=2.3"),
            "coordinates must be rounded to 0.1 deg; got {}",
            f_reqs[0]
        );

        // When client_ip is None, falls back to server egress endpoint `/v1/ip/geo.json`
        let geo_num = StubHttpServer::start(
            "200 OK",
            r#"{"latitude":45.75,"longitude":4.85,"city":"Lyon","country_code":"fr"}"#,
        )
        .await;
        let snap_egress = resolve_weather_with_bases(
            &profile,
            None,
            None,
            &geo_num.base_url,
            &forecast.base_url,
        )
        .await
        .unwrap();
        assert_eq!(geo_num.recorded_requests(), vec!["/v1/ip/geo.json"]);
        assert_eq!(snap_egress.location_label.as_deref(), Some("Lyon, FR"));

        // Priority 2: saved weather_lat/weather_lon skips GeoJS completely when no live client coords
        let saved_profile = UserCultureProfile {
            weather_city: Some("Chamonix, FR".into()),
            weather_lat: Some(45.9237),
            weather_lon: Some(6.8694),
            ..Default::default()
        };
        let geo_unused = StubHttpServer::start("500 Internal Server Error", "{}").await;
        let snow_forecast = StubHttpServer::start(
            "200 OK",
            r#"{"current":{"temperature_2m":-3.5,"weather_code":73,"is_day":0}}"#,
        )
        .await;
        let snap_saved = resolve_weather_with_bases(
            &saved_profile,
            None,
            Some(ip),
            &geo_unused.base_url,
            &snow_forecast.base_url,
        )
        .await
        .unwrap();
        assert!(
            geo_unused.recorded_requests().is_empty(),
            "saved coordinates must not call GeoJS"
        );
        assert_eq!(snap_saved.condition, WeatherCondition::Snowy);
        assert!(!snap_saved.is_day);
        assert_eq!(snap_saved.location_label.as_deref(), Some("Chamonix, FR"));

        // Priority 1: live client_coords overrides saved city and bypasses GeoJS, rounding to 0.1 deg
        let live_forecast = StubHttpServer::start(
            "200 OK",
            r#"{"current":{"temperature_2m":26.2,"weather_code":0,"is_day":1}}"#,
        )
        .await;
        let snap_live = resolve_weather_with_bases(
            &saved_profile,
            Some((43.2965, 5.3698)),
            Some(ip),
            &geo_unused.base_url,
            &live_forecast.base_url,
        )
        .await
        .unwrap();
        assert!(
            geo_unused.recorded_requests().is_empty(),
            "live client coordinates must not call GeoJS"
        );
        assert_eq!(snap_live.condition, WeatherCondition::Sunny);
        assert!(snap_live.is_day);
        assert_eq!(snap_live.location_label, None);
        let live_reqs = live_forecast.recorded_requests();
        assert_eq!(live_reqs.len(), 1);
        assert!(
            live_reqs[0].contains("latitude=43.3") && live_reqs[0].contains("longitude=5.4"),
            "live client coordinates must override saved city and round to 0.1 deg; got {}",
            live_reqs[0]
        );

        // Upstream errors degrade cleanly to None
        let broken = StubHttpServer::start("503 Service Unavailable", "").await;
        assert!(resolve_weather_with_bases(
            &saved_profile,
            None,
            None,
            &geo.base_url,
            &broken.base_url
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn offline_open_meteo_geocoding_prefers_country_and_url_encodes_query() {
        let geocoder = StubHttpServer::start(
            "200 OK",
            r#"{"results":[
                {"name":"Paris","latitude":33.6609,"longitude":-95.5555,"country_code":"US"},
                {"name":"Paris","latitude":48.8534,"longitude":2.3488,"country_code":"FR"}
            ]}"#,
        )
        .await;

        let fr = geocode_city_with_base("Paris", Some("fr"), &geocoder.base_url)
            .await
            .unwrap();
        assert_eq!(fr.label.as_deref(), Some("Paris, FR"));
        assert!((fr.lat - 48.8534).abs() < 0.001);

        // "City, CC" input (such as pre-filled saved Taste DNA labels) must strip ", FR" before
        // querying Open-Meteo `name=` and use "FR" for country disambiguation.
        let comma_fr = geocode_city_with_base("Paris, FR", None, &geocoder.base_url)
            .await
            .unwrap();
        assert_eq!(comma_fr.label.as_deref(), Some("Paris, FR"));
        assert!((comma_fr.lat - 48.8534).abs() < 0.001);
        let comma_req = geocoder.recorded_requests()[1].clone();
        assert!(
            comma_req.contains("name=Paris&"),
            "comma country suffix must be stripped from Open-Meteo name param, got {comma_req}"
        );

        let default_first = geocode_city_with_base("Saint-Étienne & Lyon", None, &geocoder.base_url)
            .await
            .unwrap();
        assert_eq!(default_first.label.as_deref(), Some("Paris, US"));
        let reqs = geocoder.recorded_requests();
        assert!(
            reqs[2].contains("Saint-%C3%89tienne+%26+Lyon")
                || reqs[2].contains("Saint-%C3%89tienne%20%26%20Lyon"),
            "query must be URL-encoded, got {}",
            reqs[2]
        );

        let empty_geocoder = StubHttpServer::start("200 OK", r#"{}"#).await;
        let missing = geocode_city_with_base("Atlantis", None, &empty_geocoder.base_url)
            .await
            .unwrap_err();
        assert_eq!(missing.code, FailureCode::MetadataNotFound);

        let blank = geocode_city_with_base("   ", None, &empty_geocoder.base_url)
            .await
            .unwrap_err();
        assert_eq!(blank.code, FailureCode::InvalidInput);
    }
}
