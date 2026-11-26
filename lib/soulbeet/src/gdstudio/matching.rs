//! Which NetEase rows are the wanted tracks, and the groups and items they are offered as.
//!
//! NetEase's rows are structured: a title, an album, an artist list, and no length until the
//! file is read. A row is scored title against title, album against album and its artists
//! against the wanted artist; once its file's header is read, its length counts too.

use super::api::{Located, Row};
use super::audio_header::Header;
use super::error::GdError;
use crate::matching::assign::assign_tracks;
use crate::matching::duration::{duration_score, DURATION_WEIGHT};
use crate::matching::path::WantedTrack;
use crate::matching::score::{group_score, MIN_SCORE_THRESHOLD};
use crate::matching::text::{containment_sim, dice_sim, jaccard_sim, CleanedText};
use crate::matching::version::version_veto;
use shared::download::{
    format_score, sanitize_filename, DownloadableGroup, DownloadableItem, FileFacts, SearchSpec,
};
use shared::gdstudio::{GdMusicBrainz, GdTags, GdTrack, BACKEND_ID, SOURCE};
use std::collections::HashMap;

const TITLE_WEIGHT: f64 = 0.4;
const ALBUM_WEIGHT: f64 = 0.4;
const ARTIST_WEIGHT: f64 = 0.2;
/// A row's artists must name the wanted artist this well, unless its title and album both
/// match well: NetEase can write an artist in another script (高田みどり for Midori Takada).
const ARTIST_GATE: f64 = 0.6;
/// Title and album scores at which a row is the track whatever script its artist is in.
const CLOSE: f64 = 0.8;
/// A file this many seconds off the wanted length is another take.
const LENGTH_LIMIT_S: u32 = 30;

/// Markers of another version NetEase's catalogue uses besides the shared ones: live, a cover
/// of, a DJ edit, a backing vocal, a concert, a ringtone, a preview.
const EXTRA_MARKERS: &[&str] = &["现场", "翻自", "dj版", "伴唱", "演唱会", "铃声", "试听"];

/// Whether `candidate` names a version `wanted` does not.
fn other_version(candidate: &str, wanted: &str) -> bool {
    let (candidate_l, wanted_l) = (candidate.to_lowercase(), wanted.to_lowercase());
    version_veto(candidate, wanted)
        || EXTRA_MARKERS
            .iter()
            .any(|m| candidate_l.contains(m) && !wanted_l.contains(m))
}

/// One wanted track, with what its tags need.
#[derive(Debug, Clone)]
pub(crate) struct Track {
    pub wanted: WantedTrack,
    pub artist: String,
    pub recording: Option<String>,
}

/// What a search is for.
#[derive(Debug, Clone)]
pub(crate) struct Wanted {
    pub artist: String,
    /// The album the search names, or the album of the one track searched.
    pub album: Option<String>,
    pub tracks: Vec<Track>,
    pub release: Option<String>,
    pub date: Option<String>,
    /// An album's tracks, not one track.
    pub is_album: bool,
}

impl Wanted {
    /// An album of more than one track, or one track: an album with one track is searched as
    /// that track, as slskd does.
    pub(crate) fn of(spec: &SearchSpec) -> Result<Wanted, GdError> {
        let nonempty = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let tracks = (spec.tracks.iter())
            .map(|t| Track {
                wanted: WantedTrack {
                    title: t.title.trim().to_string(),
                    duration_s: t.duration_secs(),
                },
                artist: t.artist.trim().to_string(),
                recording: t.mbid.clone(),
            })
            .collect();
        match (&spec.album, spec.tracks.as_slice()) {
            (Some(album), [_, _, ..]) => Ok(Wanted {
                artist: album.artist.trim().to_string(),
                album: nonempty(&album.title),
                tracks,
                release: album.mbid.clone(),
                date: album.release_date.clone(),
                is_album: true,
            }),
            (album, [track]) => Ok(Wanted {
                artist: track.artist.trim().to_string(),
                album: (album.as_ref().map(|a| a.title.as_str()))
                    .or(track.album_title.as_deref())
                    .and_then(nonempty),
                tracks,
                release: (album.as_ref().and_then(|a| a.mbid.clone()))
                    .or(track.release_mbid.clone()),
                date: (album.as_ref().and_then(|a| a.release_date.clone()))
                    .or(track.release_date.clone()),
                is_album: false,
            }),
            (_, tracks) => Err(GdError::NoSearchTarget {
                tracks: tracks.len(),
            }),
        }
    }

