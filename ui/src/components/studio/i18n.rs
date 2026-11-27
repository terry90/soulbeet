//! Client-side localization & formatting for Discovery 2.0 `i18n_key`s, `CandidateReason`s,
//! and OpenSubsonic media URLs derived from [`NavidromeSessionInfo`].

use shared::client::NavidromeSessionInfo;
use shared::recommendation::{
    canonical_mix_title, humanize_mix_slug, CandidateReason, ContextualMixKind,
    ContextualMixSummary,
};

/// Formats a human-readable title for a [`ContextualMixSummary`].
/// Uses canonical neutral titles (no country-specific title overrides or country suffixes);
/// regional blend ratios are surfaced via badges and steering controls instead.
pub fn format_mix_title(summary: &ContextualMixSummary) -> String {
    canonical_mix_title(
        &summary.slug,
        Some(&summary.i18n_key),
        summary.custom_name.as_deref(),
    )
}

/// Formats a descriptive subtitle for a [`ContextualMixSummary`].
pub fn format_mix_subtitle(summary: &ContextualMixSummary) -> String {
    if let Some(custom) = summary
        .custom_description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return custom.to_string();
    }

    match summary.i18n_key.as_str() {
        "mix.seasonal.winter_holiday" => {
            "Christmas favorites and festive classics, blended to your taste.".to_string()
        }
        "mix.seasonal.new_years_eve" => {
            "High-energy dance floor staples and regional party picks to close the year.".to_string()
        }
        "mix.seasonal.cozy_winter" => {
            "Fireside folk, gentle ambient, and acoustic warmth for cold winter days.".to_string()
        }
        "mix.seasonal.lunar_new_year" => {
            "Celebratory regional pop and fresh festive picks for the new lunar cycle.".to_string()
        }
        "mix.seasonal.valentines" => {
            "Smooth soul, jazz, R&B, and romantic classics set for an intimate evening.".to_string()
        }
        "mix.seasonal.carnival" => {
            "Percussive rhythms, samba, funk, and vibrant carnival energy.".to_string()
        }
        "mix.seasonal.celtic_spring" => {
            "Spirited Irish trad, Celtic folk, and lively acoustic pub anthems.".to_string()
        }
        "mix.seasonal.spring" => {
            "Fresh indie, acoustic warmth, and bright new-season discoveries.".to_string()
        }
        "mix.seasonal.summer" => {
            "Sun-soaked grooves, upbeat pop, and warm-weather classics.".to_string()
        }
        "mix.seasonal.summer_solstice" => {
            "Golden-hour indie, festival electro, and long-evening summer sounds.".to_string()
        }
        "mix.seasonal.late_summer_reset" => {
            "Hazy dream pop, indie grooves, and bittersweet September transitions.".to_string()
        }
        "mix.seasonal.autumn" => {
            "Cozy acoustic textures, folk, jazz, and mellow indie cuts.".to_string()
        }
        "mix.seasonal.halloween" => {
            "Atmospheric darkwave, post-punk, and nocturnal synth textures.".to_string()
        }
        "mix.moment.drive" => {
            "Offline-prioritized favorites and upbeat momentum built for the road.".to_string()
        }
        "mix.moment.morning" => {
            "Bright grooves and familiar lifts to ease into the day.".to_string()
        }
        "mix.moment.focus" => {
            "Steady ambient, instrumental, and downtempo cuts for uninterrupted flow.".to_string()
        }
        "mix.moment.evening_unwind" => {
            "Warm soul, acoustic folk, and mellow cuts to ease out of the day.".to_string()
        }
        "mix.moment.dinner_groove" => {
            "Feel-good funk, bossa nova, and nu-disco grooves for cooking and company.".to_string()
        }
        "mix.moment.late_night" => {
            "Low-lit dream pop, trip hop, and midnight library favorites.".to_string()
        }
        "mix.moment.rainy_day" => {
            "Soft acoustic folk, piano, and reflective melodies made for gray skies.".to_string()
        }
        "mix.moment.friday_night" => {
            "Upbeat electro, disco, and indie energy to kick off the weekend.".to_string()
        }
        "mix.moment.sunday_morning" => {
            "Unrushed jazz, warm soul, and acoustic favorites for a slow start.".to_string()
        }
        "mix.moment.forgotten_gems" => {
            "Top-rated favorites from your library that you have not spun in months.".to_string()
        }
        "mix.moment.fresh_arrivals" => {
            "Recently added tracks in your library waiting for their first proper listen."
                .to_string()
        }
        "mix.moment.decade_80s" => {
            "Your library's finest 1980s synth, new wave, and classic cuts.".to_string()
        }
        "mix.moment.decade_90s" => {
            "Standout 1990s records and golden-era favorites from your collection.".to_string()
        }
        "mix.moment.decade_2000s" => {
            "Essential 2000s indie, electronic, and throwback tracks from your library.".to_string()
        }
        "mix.rolling.conservative" | "mix.rolling.comfort" => {
            "High-rotation favorites and trusted genres from your library.".to_string()
        }
        "mix.rolling.balanced" | "mix.rolling.weekly_radar" => {
            "Fresh recommendations tuned to your recent listening momentum.".to_string()
        }
        "mix.rolling.adventurous" | "mix.rolling.deep_cuts" => {
            "Hidden gems and adventurous tracks beyond your usual rotation.".to_string()
        }
        _ => match summary.kind {
            ContextualMixKind::Seasonal => {
                "Seasonal discovery blend isolated from your core taste profile.".to_string()
            }
            ContextualMixKind::Moment => {
                format!(
                    "{}% library favorites blended with fresh contextual picks.",
                    summary.owned_pct
                )
            }
            ContextualMixKind::Rolling => {
                "Continuously updated mix shaped by your listening history.".to_string()
            }
        },
    }
}

