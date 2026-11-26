//! Client-side localization & formatting for Discovery 2.0 `i18n_key`s, `CandidateReason`s,
//! and OpenSubsonic media URLs derived from [`NavidromeSessionInfo`].

use shared::client::NavidromeSessionInfo;
use shared::recommendation::{CandidateReason, ContextualMixKind, ContextualMixSummary};

/// Formats a human-readable title for a [`ContextualMixSummary`].
/// Honors explicit `custom_name` first; otherwise localizes `i18n_key` (with optional country).
pub fn format_mix_title(summary: &ContextualMixSummary) -> String {
    if let Some(custom) = summary
        .custom_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return custom.to_string();
    }

    let cc = summary
        .country_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_uppercase);

    let base = match summary.i18n_key.as_str() {
        "mix.seasonal.winter_holiday" => match cc.as_deref() {
            Some("FR") => "Noël à la Française",
            Some("DE") | Some("AT") | Some("CH") => "Winter & Festtage",
            Some("ES") | Some("MX") => "Navidad & Invierno",
            Some("IT") => "Natale d'Inverno",
            _ => "Winter Holiday Mix",
        },
        "mix.seasonal.new_years_eve" => "New Year's Eve",
        "mix.seasonal.lunar_new_year" => "Lunar New Year",
        "mix.seasonal.carnival" => "Carnival Beats",
        "mix.seasonal.spring" => "Spring Awakening",
        "mix.seasonal.summer" => "Summer Anthems",
        "mix.seasonal.autumn" => "Autumn Sessions",
        "mix.seasonal.halloween" => "Darkwave Halloween",
        "mix.seasonal.diwali" => "Diwali Celebration",
        "mix.moment.drive" => "Drive Mix",
        "mix.moment.morning" => "Morning Lift",
        "mix.moment.focus" => "Deep Focus",
        "mix.moment.late_night" => "Late Night",
        "mix.rolling.comfort" => "Comfort Mix",
        "mix.rolling.weekly_radar" => "Weekly Radar",
        "mix.rolling.deep_cuts" => "Deep Cuts",
        other => {
            let tail = other.rsplit('.').next().unwrap_or(&summary.slug);
            return humanize_slug(tail);
        }
    };

    if matches!(summary.kind, ContextualMixKind::Seasonal) {
        if let Some(code) = cc {
            if summary.i18n_key != "mix.seasonal.winter_holiday"
                || !matches!(code.as_str(), "FR" | "DE" | "AT" | "CH" | "ES" | "MX" | "IT")
            {
                return format!("{base} · {code}");
            }
        }
    }

    base.to_string()
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
            "Festive local favorites and international winter classics, blended to your taste."
                .to_string()
        }
        "mix.seasonal.new_years_eve" => {
            "High-energy dance floor staples and regional party picks to close the year.".to_string()
        }
        "mix.seasonal.lunar_new_year" => {
            "Celebratory regional pop and fresh festive picks for the new lunar cycle.".to_string()
        }
        "mix.seasonal.carnival" => {
            "Percussive rhythms, samba, funk, and vibrant carnival energy.".to_string()
        }
        "mix.seasonal.spring" => {
            "Fresh indie, acoustic warmth, and bright new-season discoveries.".to_string()
        }
        "mix.seasonal.summer" => {
            "Sun-soaked grooves, upbeat pop, and warm-weather classics.".to_string()
        }
        "mix.seasonal.autumn" => {
            "Cozy acoustic textures, folk, jazz, and mellow indie cuts.".to_string()
        }
        "mix.seasonal.halloween" => {
            "Atmospheric darkwave, post-punk, and nocturnal synth textures.".to_string()
        }
        "mix.seasonal.diwali" => {
            "Festive Indian classics, indie gems, and celebratory rhythms.".to_string()
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
        "mix.moment.late_night" => {
            "Low-lit dream pop, trip hop, and midnight library favorites.".to_string()
        }
        "mix.rolling.comfort" => {
            "High-rotation favorites and trusted genres from your library.".to_string()
        }
        "mix.rolling.weekly_radar" => {
            "Fresh recommendations tuned to your recent listening momentum.".to_string()
        }
        "mix.rolling.deep_cuts" => {
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
            let mix = humanize_slug(mix_slug);
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
            format!("International pick · {}", humanize_slug(mix_slug))
        }
        CandidateReason::TasteWildcard { mix_slug, genre } => {
            format!("{genre} crossover · {}", humanize_slug(mix_slug))
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
                format!("{mood} · {}", humanize_slug(mix_slug))
            }
            (_, Some(g)) if !g.is_empty() => {
                format!("{g} · {}", humanize_slug(mix_slug))
            }
            _ => format!("Matched {}", humanize_slug(mix_slug)),
        },
        CandidateReason::LibraryMatch {
            mix_slug,
            matched_genre,
        } => match matched_genre.as_deref().filter(|g| !g.is_empty()) {
            Some(g) => format!("In your library · {g}"),
            None => format!("In your library · {}", humanize_slug(mix_slug)),
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

fn humanize_slug(slug: &str) -> String {
    slug.split(['_', '-', '.'])
        .filter(|part| !part.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = first.to_uppercase().to_string();
                    out.push_str(chars.as_str());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
        assert_eq!(format_mix_title(&fr_noel), "Noël à la Française");

        let jp_summer = sample_summary(
            "summer",
            ContextualMixKind::Seasonal,
            "mix.seasonal.summer",
            Some("jp"),
        );
        assert_eq!(format_mix_title(&jp_summer), "Summer Anthems · JP");

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