    /// The text a search sends: the artist and the album, or the artist and the track.
    pub(crate) fn text(&self) -> String {
        let named = match (self.is_album, &self.album, self.tracks.first()) {
            (true, Some(album), _) => album.as_str(),
            (_, _, Some(track)) => track.wanted.title.as_str(),
            _ => "",
        };
        format!("{} {named}", self.artist).trim().to_string()
    }
}

/// How well `text` names `wanted`: the word overlap slskd scores folder names with.
fn name_score(text: &str, wanted: &str) -> f64 {
    let (text, wanted) = (CleanedText::new(text), CleanedText::new(wanted));
    (jaccard_sim(&text, &wanted) + containment_sim(&text, &wanted)) / 2.0
}

/// How well a title names a wanted one: slskd's title score.
fn title_score(title: &str, wanted: &str) -> f64 {
    let (title, wanted) = (CleanedText::new(title), CleanedText::new(wanted));
    dice_sim(&title, &wanted) * 0.6 + containment_sim(&title, &wanted) * 0.4
}

/// How much of the wanted artist's words the row's artists hold between them.
fn artist_score(artists: &[String], wanted: &str) -> f64 {
    containment_sim(
        &CleanedText::new(&artists.join(" ")),
        &CleanedText::new(wanted),
    )
}

/// A row scored as one wanted track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Fit {
    pub score: f64,
    pub title: f64,
    /// 0 when no album is wanted.
    pub album: f64,
    pub length_diff_s: Option<u32>,
}

/// `row` scored as `track`, its file `seconds` long when that is known; None when it is not
/// that track: another version, another artist, or another take by its length.
pub(crate) fn fit(row: &Row, track: &Track, wanted: &Wanted, seconds: Option<u32>) -> Option<Fit> {
    if other_version(&row.name, &track.wanted.title) {
        return None;
    }
    let lengths = seconds.zip(track.wanted.duration_s);
    let length_diff_s = lengths.map(|(file, want)| file.abs_diff(want));
    if length_diff_s.is_some_and(|d| d >= LENGTH_LIMIT_S) {
        return None;
    }
    let title = title_score(&row.name, &track.wanted.title);
    let album = wanted.album.as_deref().map(|a| name_score(&row.album, a));
    let artist = artist_score(&row.artist, &wanted.artist);
    let mut sum = title * TITLE_WEIGHT;
    let mut weight = TITLE_WEIGHT;
    if let Some(album) = album {
        // NetEase always names the album, so a wanted album always counts.
        sum += album * ALBUM_WEIGHT;
        weight += ALBUM_WEIGHT;
    }
    if artist >= ARTIST_GATE {
        sum += artist * ARTIST_WEIGHT;
        weight += ARTIST_WEIGHT;
    } else if !(title >= CLOSE && album.is_some_and(|a| a >= CLOSE)) {
        return None;
    }
    if let Some((file, want)) = lengths {
        sum += duration_score(want, file) * DURATION_WEIGHT;
        weight += DURATION_WEIGHT;
    }
    Some(Fit {
        score: sum / weight,
        title,
        album: album.unwrap_or(0.0),
        length_diff_s,
    })
}

fn under(score: f64) -> bool {
    score < MIN_SCORE_THRESHOLD
}

