//! Live catalog search quality benchmark derived from real production library tracks.
//!
//! Marked `#[ignore]` so CI does not depend on external network availability.
//! Run manually with:
//! `cargo test -p soulbeet --test live_search_benchmark -- --ignored --nocapture`

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use shared::metadata::{CatalogProvider, UnifiedCatalogSearch};
use shared::search_norm::{canonical_artist_key, canonical_title_key, fold_search_text};
use soulbeet::deezer::DeezerClient;
use soulbeet::knowledge::CacheError;
use soulbeet::musicbrainz;
use soulbeet::polite::Pace;
use soulbeet::traits::KnowledgeCache;

struct BenchNoopCache;

#[async_trait]
impl KnowledgeCache for BenchNoopCache {
    async fn get(&self, _key: &str) -> Result<Option<String>, CacheError> {
        Ok(None)
    }

    async fn put(&self, _key: &str, _value: &str, _ttl: Duration) -> Result<(), CacheError> {
        Ok(())
    }
}

async fn hybrid_search(
    deezer: &DeezerClient,
    query: &str,
    limit: usize,
) -> UnifiedCatalogSearch {
    match deezer.search_unified(query, limit).await {
        Ok(res)
            if !res.tracks.is_empty()
                || !res.albums.is_empty()
                || !res.artists.is_empty() =>
        {
            res
        }
        _ => {
            let mut mb = musicbrainz::search_unified(query, limit)
                .await
                .expect("MusicBrainz fallback should succeed");
            mb.fallback_used = true;
            mb
        }
    }
}

fn assert_no_duplicate_studio_tracks(res: &UnifiedCatalogSearch) {
    let mut seen = HashSet::new();
    for t in &res.tracks {
        if t.variant_label.is_none() {
            let key = (canonical_artist_key(&t.artist), canonical_title_key(&t.title));
            assert!(
                seen.insert(key.clone()),
                "Duplicate studio track in {:?} for query {:?}: {} - {} (album: {:?})",
                res.provider,
                res.query,
                t.artist,
                t.title,
                t.album_title
            );
        }
    }
}

fn assert_no_karaoke(res: &UnifiedCatalogSearch) {
    for t in &res.tracks {
        assert_ne!(
            t.variant_label.as_deref(),
            Some("Karaoke"),
            "Karaoke leaked into query {:?}: {} - {}",
            res.query,
            t.artist,
            t.title
        );
        let low_art = fold_search_text(&t.artist);
        assert!(
            !low_art.contains("karaoke") && !low_art.contains("prosource"),
            "Karaoke artist leaked into query {:?}: {}",
            res.query,
            t.artist
        );
    }
}