/// Formats the compact schedule/window badge shown atop [`crate::SeasonalHeroCard`].
pub fn format_mix_window_badge(summary: &ContextualMixSummary) -> Option<String> {
    match (&summary.active_from, &summary.active_until) {
        (Some(from), Some(until)) => {
            let from_lbl = format_short_iso_date(from);
            let until_lbl = format_short_iso_date(until);
            if let Some(exp) = summary.expires_at.as_deref() {
                let exp_lbl = format_short_iso_date(exp);
                Some(format!("{from_lbl} – {until_lbl} · AUTO-CLEANS {exp_lbl}"))
            } else {
                Some(format!("{from_lbl} – {until_lbl}"))
            }
        }
        _ if summary.offline_priority => Some("OFFLINE PRIORITY".to_string()),
        _ => match summary.kind {
            ContextualMixKind::Seasonal => Some("SEASONAL PACK".to_string()),
            ContextualMixKind::Moment => Some("MOMENT MIX".to_string()),
            ContextualMixKind::Rolling => Some("ROLLING MIX".to_string()),
        },
    }
}

/// Formats a structured [`CandidateReason`] into concise client-side provenance copy.
pub fn format_candidate_reason(reason: &CandidateReason) -> String {
    match reason {
        CandidateReason::RegionalContextual {
            mix_slug,
            country_code,
        } => {
            let mix = humanize_mix_slug(mix_slug);
            match country_code
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(cc) => format!("{} local pick · {mix}", cc.to_ascii_uppercase()),
                None => format!("Local pick · {mix}"),
            }
        }
        CandidateReason::InternationalContextual { mix_slug } => {
            format!("International pick · {}", humanize_mix_slug(mix_slug))
        }
        CandidateReason::TasteWildcard { mix_slug, genre } => {
            format!("{genre} crossover · {}", humanize_mix_slug(mix_slug))
        }
        CandidateReason::MomentMatch {
            mix_slug,
            mood_tag,
            genre,
        } => match (mood_tag.as_deref(), genre.as_deref()) {
            (Some(mood), Some(g)) if !mood.is_empty() && !g.is_empty() => {
                format!("{mood} · {g}")
            }
            (Some(mood), _) if !mood.is_empty() => {
                format!("{mood} · {}", humanize_mix_slug(mix_slug))
            }
            (_, Some(g)) if !g.is_empty() => {
                format!("{g} · {}", humanize_mix_slug(mix_slug))
            }
            _ => format!("Matched {}", humanize_mix_slug(mix_slug)),
        },
        CandidateReason::LibraryMatch {
            mix_slug,
            matched_genre,
        } => match matched_genre.as_deref().filter(|g| !g.is_empty()) {
            Some(g) => format!("In your library · {g}"),
            None => format!("In your library · {}", humanize_mix_slug(mix_slug)),
        },
        CandidateReason::RadioSeed { seed_artist } => {
            format!("Radio seed · {seed_artist}")
        }
        CandidateReason::MoreFromArtist { artist } => {
            format!("More from {artist}")
        }
        CandidateReason::SimilarArtist { seed_artist, .. } => {
            format!("Similar to {seed_artist}")
        }
    }
}