/// For each wanted track, the row of `rows` that is it and its fit, before any file is read;
/// rows under the floor only with `keep_below_floor`.
pub(crate) fn assign(rows: &[Row], wanted: &Wanted, keep_below_floor: bool) -> Assigned {
    let tracks: Vec<WantedTrack> = wanted.tracks.iter().map(|t| t.wanted.clone()).collect();
    // Each row's fit as each track, the rows given by their index.
    let fits: Vec<Vec<Option<Fit>>> = rows
        .iter()
        .map(|row| {
            (wanted.tracks.iter())
                .map(|t| fit(row, t, wanted, None).filter(|f| keep_below_floor || !under(f.score)))
                .collect()
        })
        .collect();
    let indexes: Vec<usize> = (0..rows.len()).collect();
    let taken = assign_tracks(
        &tracks,
        &indexes,
        |&r, t, _| fits[r][t].map(|f| f.score),
        |_| 0.0,
    );
    (taken.into_iter().enumerate())
        .map(|(t, took)| took.and_then(|(r, _)| fits[r][t].map(|f| (r, f))))
        .collect()
}

/// For each wanted track, the index of the row that is it and its fit.
pub(crate) type Assigned = Vec<Option<(usize, Fit)>>;

/// An album's name, its rows, and which of them are the wanted tracks.
pub(crate) type AlbumFit = (String, Vec<Row>, Assigned);

/// The rows of `rows` by album, in the order the albums first appear.
pub(crate) fn albums(rows: &[Row]) -> Vec<(String, Vec<Row>)> {
    let mut albums: Vec<(String, Vec<Row>)> = Vec::new();
    for row in rows {
        match albums.iter_mut().find(|(name, _)| *name == row.album) {
            Some((_, kept)) => kept.push(row.clone()),
            None => albums.push((row.album.clone(), vec![row.clone()])),
        }
    }
    albums
}

/// The album of `rows` that holds the most wanted tracks, the best matched between equals, and
/// its assignment.
pub(crate) fn best_album(
    rows: &[Row],
    wanted: &Wanted,
    keep_below_floor: bool,
) -> Option<AlbumFit> {
    let mut best: Option<AlbumFit> = None;
    let rank = |taken: &[Option<(usize, Fit)>]| {
        let fits: Vec<f64> = taken.iter().flatten().map(|(_, f)| f.score).collect();
        (fits.len(), fits.iter().sum::<f64>())
    };
    for (name, album) in albums(rows) {
        let taken = assign(&album, wanted, keep_below_floor);
        let better = match &best {
            None => true,
            Some((_, _, kept)) => {
                let ((n, sum), (m, kept_sum)) = (rank(&taken), rank(kept));
                n > m || (n == m && sum > kept_sum)
            }
        };
        if better {
            best = Some((name, album, taken));
        }
    }
    best
}

/// The album names `rows` hold, the one closest to `album` first; only those sharing a word
/// with it.
pub(crate) fn closest_albums(rows: &[Row], album: &str) -> Vec<String> {
    let mut named: Vec<(f64, String)> = albums(rows)
        .into_iter()
        .map(|(name, _)| (name_score(&name, album), name))
        .filter(|(score, _)| *score > 0.0)
        .collect();
    named.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    named.into_iter().map(|(_, name)| name).collect()
}

/// A row the search resolved: its file and what its header says.
#[derive(Debug, Clone)]
pub(crate) struct Read {
    pub row: Row,
    pub located: Located,
    pub header: Header,
}

impl Read {
    pub(crate) fn seconds(&self) -> Option<u32> {
        (self.header.seconds(self.located.size, self.located.kbps)).map(|s| s.round() as u32)
    }

    fn bits_and_rate(&self) -> (Option<u32>, Option<u32>) {
        match self.header {
            Header::Flac { bits, rate, .. } => (Some(bits), Some(rate)),
            Header::Mp3 { .. } => (None, None),
        }
    }