#[tokio::test]
#[ignore = "live network benchmark against Deezer and MusicBrainz"]
async fn prod_library_unified_search_benchmark() {
    let deezer = DeezerClient::new(
        "https://api.deezer.com",
        Pace::DEEZER,
        Arc::new(BenchNoopCache),
    )
    .expect("deezer client builds");

    struct TrackCase {
        query: &'static str,
        expect_artist_sub: &'static str,
        expect_title_sub: &'static str,
        expect_provider: Option<CatalogProvider>,
        expect_variant: Option<&'static str>,
        forbid_live: bool,
        max_same_song_rows: usize,
    }

    let cases = [
        TrackCase {
            query: "Blue oyster the reaper",
            expect_artist_sub: "blue oyster cult",
            expect_title_sub: "reaper",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "the reaper",
            expect_artist_sub: "blue oyster cult",
            expect_title_sub: "reaper",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Ours Samplus Saliverne",
            expect_artist_sub: "ours samplus",
            expect_title_sub: "saliverne",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Lorien Testard Lumiere",
            expect_artist_sub: "lorien testard",
            expect_title_sub: "lumiere",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Hermanos Gutierrez Tres hermanos",
            expect_artist_sub: "hermanos gutierrez",
            expect_title_sub: "tres hermanos",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Gealdyr Eptir Ragnarok",
            expect_artist_sub: "gealdyr",
            expect_title_sub: "eptir ragnarok",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "4 non blondes whats up",
            expect_artist_sub: "4 non blondes",
            expect_title_sub: "whats up",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Bee Gees Stayin Alive",
            expect_artist_sub: "bee gees",
            expect_title_sub: "stayin alive",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Palace Live Well",
            expect_artist_sub: "palace",
            expect_title_sub: "live well",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "OMI Cheerleader",
            expect_artist_sub: "omi",
            expect_title_sub: "cheerleader",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Alain Bashung Residents de la Republique",
            expect_artist_sub: "alain bashung",
            expect_title_sub: "residents de la republique",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Alain Bashung Residents de la Republique live",
            expect_artist_sub: "alain bashung",
            expect_title_sub: "residents de la republique",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: Some("Live"),
            forbid_live: false,
            max_same_song_rows: 3,
        },
        TrackCase {
            query: "Mylene Farmer Sans contrefacon",
            expect_artist_sub: "mylene farmer",
            expect_title_sub: "sans contrefacon",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "4 non blondes whats up piano",
            expect_artist_sub: "4 non blondes",
            expect_title_sub: "whats up",
            expect_provider: Some(CatalogProvider::Deezer),
            expect_variant: Some("Acoustic"),
            forbid_live: true,
            max_same_song_rows: 3,
        },
        // Niche / OST / underground queries that Deezer misses -> MusicBrainz automatic fallback!
        TrackCase {
            query: "Akira Yamaoka Murder Song",
            expect_artist_sub: "akira yamaoka",
            expect_title_sub: "murder song",
            expect_provider: Some(CatalogProvider::Musicbrainz),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
        TrackCase {
            query: "Drago Cheese Road To Dawn",
            expect_artist_sub: "drago cheese",
            expect_title_sub: "road to dawn",
            expect_provider: Some(CatalogProvider::Musicbrainz),
            expect_variant: None,
            forbid_live: true,
            max_same_song_rows: 2,
        },
    ];

    println!("\n=== LIVE HYBRID SEARCH BENCHMARK ({} queries) ===", cases.len());
    for c in &cases {
        let res = hybrid_search(&deezer, c.query, 15).await;
        assert_no_duplicate_studio_tracks(&res);
        assert_no_karaoke(&res);

        let top = res
            .tracks
            .first()
            .unwrap_or_else(|| panic!("No tracks returned for {:?}", c.query));

        println!(
            "[{:?}{}] Q={:?} -> {} tracks, {} albums, {} artists | #1: {} - {} [album={:?}, variant={:?}]",
            res.provider,
            if res.fallback_used { " fallback" } else { "" },
            c.query,
            res.tracks.len(),
            res.albums.len(),
            res.artists.len(),
            top.artist,
            top.title,
            top.album_title,
            top.variant_label
        );

        if let Some(p) = c.expect_provider {
            assert_eq!(res.provider, p, "Unexpected provider for {:?}", c.query);
        }
        assert!(
            fold_search_text(&top.artist).contains(c.expect_artist_sub),
            "Q={:?}: expected #1 artist containing {:?}, got {:?}",
            c.query,
            c.expect_artist_sub,
            top.artist
        );
        assert!(
            fold_search_text(&top.title).contains(c.expect_title_sub),
            "Q={:?}: expected #1 title containing {:?}, got {:?}",
            c.query,
            c.expect_title_sub,
            top.title
        );
        if let Some(v) = c.expect_variant {
            assert_eq!(
                top.variant_label.as_deref(),
                Some(v),
                "Q={:?}: expected #1 variant {:?}, got {:?}",
                c.query,
                v,
                top.variant_label
            );
        }
        if c.forbid_live {
            assert_ne!(
                top.variant_label.as_deref(),
                Some("Live"),
                "Q={:?}: #1 track falsely marked Live: {} - {}",
                c.query,
                top.artist,
                top.title
            );
        }
        let top_base = shared::search_norm::base_song_title_key(&top.title);
        let top_art = canonical_artist_key(&top.artist);
        let same_count = res
            .tracks
            .iter()
            .filter(|t| {
                canonical_artist_key(&t.artist) == top_art
                    && shared::search_norm::base_song_title_key(&t.title) == top_base
            })
            .count();
        assert!(
            same_count <= c.max_same_song_rows,
            "Q={:?}: too many near-duplicate rows ({} > {}): {:#?}",
            c.query,
            same_count,
            c.max_same_song_rows,
            res.tracks
                .iter()
                .map(|t| (&t.artist, &t.title, &t.album_title, &t.variant_label))
                .collect::<Vec<_>>()
        );
    }

    // Specific studio-album promotion proof for "the reaper": Agents Of Fortune must win over compilations!
    let reaper = hybrid_search(&deezer, "the reaper", 15).await;
    let top_reaper = &reaper.tracks[0];
    assert!(
        fold_search_text(top_reaper.album_title.as_deref().unwrap_or("")).contains("agents of fortune"),
        "Expected 'the reaper' #1 album to be Agents of Fortune, got {:?}",
        top_reaper.album_title
    );

    // Artist & Album queries
    let boc = hybrid_search(&deezer, "Blue Oyster Cult", 15).await;
    assert!(
        fold_search_text(&boc.artists[0].name).contains("blue oyster cult"),
        "Expected #1 artist Blue Öyster Cult, got {:?}",
        boc.artists[0].name
    );
    let aof = hybrid_search(&deezer, "Agents of Fortune", 15).await;
    assert!(
        fold_search_text(&aof.albums[0].title).contains("agents of fortune"),
        "Expected #1 album Agents of Fortune, got {:?}",
        aof.albums[0].title
    );

    // Explicit MusicBrainz deep-catalog proof for the exact prod bug query!
    let mb_reaper = musicbrainz::search_unified("Blue oyster the reaper", 15)
        .await
        .expect("MusicBrainz unified search succeeds");
    assert_no_duplicate_studio_tracks(&mb_reaper);
    assert_no_karaoke(&mb_reaper);
    println!(
        "[MusicBrainz direct] Q='Blue oyster the reaper' -> {} tracks | top3={:?}",
        mb_reaper.tracks.len(),
        mb_reaper
            .tracks
            .iter()
            .take(3)
            .map(|t| (&t.artist, &t.title, &t.album_title, &t.variant_label))
            .collect::<Vec<_>>()
    );
    let mb_top = &mb_reaper.tracks[0];
    assert!(fold_search_text(&mb_top.artist).contains("blue oyster cult"));
    assert!(fold_search_text(&mb_top.title).contains("reaper"));
    assert!(
        mb_reaper.tracks.len() <= 5,
        "Expected collapsed MusicBrainz results (<= 5), got {}",
        mb_reaper.tracks.len()
    );

    // Deezer album detail & artist catalog proof
    let detail = deezer
        .get_catalog_album_detail(1408080)
        .await
        .expect("album detail ok")
        .expect("Agents of Fortune exists");
    assert!(detail.tracks.len() >= 10);
    assert!(detail
        .tracks
        .iter()
        .any(|t| fold_search_text(&t.title).contains("reaper")));

    let artist = deezer
        .get_artist_catalog(692)
        .await
        .expect("artist catalog ok")
        .expect("Blue Oyster Cult exists");
    assert!(!artist.top_tracks.is_empty());
    assert!(!artist.albums.is_empty());
    println!(
        "=== BENCHMARK PASSED: 16/16 track queries + artist/album + MB deep catalog + detail lookups ==="
    );
}