/// Computes remaining days from a Discovery track's `created_at` and configured lifetime.
pub fn discovery_days_remaining(created_at: &str, lifetime_days: u32) -> Option<u32> {
    let parsed = chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(created_at, "%Y-%m-%d %H:%M:%S")
                .map(|naive| naive.and_utc())
        })
        .ok()?;
    let elapsed_days = chrono::Utc::now()
        .signed_duration_since(parsed)
        .num_days()
        .max(0) as u32;
    Some(lifetime_days.saturating_sub(elapsed_days))
}

fn format_short_iso_date(iso: &str) -> String {
    let parts: Vec<&str> = iso.trim().split('-').collect();
    if parts.len() >= 3 {
        let month = match parts[1] {
            "01" => "JAN",
            "02" => "FEB",
            "03" => "MAR",
            "04" => "APR",
            "05" => "MAY",
            "06" => "JUN",
            "07" => "JUL",
            "08" => "AUG",
            "09" => "SEP",
            "10" => "OCT",
            "11" => "NOV",
            "12" => "DEC",
            other => other,
        };
        let day = parts[2]
            .split('T')
            .next()
            .unwrap_or(parts[2])
            .trim_start_matches('0');
        let day = if day.is_empty() { "0" } else { day };
        format!("{month} {day}")
    } else {
        iso.to_ascii_uppercase()
    }
}

/// Percent-encodes a query parameter component per RFC 3986 unreserved set.
pub fn percent_encode_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let is_unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "%{byte:02X}");
        }
    }
    out
}

/// Builds an authenticated OpenSubsonic REST URL (JSON response) from [`NavidromeSessionInfo`].
pub fn subsonic_rest_url(
    info: &NavidromeSessionInfo,
    endpoint: &str,
    extra: &[(&str, &str)],
) -> String {
    let base = info.base_url.trim_end_matches('/');
    let mut url = format!(
        "{base}/rest/{endpoint}?u={}&t={}&s={}&v={}&c={}&f=json",
        percent_encode_param(&info.username),
        percent_encode_param(&info.subsonic_token),
        percent_encode_param(&info.subsonic_salt),
        percent_encode_param(&info.api_version),
        percent_encode_param(&info.client_name),
    );
    for (k, v) in extra {
        url.push('&');
        url.push_str(k);
        url.push('=');
        url.push_str(&percent_encode_param(v));
    }
    url
}

/// Builds a direct OpenSubsonic media stream URL (`rest/stream.view`) for `<audio>` playback.
pub fn subsonic_stream_url(info: &NavidromeSessionInfo, song_id: &str) -> String {
    let base = info.base_url.trim_end_matches('/');
    format!(
        "{base}/rest/stream.view?id={}&u={}&t={}&s={}&v={}&c={}",
        percent_encode_param(song_id),
        percent_encode_param(&info.username),
        percent_encode_param(&info.subsonic_token),
        percent_encode_param(&info.subsonic_salt),
        percent_encode_param(&info.api_version),
        percent_encode_param(&info.client_name),
    )
}