    /// The shared format score, with no term of NetEase's own: there is no peer, slot or
    /// queue, and a 24-bit header earns nothing over a 16-bit one. A lossless file's bitrate
    /// says nothing of its quality, so only a lossy one's counts.
    pub(crate) fn quality_score(&self) -> f64 {
        let kbps = (self.located.ext != "flac").then_some(self.located.kbps);
        format_score(&self.located.ext, kbps).min(1.0)
    }
}

/// The most bytes a landing name has: the 255 a file name may have on most file systems, less
/// the 19 a ticks copy of it adds when the name is taken.
const NAME_BYTES: usize = 255 - 19;

/// The name a file lands under and the last segment of its item id. A long title is cut so the
/// name fits in `NAME_BYTES`, and its ` [<track id>].<ext>` end is always whole.
pub(crate) fn file_name(position: Option<usize>, title: &str, track_id: &str, ext: &str) -> String {
    let prefix = position.map(|n| format!("{n:02} - ")).unwrap_or_default();
    let suffix = sanitize_filename(&format!(" [{track_id}].{ext}"));
    let title = sanitize_filename(title);
    let room = NAME_BYTES.saturating_sub(prefix.len() + suffix.len());
    let cut = (0..=room.min(title.len()))
        .rev()
        .find(|&at| title.is_char_boundary(at))
        .unwrap_or(0);
    format!("{prefix}{}{suffix}", &title[..cut])
}

/// One resolved row as the wanted track at `index`, which it fits as `fit`.
fn item(
    read: &Read,
    index: usize,
    fit: Fit,
    wanted: &Wanted,
    album_rows: u32,
    below_floor: bool,
) -> DownloadableItem {
    let track = &wanted.tracks[index];
    let position = wanted.is_album.then_some(index + 1);
    let (bits, rate) = read.bits_and_rate();
    let seconds = read.seconds();
    let ext = read.located.ext.clone();
    let album = wanted
        .album
        .clone()
        .unwrap_or_else(|| read.row.album.clone());
    let musicbrainz =
        track
            .recording
            .clone()
            .zip(wanted.release.clone())
            .map(|(recording, release)| GdMusicBrainz {
                recording,
                release,
                title_score: fit.title,
                album_score: fit.album,
                length_diff_s: fit.length_diff_s,
            });
    let data = GdTrack {
        track_id: read.row.id.clone(),
        ext: ext.clone(),
        size: read.located.size,
        md5: read.located.md5.clone(),
        tags: GdTags {
            title: track.wanted.title.clone(),
            artist: track.artist.clone(),
            album: album.clone(),
            album_artist: wanted.artist.clone(),
            track: position.map(|n| n as u32),
            track_total: wanted.is_album.then_some(wanted.tracks.len() as u32),
            date: wanted.date.clone(),
            musicbrainz,
        },
    };
    let mut backend_facts = serde_json::Map::new();
    backend_facts.insert("netease_track_id".into(), read.row.id.clone().into());
    backend_facts.insert("netease_title".into(), read.row.name.clone().into());
    backend_facts.insert("netease_artists".into(), read.row.artist.clone().into());
    backend_facts.insert("netease_album".into(), read.row.album.clone().into());
    // Bit depth and rate come from the header and were not checked against the audio.
    backend_facts.insert("header_only".into(), true.into());
    DownloadableItem {
        id: format!(
            "{}/{}",
            read.row.id,
            file_name(position, &track.wanted.title, &read.row.id, &ext)
        ),
        backend: BACKEND_ID.to_string(),
        source: SOURCE.to_string(),
        // Failover pairs files of one track across sources by the wanted title.
        title: track.wanted.title.clone(),
        artist: track.artist.clone(),
        album,
        size: Some(read.located.size),
        duration: seconds,
        quality: ext.clone(),
        quality_score: read.quality_score(),
        backend_data: serde_json::to_string(&data).ok(),
        match_score: fit.score,
        below_floor,
        facts: FileFacts {
            extension: Some(ext),
            bitrate_kbps: Some(read.located.kbps),
            variable_bitrate: None,
            sample_rate_hz: rate,
            bit_depth: bits,
            folder_audio_files: Some(album_rows),
            source_has_free_slot: Some(true),
            source_upload_speed: None,
            source_queue_length: Some(0),
            backend_facts,
        },
    }
}

/// The wanted tracks `reads` (each with the index of the wanted track it was assigned) are, as
/// one group named after `album`, which NetEase lists `album_rows` rows of. Each read is scored
/// again with its length; one that no longer fits is left out. None when nothing is left, or
/// for an album when less than half of it is, unless `keep_below_floor`, which keeps and flags
/// what is under the floor.
pub(crate) fn group(
    album: &str,
    reads: &[(usize, Read)],
    wanted: &Wanted,
    album_rows: u32,
    keep_below_floor: bool,
) -> (Option<DownloadableGroup>, u32) {
    let fitted: Vec<(usize, &Read, Fit)> = reads
        .iter()
        .filter_map(|(t, read)| {
            fit(&read.row, &wanted.tracks[*t], wanted, read.seconds()).map(|f| (*t, read, f))
        })
        .collect();
    let below_floor = fitted.iter().filter(|(_, _, f)| under(f.score)).count() as u32;
    let kept: Vec<&(usize, &Read, Fit)> = fitted
        .iter()
        .filter(|(_, _, f)| keep_below_floor || !under(f.score))
        .collect();
    if kept.is_empty() || wanted.tracks.is_empty() {
        return (None, below_floor);
    }
    let short = wanted.is_album && kept.len() * 2 < wanted.tracks.len();
    if short && !keep_below_floor {
        return (None, below_floor);
    }
    let masters: std::collections::HashSet<(Option<u32>, Option<u32>)> =
        kept.iter().map(|(_, r, _)| r.bits_and_rate()).collect();
    let mut items: Vec<DownloadableItem> = kept
        .iter()
        .map(|(t, read, f)| item(read, *t, *f, wanted, album_rows, short || under(f.score)))
        .collect();
    if masters.len() > 1 {
        // An album of several masters: some tracks 24-bit, others 16, as NetEase holds it.
        for item in &mut items {
            item.facts
                .backend_facts
                .insert("mixed_masters".into(), true.into());
        }
    }
    let completeness = items.len() as f64 / wanted.tracks.len() as f64;
    let mean_match = items.iter().map(|i| i.match_score).sum::<f64>() / items.len() as f64;
    let mean_quality = items.iter().map(|i| i.quality_score).sum::<f64>() / items.len() as f64;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for i in &items {
        *counts.entry(i.quality.as_str()).or_default() += 1;
    }
    let quality = (counts.into_iter())
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(q, _)| q.to_string())
        .unwrap_or_default();
    let first = kept[0].1;
    (
        Some(DownloadableGroup {
            source: SOURCE.to_string(),
            group_id: format!("netease:{}", first.row.id),
            title: album.to_string(),
            artist: first.row.artist.first().cloned(),
            item_count: items.len(),
            total_size: items.iter().filter_map(|i| i.size).sum(),
            score: group_score(mean_match, completeness, mean_quality),
            quality,
            items,
        }),
        below_floor,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gdstudio::api::tests::fixture;
    use shared::metadata::{Album, Track as MbTrack};
    use std::time::Instant;

    pub(crate) fn rows(name: &str) -> Vec<Row> {
        serde_json::from_str(&fixture(name)).unwrap()
    }

    pub(crate) fn mb_track(artist: &str, title: &str, duration: Option<&str>) -> MbTrack {
        MbTrack {
            id: title.into(),
            title: title.into(),
            artist: artist.into(),
            album_id: None,
            album_title: None,
            release_date: None,
            duration: duration.map(str::to_string),
            mbid: Some(format!("rec-{title}")),
            release_mbid: None,
        }
    }

    pub(crate) fn album_spec(artist: &str, album: &str, titles: &[&str]) -> SearchSpec {
        SearchSpec::of(
            Some(Album {
                id: album.into(),
                title: album.into(),
                artist: artist.into(),
                release_date: Some("2001-03-12".into()),
                mbid: Some(format!("rel-{album}")),
                cover_url: None,
                release_types: None,
                first_release_date: None,
            }),
            titles.iter().map(|t| mb_track(artist, t, None)).collect(),
        )
    }

    fn track_spec(artist: &str, title: &str, album: Option<&str>) -> SearchSpec {
        let mut track = mb_track(artist, title, None);
        track.album_title = album.map(str::to_string);
        SearchSpec::of(None, vec![track])
    }

    pub(crate) fn read_of(row: &Row, ext: &str, kbps: u32, header: Header, size: u64) -> Read {
        Read {
            row: row.clone(),
            located: Located {
                url: format!("https://m701.music.126.net/x/{}.{ext}", "a".repeat(32)),
                host: "m701.music.126.net".into(),
                kbps,
                size,
                ext: ext.into(),
                md5: "a".repeat(32),
                issued: Instant::now(),
            },
            header,
        }
    }

    pub(crate) fn flac(bits: u32, rate: u32, seconds: u64) -> Header {
        Header::Flac {
            bits,
            rate,
            channels: 2,
            samples: seconds * u64::from(rate),
        }
    }

    fn matched(taken: &[Option<(usize, Fit)>]) -> usize {
        taken.iter().flatten().count()
    }

    #[test]
    fn an_album_in_native_script_matches_by_its_title_and_album() {
        let wanted = Wanted::of(&album_spec(
            "Midori Takada",
            "Through the Looking Glass",
            &[
                "Mr. Henri Rousseau's Dream",
                "Crossing",
                "Trompe-l'œil",
                "Catastrophe Σ",
            ],
        ))
        .unwrap();
        let rows = rows("s5_midori.json");
        let taken = assign(&rows, &wanted, false);
        assert_eq!(matched(&taken), 4, "{taken:?}");
        // Without the album the artist rule cannot apply, and 高田みどり is not Midori Takada.
        let lone = Wanted::of(&track_spec("Midori Takada", "Crossing", None)).unwrap();
        assert!(fit(&rows[1], &lone.tracks[0], &lone, None).is_none());
    }

    #[test]
    fn every_cover_of_an_unlicensed_song_is_dropped() {
        let rows = rows("s6_jay.json");
        for album in [Some("叶惠美"), None] {
            let wanted = Wanted::of(&track_spec("周杰伦", "晴天", album)).unwrap();
            let kept: Vec<&str> = rows
                .iter()
                .filter(|r| fit(r, &wanted.tracks[0], &wanted, None).is_some())
                .map(|r| r.name.as_str())
                .collect();
            assert!(kept.is_empty(), "{album:?}: {kept:?}");
        }
        assert!(other_version("晴天 (现场)", "晴天"));
        assert!(other_version("晴天 (DJ版)", "晴天"));
        assert!(!other_version("晴天", "晴天"));
    }

    #[test]
    fn the_track_on_the_wanted_album_ranks_first() {
        let rows = rows("s2_track_search.json");
        let wanted =
            Wanted::of(&track_spec("Stromae", "Papaoutai", Some("Racine carrée"))).unwrap();
        let mut scored: Vec<(f64, &str)> = rows
            .iter()
            .filter_map(|r| {
                fit(r, &wanted.tracks[0], &wanted, None).map(|f| (f.score, r.album.as_str()))
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        assert_eq!(scored[0].1, "racine carrée", "{scored:?}");
        // The remixes are other versions.
        assert!(rows
            .iter()
            .filter(|r| r.name.contains("Remix"))
            .all(|r| fit(r, &wanted.tracks[0], &wanted, None).is_none()));
    }

    #[test]
    fn a_whole_album_matches_each_track_once() {
        let titles: Vec<String> = rows("s4_kol.json").iter().map(|r| r.name.clone()).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let wanted = Wanted::of(&album_spec("Kings of Leon", "O My Beloved", &titles)).unwrap();
        let taken = assign(&rows("s4_kol.json"), &wanted, false);
        assert_eq!(matched(&taken), 13);
        for (t, took) in taken.iter().enumerate() {
            assert_eq!(took.unwrap().0, t, "track {t} took its own row");
        }
        assert_eq!(wanted.text(), "Kings of Leon O My Beloved");
    }

    #[test]
    fn a_track_far_off_the_wanted_length_is_another_take() {
        let rows = rows("s7_okc.json");
        let mut spec = album_spec("Radiohead", "OK Computer", &["Airbag", "Paranoid Android"]);
        spec.tracks[0].duration = Some("4:47".into());
        let wanted = Wanted::of(&spec).unwrap();
        let close = fit(&rows[0], &wanted.tracks[0], &wanted, Some(288)).unwrap();
        assert_eq!(close.length_diff_s, Some(1));
        assert!((close.score - 1.0).abs() < 1e-9, "{close:?}");
        assert!(fit(&rows[0], &wanted.tracks[0], &wanted, Some(317)).is_none());
        let off = fit(&rows[0], &wanted.tracks[0], &wanted, Some(300)).unwrap();
        assert!(off.score < close.score);
    }

    #[test]
    fn a_mixed_master_album_scores_its_formats_alike_and_says_it_is_mixed() {
        let rows = rows("s7_okc.json");
        let wanted = Wanted::of(&album_spec(
            "Radiohead",
            "OK Computer",
            &["Airbag", "Paranoid Android", "Subterranean Homesick Alien"],
        ))
        .unwrap();
        // u10: track 1, 24/44.1 at 1695 kbps; u11: track 3, 16/44.1 at 956 kbps.
        let reads = vec![
            (
                0,
                read_of(&rows[0], "flac", 1695, flac(24, 44_100, 288), 61_024_163),
            ),
            (
                2,
                read_of(&rows[2], "flac", 956, flac(16, 44_100, 268), 31_996_978),
            ),
        ];
        assert_eq!(reads[0].1.quality_score(), reads[1].1.quality_score());
        // A lossless file's bitrate earns or costs nothing.
        let quiet = read_of(&rows[0], "flac", 98, flac(16, 44_100, 30), 360_000);
        assert_eq!(quiet.quality_score(), 1.0);
        let mp3 = read_of(&rows[0], "mp3", 192, Header::Mp3 { id3: 45 }, 7_000_000);
        assert_eq!(mp3.quality_score(), format_score("mp3", Some(192)));
        let (group, _) = group("OK Computer", &reads, &wanted, 12, false);
        let group = group.unwrap();
        assert_eq!(group.items.len(), 2);
        assert!(group
            .items
            .iter()
            .all(|i| i.facts.backend_facts["mixed_masters"] == true));
        assert_eq!(
            (
                group.items[0].facts.bit_depth,
                group.items[1].facts.bit_depth
            ),
            (Some(24), Some(16))
        );
        assert_eq!(group.source, "NetEase");
        assert_eq!(group.group_id, "netease:22497470");
    }

    #[test]
    fn a_whole_flac_album_scores_as_a_slskd_one_with_the_same_match() {
        let source = rows("s7_okc.json");
        let titles: Vec<String> = source.iter().map(|r| r.name.clone()).collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let wanted = Wanted::of(&album_spec("Radiohead", "OK Computer", &titles)).unwrap();
        let reads: Vec<(usize, Read)> = source
            .iter()
            .enumerate()
            .map(|(t, r)| {
                (
                    t,
                    read_of(r, "flac", 1000, flac(16, 44_100, 200), 25_000_000),
                )
            })
            .collect();
        let gd = group("OK Computer", &reads, &wanted, 12, false).0.unwrap();
        assert_eq!(gd.items.len(), 12);
        // A slskd FLAC from a peer with a free slot scores 1.0 too.
        let slskd_quality = (format_score("flac", None) + 0.1).min(1.0);
        let mean_match = gd.items.iter().map(|i| i.match_score).sum::<f64>() / 12.0;
        assert_eq!(gd.score, group_score(mean_match, 1.0, slskd_quality));
        assert!(gd.score > 0.99, "{}", gd.score);
    }

    #[test]
    fn an_item_lands_under_its_position_title_and_track_id() {
        let source = rows("s1_album_search.json");
        let wanted = Wanted::of(&album_spec(
            "Daft Punk",
            "Discovery",
            &[
                "One More Time",
                "Aerodynamic",
                "Harder, Better, Faster, Stronger",
            ],
        ))
        .unwrap();
        let reads = vec![(
            2,
            read_of(&source[3], "mp3", 320, Header::Mp3 { id3: 45 }, 9_000_000),
        )];
        let group = group("Discovery", &reads, &wanted, 14, true).0.unwrap();
        let item = &group.items[0];
        assert_eq!(
            item.id,
            "17455803/03 - Harder, Better, Faster, Stronger [17455803].mp3"
        );
        assert!(item.below_floor, "one of three is less than half the album");
        assert_eq!(
            (item.backend.as_str(), item.source.as_str()),
            ("gdstudio", "NetEase")
        );
        assert_eq!(item.duration, Some(225));
        assert_eq!(item.facts.source_queue_length, Some(0));
        let data: GdTrack = serde_json::from_str(item.backend_data.as_deref().unwrap()).unwrap();
        assert_eq!((data.tags.track, data.tags.track_total), (Some(3), Some(3)));
        assert_eq!(
            data.tags.musicbrainz.unwrap().recording,
            "rec-Harder, Better, Faster, Stronger"
        );
        assert_eq!(
            file_name(None, "AC/DC: Live?", "1", "flac"),
            "AC_DC_ Live_ [1].flac"
        );
        // 120 CJK characters are 360 bytes: the title is cut on a character, the end kept.
        let long = "夜".repeat(120);
        let name = file_name(Some(7), &long, "1306373588", "flac");
        assert_eq!(name.len(), 236);
        assert!(name.starts_with("07 - 夜"));
        assert!(name.ends_with("夜 [1306373588].flac"));
        // A character that does not fit whole is left out.
        let mixed = format!("ab{long}");
        assert_eq!(file_name(Some(7), &mixed, "1306373588", "flac").len(), 235);
        // Not offered at all unless below-floor files are kept.
        assert!(group_of_short(&reads, &wanted).is_none());
    }

    fn group_of_short(reads: &[(usize, Read)], wanted: &Wanted) -> Option<DownloadableGroup> {
        group("Discovery", reads, wanted, 14, false).0
    }

    #[test]
    fn only_an_album_or_one_track_is_a_search() {
        assert!(matches!(
            Wanted::of(&SearchSpec::of(None, vec![])),
            Err(GdError::NoSearchTarget { tracks: 0 })
        ));
        let two = vec![mb_track("a", "x", None), mb_track("a", "y", None)];
        assert!(Wanted::of(&SearchSpec::of(None, two)).is_err());
        let single = album_spec("Stromae", "Racine carrée", &["Papaoutai"]);
        let wanted = Wanted::of(&single).unwrap();
        assert!(!wanted.is_album);
        assert_eq!(wanted.album.as_deref(), Some("Racine carrée"));
        assert_eq!(wanted.text(), "Stromae Papaoutai");
    }
}