/// Builds a direct OpenSubsonic cover art URL (`rest/getCoverArt.view`).
pub fn subsonic_cover_art_url(info: &NavidromeSessionInfo, cover_id: &str, size: u32) -> String {
    let base = info.base_url.trim_end_matches('/');
    format!(
        "{base}/rest/getCoverArt.view?id={}&size={size}&u={}&t={}&s={}&v={}&c={}",
        percent_encode_param(cover_id),
        percent_encode_param(&info.username),
        percent_encode_param(&info.subsonic_token),
        percent_encode_param(&info.subsonic_salt),
        percent_encode_param(&info.api_version),
        percent_encode_param(&info.client_name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_summary(
        slug: &str,
        kind: ContextualMixKind,
        i18n_key: &str,
        country: Option<&str>,
    ) -> ContextualMixSummary {
        ContextualMixSummary {
            slug: slug.into(),
            kind,
            i18n_key: i18n_key.into(),
            custom_name: None,
            custom_description: None,
            built_in: true,
            country_code: country.map(str::to_string),
            local_pct: Some(45),
            owned_pct: 0,
            offline_priority: false,
            exclude_from_core_taste: true,
            steering: None,
            active_from: Some("2026-11-20".into()),
            active_until: Some("2026-12-27".into()),
            expires_at: Some("2027-01-05".into()),
            navidrome_playlist_id: None,
            track_count: 25,
            target_tracks: 25,
        }
    }

    #[test]
    fn formats_mix_titles_for_builtin_and_custom_mixes() {
        let fr_noel = sample_summary(
            "winter_holiday",
            ContextualMixKind::Seasonal,
            "mix.seasonal.winter_holiday",
            Some("FR"),
        );
        assert_eq!(format_mix_title(&fr_noel), "Christmas Mix");

        let jp_summer = sample_summary(
            "summer",
            ContextualMixKind::Seasonal,
            "mix.seasonal.summer",
            Some("jp"),
        );
        assert_eq!(format_mix_title(&jp_summer), "Summer Anthems");

        let mut custom = sample_summary(
            "late_coding",
            ContextualMixKind::Moment,
            "mix.custom.late_coding",
            None,
        );
        assert_eq!(format_mix_title(&custom), "Late Coding");
        custom.custom_name = Some("Midnight Synth Lab".into());
        assert_eq!(format_mix_title(&custom), "Midnight Synth Lab");

        assert_eq!(
            format_mix_window_badge(&fr_noel).as_deref(),
            Some("NOV 20 – DEC 27 · AUTO-CLEANS JAN 5")
        );
    }

    #[test]
    fn formats_rolling_mix_titles_for_backend_and_legacy_keys() {
        for (key, expected) in [
            ("mix.rolling.conservative", "Comfort Zone"),
            ("mix.rolling.balanced", "Fresh Picks"),
            ("mix.rolling.adventurous", "Deep Cuts"),
            ("mix.rolling.comfort", "Comfort Zone"),
            ("mix.rolling.weekly_radar", "Fresh Picks"),
            ("mix.rolling.deep_cuts", "Deep Cuts"),
        ] {
            let summary = sample_summary("rolling", ContextualMixKind::Rolling, key, None);
            assert_eq!(format_mix_title(&summary), expected, "key={key}");
        }
    }

    #[test]
    fn formats_expanded_seasonal_moment_and_smart_mix_titles_and_subtitles() {
        let cases = [
            (
                "cozy_winter",
                ContextualMixKind::Seasonal,
                "mix.seasonal.cozy_winter",
                "Cozy Winter",
                "Fireside folk, gentle ambient, and acoustic warmth for cold winter days.",
            ),
            (
                "valentines",
                ContextualMixKind::Seasonal,
                "mix.seasonal.valentines",
                "Date Night",
                "Smooth soul, jazz, R&B, and romantic classics set for an intimate evening.",
            ),
            (
                "celtic_spring",
                ContextualMixKind::Seasonal,
                "mix.seasonal.celtic_spring",
                "Celtic Folk",
                "Spirited Irish trad, Celtic folk, and lively acoustic pub anthems.",
            ),
            (
                "summer_solstice",
                ContextualMixKind::Seasonal,
                "mix.seasonal.summer_solstice",
                "Midsummer Solstice",
                "Golden-hour indie, festival electro, and long-evening summer sounds.",
            ),
            (
                "late_summer_reset",
                ContextualMixKind::Seasonal,
                "mix.seasonal.late_summer_reset",
                "Late Summer Reset",
                "Hazy dream pop, indie grooves, and bittersweet September transitions.",
            ),
            (
                "evening_unwind",
                ContextualMixKind::Moment,
                "mix.moment.evening_unwind",
                "Evening Unwind",
                "Warm soul, acoustic folk, and mellow cuts to ease out of the day.",
            ),
            (
                "dinner_groove",
                ContextualMixKind::Moment,
                "mix.moment.dinner_groove",
                "Dinner Groove",
                "Feel-good funk, bossa nova, and nu-disco grooves for cooking and company.",
            ),
            (
                "rainy_day",
                ContextualMixKind::Moment,
                "mix.moment.rainy_day",
                "Rainy Day",
                "Soft acoustic folk, piano, and reflective melodies made for gray skies.",
            ),
            (
                "friday_night",
                ContextualMixKind::Moment,
                "mix.moment.friday_night",
                "Friday Night Warmup",
                "Upbeat electro, disco, and indie energy to kick off the weekend.",
            ),
            (
                "sunday_morning",
                ContextualMixKind::Moment,
                "mix.moment.sunday_morning",
                "Sunday Morning Coffee",
                "Unrushed jazz, warm soul, and acoustic favorites for a slow start.",
            ),
            (
                "forgotten_gems",
                ContextualMixKind::Moment,
                "mix.moment.forgotten_gems",
                "Forgotten Gems",
                "Top-rated favorites from your library that you have not spun in months.",
            ),
            (
                "fresh_arrivals",
                ContextualMixKind::Moment,
                "mix.moment.fresh_arrivals",
                "Fresh Arrivals",
                "Recently added tracks in your library waiting for their first proper listen.",
            ),
            (
                "decade_80s",
                ContextualMixKind::Moment,
                "mix.moment.decade_80s",
                "80s Wave",
                "Your library's finest 1980s synth, new wave, and classic cuts.",
            ),
            (
                "decade_90s",
                ContextualMixKind::Moment,
                "mix.moment.decade_90s",
                "90s Mix",
                "Standout 1990s records and golden-era favorites from your collection.",
            ),
            (
                "decade_2000s",
                ContextualMixKind::Moment,
                "mix.moment.decade_2000s",
                "2000s Mix",
                "Essential 2000s indie, electronic, and throwback tracks from your library.",
            ),
        ];

        for (slug, kind, key, expected_title, expected_subtitle) in cases {
            let summary = sample_summary(slug, kind, key, None);
            assert_eq!(format_mix_title(&summary), expected_title, "slug={slug}");
            assert_eq!(
                format_mix_subtitle(&summary),
                expected_subtitle,
                "slug={slug}"
            );
        }

        // Removed diwali built-in now falls back to generic seasonal subtitle.
        let removed_diwali = sample_summary(
            "diwali",
            ContextualMixKind::Seasonal,
            "mix.seasonal.diwali",
            None,
        );
        assert_eq!(format_mix_title(&removed_diwali), "Diwali");
        assert_eq!(
            format_mix_subtitle(&removed_diwali),
            "Seasonal discovery blend isolated from your core taste profile."
        );
    }

    #[test]
    fn formats_all_candidate_reason_variants_cleanly() {
        assert_eq!(
            format_candidate_reason(&CandidateReason::RegionalContextual {
                mix_slug: "winter_holiday".into(),
                country_code: Some("fr".into()),
            }),
            "FR local pick · Winter Holiday"
        );
        assert_eq!(
            format_candidate_reason(&CandidateReason::InternationalContextual {
                mix_slug: "summer".into(),
            }),
            "International pick · Summer"
        );
        assert_eq!(
            format_candidate_reason(&CandidateReason::TasteWildcard {
                mix_slug: "halloween".into(),
                genre: "Synthwave".into(),
            }),
            "Synthwave crossover · Halloween"
        );
        assert_eq!(
            format_candidate_reason(&CandidateReason::MomentMatch {
                mix_slug: "late_night".into(),
                mood_tag: Some("dream pop".into()),
                genre: Some("shoegaze".into()),
            }),
            "dream pop · shoegaze"
        );
        assert_eq!(
            format_candidate_reason(&CandidateReason::LibraryMatch {
                mix_slug: "drive".into(),
                matched_genre: Some("Funk".into()),
            }),
            "In your library · Funk"
        );
        assert_eq!(
            format_candidate_reason(&CandidateReason::SimilarArtist {
                seed_artist: "Daft Punk".into(),
                artist: "Justice".into(),
            }),
            "Similar to Daft Punk"
        );
    }

    #[test]
    fn builds_percent_encoded_subsonic_urls() {
        let info = NavidromeSessionInfo {
            base_url: "https://music.example.com/".into(),
            username: "alice+dev".into(),
            subsonic_salt: "salt123".into(),
            subsonic_token: "deadbeef".into(),
            api_version: "1.16.1".into(),
            client_name: "Soulbeet".into(),
            server_version: None,
        };
        let stream = subsonic_stream_url(&info, "song/1 2");
        assert!(stream.starts_with("https://music.example.com/rest/stream.view?id=song%2F1%202&"));
        assert!(stream.contains("u=alice%2Bdev"));
        assert!(stream.contains("t=deadbeef"));
        assert!(stream.contains("s=salt123"));

        let art = subsonic_cover_art_url(&info, "al-42", 300);
        assert!(art.starts_with("https://music.example.com/rest/getCoverArt.view?id=al-42&size=300&"));
    }
}
