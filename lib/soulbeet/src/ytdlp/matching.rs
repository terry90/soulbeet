//! Which YouTube Music albums and songs a search offers, and which wanted track each one is.
//!
//! YouTube Music lists an album's tracks with their exact lengths, so a track is matched by its
//! title, its artist, its length and its album, with slskd's weights. Only "Art Track" audio
//! (`MUSIC_VIDEO_TYPE_ATV`) is offered, never a music video or a user upload, and a title that
//! names a version the wanted one does not (live, remix, sped up) is never a match.

use super::error::YtDlpError;
use super::protocol::{AlbumHit, AlbumPage, AlbumTrack, Song};
use crate::matching::assign::assign_tracks;
use crate::matching::diagnostics::{
    RejectionCollector, REASON_BELOW_THRESHOLD, REASON_DURATION_DELTA, REASON_TITLE_MISMATCH,
    REASON_VERSION_VETO,
};
use crate::matching::duration::{duration_score, DURATION_WEIGHT};
use crate::matching::path::WantedTrack as MatchTrack;
use crate::matching::score::{group_score, MIN_SCORE_THRESHOLD};
use crate::matching::text::{clean_name, containment_sim, dice_sim, jaccard_sim, CleanedText};
use crate::matching::version::version_veto;
use shared::download::{
    format_score, sanitize_filename, DownloadableGroup, DownloadableItem, FileFacts, SearchSpec,
};
use shared::metadata::Track;
use shared::ytdlp::{WantedTrack, YtTrack, BACKEND_ID};

/// The albums an album search asks for.
pub(crate) const ALBUM_HITS: u32 = 5;
/// The albums whose tracklist an album search reads, at most.
pub(crate) const ALBUM_PAGES: usize = 3;
/// The songs a track search asks for, per track.
pub(crate) const SONG_HITS: u32 = 10;
/// The tracks a search without an album looks for, at most.
pub(crate) const TRACKS_SEARCHED: usize = 10;

/// An album's artists must name the wanted artist this well, and its title the wanted album.
const ALBUM_GATE: f64 = 0.5;
const TITLE_WEIGHT: f64 = 0.4;
const ARTIST_WEIGHT: f64 = 0.2;
const ALBUM_WEIGHT: f64 = 0.4;
/// Under this, an album score says nothing, and is not held against the track.
const ALBUM_INFO_THRESHOLD: f64 = 0.25;
/// A track this many seconds off the wanted length is another take.
const LENGTH_LIMIT_S: u32 = 30;
/// YouTube's audio-only tracks: the release's own audio, not a video or an upload.
const ART_TRACK: &str = "MUSIC_VIDEO_TYPE_ATV";
/// About what anonymous YouTube serves as Opus (format 251).
pub(crate) const ESTIMATED_KBPS: u32 = 130;
/// The longest part of an item name before its video id, in bytes.
const NAME_MAX_BYTES: usize = 180;

/// What Opus adds to its score: the shared table knows no Opus and scores it as an unknown
/// format. With it, Opus at about 130 kbps scores 0.5, just under an MP3 of 128 kbps.
const OPUS_TERM: f64 = 0.2;

/// The one quality score every YouTube file has: an Opus file at about 130 kbps.
pub(crate) fn quality_score() -> f64 {
    (format_score("opus", Some(ESTIMATED_KBPS)) + OPUS_TERM).min(1.0)
}

/// What a search is for.
#[derive(Debug, Clone)]
pub(crate) struct Wanted {
    pub album: Option<shared::metadata::Album>,
    pub tracks: Vec<Track>,
}

impl Wanted {
    pub(crate) fn of(spec: &SearchSpec) -> Result<Wanted, YtDlpError> {
        if spec.album.is_none() && spec.tracks.is_empty() {
            return Err(YtDlpError::NoSearchTarget);
        }
        let tracks = match &spec.album {
            Some(_) => spec.tracks.clone(),
            None => spec.tracks.iter().take(TRACKS_SEARCHED).cloned().collect(),
        };
        Ok(Wanted {
            album: spec.album.clone(),
            tracks,
        })
    }

    /// What an album search asks for: the spec's own text, else the album's artist and title.
    pub(crate) fn album_query(&self, text: Option<&str>) -> Option<String> {
        let album = self.album.as_ref()?;
        Some(match text.map(str::trim).filter(|t| !t.is_empty()) {
            Some(text) => text.to_string(),
            None => format!("{} {}", album.artist.trim(), album.title.trim()),
        })
    }

    /// What a search for the track `index` asks for.
    pub(crate) fn song_query(&self, index: usize, text: Option<&str>) -> String {
        let track = &self.tracks[index];
        match text.map(str::trim).filter(|t| !t.is_empty()) {
            Some(text) => text.to_string(),
            None => format!("{} {}", track.artist.trim(), track.title.trim()),
        }
    }

    /// The artist the track `index` is by.
    fn artist(&self, index: usize) -> &str {
        match &self.album {
            Some(album) => &album.artist,
            None => &self.tracks[index].artist,
        }
    }

    /// The album the track `index` is on.
    fn album_title(&self, index: usize) -> Option<&str> {
        match &self.album {
            Some(album) => Some(album.title.as_str()),
            None => self.tracks[index].album_title.as_deref(),
        }
        .map(str::trim)
        .filter(|t| !t.is_empty())
    }

    /// What the file of the track `index` is tagged as.
    fn tags(&self, index: usize) -> WantedTrack {
        let track = &self.tracks[index];
        let on_album = self.album.as_ref();
        WantedTrack {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: self.album_title(index).unwrap_or_default().to_string(),
            album_artist: self.artist(index).to_string(),
            track_no: on_album.and_then(|_| u32::try_from(index + 1).ok()),
            track_total: on_album.and_then(|_| u32::try_from(self.tracks.len()).ok()),
            date: on_album
                .and_then(|a| a.release_date.clone())
                .or_else(|| track.release_date.clone()),
            duration_s: track.duration_secs(),
            recording_mbid: track.mbid.clone(),
            release_mbid: on_album
                .and_then(|a| a.mbid.clone())
                .or_else(|| track.release_mbid.clone()),
        }
    }
}

/// How well `text` names `wanted`: the word overlap slskd scores folder names with.
fn name_score(text: &str, wanted: &str) -> f64 {
    let (text, wanted) = (CleanedText::new(text), CleanedText::new(wanted));
    (jaccard_sim(&text, &wanted) + containment_sim(&text, &wanted)) / 2.0
}

/// How much of `wanted`'s words one of `artists` holds.
fn artist_score<'a>(artists: impl IntoIterator<Item = &'a String>, wanted: &str) -> f64 {
    let wanted = CleanedText::new(wanted);
    artists
        .into_iter()
        .map(|a| containment_sim(&CleanedText::new(a), &wanted))
        .fold(0.0, f64::max)
}

/// slskd's title score, on titles without a trailing bracket or year.
fn title_score(title: &str, wanted: &str) -> f64 {
    let (title, wanted) = (
        CleanedText::new(&clean_name(title)),
        CleanedText::new(&clean_name(wanted)),
    );
    dice_sim(&title, &wanted) * 0.6 + containment_sim(&title, &wanted) * 0.4
}

/// A YouTube Music track scored as a wanted one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scored {
    score: f64,
    title: f64,
    album: f64,
    /// It names a version the wanted title does not: under the floor, whatever its score.
    vetoed: bool,
}

impl Scored {
    fn under(&self) -> bool {
        self.vetoed || self.score < MIN_SCORE_THRESHOLD
    }
}

/// A YouTube Music track as a search found it.
struct Candidate<'a> {
    title: &'a str,
    artists: Vec<&'a String>,
    album: Option<&'a str>,
    duration_s: Option<u32>,
}

/// `candidate` scored as the wanted track `index`, or None when it is not that track: its
/// title shares no word with the wanted one, or its length is 30 s or more off.
fn score_as(candidate: &Candidate, index: usize, wanted: &Wanted) -> Option<Scored> {
    let track = &wanted.tracks[index];
    let title = title_score(candidate.title, &track.title);
    if title == 0.0 {
        return None;
    }
    let lengths = track.duration_secs().zip(candidate.duration_s);
    if lengths.is_some_and(|(w, c)| w.abs_diff(c) >= LENGTH_LIMIT_S) {
        return None;
    }
    let mut sum = title * TITLE_WEIGHT;
    let mut weight = TITLE_WEIGHT;
    if let Some((w, c)) = lengths {
        sum += duration_score(w, c) * DURATION_WEIGHT;
        weight += DURATION_WEIGHT;
    }
    let artist = artist_score(candidate.artists.iter().copied(), wanted.artist(index)).max(
        artist_score(candidate.artists.iter().copied(), &track.artist),
    );
    sum += artist * ARTIST_WEIGHT;
    weight += ARTIST_WEIGHT;
    let mut album = 0.0;
    if let Some(wanted_album) = wanted.album_title(index) {
        album = candidate.album.map_or(0.0, |a| name_score(a, wanted_album));
        sum += album * ALBUM_WEIGHT;
        if album > ALBUM_INFO_THRESHOLD {
            weight += ALBUM_WEIGHT;
        }
    }
    Some(Scored {
        score: sum / weight,
        title,
        album,
        vetoed: version_veto(candidate.title, &track.title),
    })
}

/// Why `score_as` returned `None` (pure diagnostics; scoring logic unchanged).
fn score_miss_reason(candidate: &Candidate, index: usize, wanted: &Wanted) -> &'static str {
    let track = &wanted.tracks[index];
    if title_score(candidate.title, &track.title) == 0.0 {
        return REASON_TITLE_MISMATCH;
    }
    let lengths = track.duration_secs().zip(candidate.duration_s);
    if lengths.is_some_and(|(w, c)| w.abs_diff(c) >= LENGTH_LIMIT_S) {
        return REASON_DURATION_DELTA;
    }
    REASON_BELOW_THRESHOLD
}

/// What a search read, for its stats.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Counts {
    /// Tracks weighed against the wanted ones.
    pub weighed: u32,
    /// Videos and uploads left out, and tracks YouTube Music does not play.
    pub not_audio: u32,
    pub below_floor: u32,
    pub rejections: RejectionCollector,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Counts) {
        self.weighed += other.weighed;
        self.not_audio += other.not_audio;
        self.below_floor += other.below_floor;
        self.rejections.extend(other.rejections);
    }
}

/// The albums worth reading the tracklist of, best first: their artists name the wanted
/// artist and their title the wanted album, each well enough.
pub(crate) fn shortlist<'a>(hits: &'a [AlbumHit], wanted: &Wanted) -> Vec<&'a AlbumHit> {
    let Some(album) = &wanted.album else {
        return vec![];
    };
    let mut kept: Vec<(f64, &AlbumHit)> = hits
        .iter()
        .filter(|hit| artist_score(&hit.artists, &album.artist) >= ALBUM_GATE)
        .map(|hit| (name_score(&hit.title, &album.title), hit))
        .filter(|(score, _)| *score >= ALBUM_GATE)
        .collect();
    // Stable: equal albums keep YouTube Music's order.
    kept.sort_by(|(a, _), (b, _)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    kept.into_iter()
        .take(ALBUM_PAGES)
        .map(|(_, hit)| hit)
        .collect()
}

/// Whether YouTube Music offers the track as the release's own audio, and plays it.
fn audio(video_type: Option<&str>, available: bool, video_id: Option<&str>) -> bool {
    video_type == Some(ART_TRACK) && available && video_id.is_some_and(valid_video_id)
}

/// Whether `id` is a YouTube video id: 11 letters, digits, `-` or `_`.
pub(crate) fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The source of a track: its album, or the song itself when it is on none. A source failing
/// often is then one YouTube edition, never all of YouTube.
pub(crate) fn source_of(browse_id: Option<&str>, video_id: &str) -> String {
    format!("ytm:{}", browse_id.unwrap_or(video_id))
}

/// The file name a track lands as, which is its item id: `{artist} - {title} [{video id}].opus`,
/// sanitized as a batch folder's names are, the part before the id cut to 180 bytes.
pub fn item_name(artist: &str, title: &str, video_id: &str) -> String {
    let named = match artist.trim() {
        "" => title.trim().to_string(),
        artist => format!("{artist} - {}", title.trim()),
    };
    let mut named = sanitize_filename(&named);
    if named.len() > NAME_MAX_BYTES {
        let mut at = NAME_MAX_BYTES;
        while !named.is_char_boundary(at) {
            at -= 1;
        }
        named.truncate(at);
    }
    format!("{} [{video_id}].opus", named.trim_end())
}

/// What the items of one group share.
struct Of<'a> {
    wanted: &'a Wanted,
    /// The album's browse id, None for a song on no album.
    browse_id: Option<&'a str>,
    /// How many tracks the album has.
    album_tracks: Option<u32>,
    /// The transfers waiting in the backend's queue now.
    queued: u64,
}

/// What YouTube Music says of a track, kept next to what it is matched as.
struct Seen<'a> {
    video_id: &'a str,
    title: &'a str,
    artists: Vec<&'a String>,
    album: Option<&'a str>,
    duration_s: Option<u32>,
    explicit: bool,
}

impl Of<'_> {
    fn item(&self, seen: &Seen, index: usize, scored: Scored) -> DownloadableItem {
        let wanted = self.wanted.tags(index);
        let source = source_of(self.browse_id, seen.video_id);
        let mut facts = serde_json::Map::new();
        facts.insert("lossy_only".into(), true.into());
        facts.insert("origin".into(), "youtube_music".into());
        facts.insert("estimated_kbps".into(), ESTIMATED_KBPS.into());
        facts.insert("video_type".into(), ART_TRACK.into());
        facts.insert("explicit".into(), seen.explicit.into());
        facts.insert("yt_title".into(), seen.title.into());
        facts.insert(
            "yt_artists".into(),
            seen.artists
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .into(),
        );
        if let Some(album) = seen.album {
            facts.insert("yt_album".into(), album.into());
        }
        let url = match self.browse_id {
            Some(browse_id) => format!("https://music.youtube.com/browse/{browse_id}"),
            None => format!("https://music.youtube.com/watch?v={}", seen.video_id),
        };
        facts.insert("item_url".into(), url.into());
        let data = YtTrack {
            video_id: seen.video_id.to_string(),
            browse_id: self.browse_id.map(str::to_string),
            title_score: scored.title,
            album_score: scored.album,
            wanted: wanted.clone(),
        };
        DownloadableItem {
            id: item_name(&wanted.artist, &wanted.title, seen.video_id),
            backend: BACKEND_ID.to_string(),
            source,
            // Failover pairs files of one track across sources by the wanted title.
            title: wanted.title.clone(),
            artist: wanted.artist.clone(),
            album: wanted.album.clone(),
            size: None,
            duration: seen.duration_s,
            quality: "opus".to_string(),
            quality_score: quality_score(),
            backend_data: serde_json::to_string(&data).ok(),
            match_score: scored.score,
            below_floor: scored.under(),
            facts: FileFacts {
                extension: Some("opus".into()),
                variable_bitrate: Some(true),
                sample_rate_hz: Some(48_000),
                folder_audio_files: self.album_tracks,
                source_queue_length: Some(self.queued),
                backend_facts: facts,
                ..FileFacts::default()
            },
        }
    }
}

/// The wanted tracks as the shared matcher takes them.
fn wanted_tracks(wanted: &Wanted) -> Vec<MatchTrack> {
    (wanted.tracks.iter())
        .map(|t| MatchTrack {
            title: t.title.clone(),
            duration_s: t.duration_secs(),
        })
        .collect()
}

/// The artists of an album's track: its own, then the album's.
fn artists_of<'a>(track: &'a AlbumTrack, page: &'a AlbumPage) -> Vec<&'a String> {
    track.artists.iter().chain(&page.artists).collect()
}

/// One album as a group of the wanted tracks it has, and what was read of it. A track under the
/// floor is left out, or kept and flagged `below_floor` when `keep_below_floor`. None when no
/// track is kept.
pub(crate) fn album_group(
    hit: &AlbumHit,
    page: &AlbumPage,
    wanted: &Wanted,
    keep_below_floor: bool,
    queued: u64,
) -> (Option<DownloadableGroup>, Counts) {
    let playable: Vec<&_> = (page.tracks.iter())
        .filter(|t| audio(t.video_type.as_deref(), t.available, t.video_id.as_deref()))
        .collect();
    let mut counts = Counts {
        weighed: playable.len() as u32,
        not_audio: (page.tracks.len() - playable.len()) as u32,
        below_floor: 0,
        rejections: RejectionCollector::default(),
    };
    let scores: Vec<Vec<Option<Scored>>> = (playable.iter())
        .map(|t| {
            let candidate = Candidate {
                title: &t.title,
                artists: artists_of(t, page),
                album: Some(&page.title),
                duration_s: t.duration_s,
            };
            (0..wanted.tracks.len())
                .map(|i| score_as(&candidate, i, wanted))
                .collect()
        })
        .collect();
    let album_source = source_of(Some(&hit.browse_id), "");
    for (idx, per_track) in scores.iter().enumerate() {
        if !per_track.iter().flatten().any(|s| !s.under()) {
            counts.below_floor += 1;
            let track_ref = playable[idx];
            let best_scored = per_track.iter().flatten().max_by(|a, b| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let (score, reason) = match best_scored {
                Some(s) if s.vetoed => (Some(s.score as f32), REASON_VERSION_VETO),
                Some(s) => (Some(s.score as f32), REASON_BELOW_THRESHOLD),
                None => {
                    let candidate = Candidate {
                        title: &track_ref.title,
                        artists: artists_of(track_ref, page),
                        album: Some(&page.title),
                        duration_s: track_ref.duration_s,
                    };
                    let mut reason = REASON_TITLE_MISMATCH;
                    for i in 0..wanted.tracks.len() {
                        let r = score_miss_reason(&candidate, i, wanted);
                        if r == REASON_DURATION_DELTA {
                            reason = r;
                            break;
                        }
                    }
                    (None, reason)
                }
            };
            counts
                .rejections
                .record(album_source.clone(), track_ref.title.clone(), score, reason);
        }
    }
    let indexes: Vec<usize> = (0..playable.len()).collect();
    let taken = assign_tracks(
        &wanted_tracks(wanted),
        &indexes,
        |&c, t, _| {
            scores[c][t]
                .filter(|s| keep_below_floor || !s.under())
                .map(|s| s.score)
        },
        // Between two that match as well, the explicit one: the version a release has first.
        |&c| f64::from(u8::from(playable[c].explicit)),
    );
    let picked: Vec<(usize, usize)> = (taken.into_iter().enumerate())
        .filter_map(|(t, took)| took.map(|(c, _)| (t, c)))
        .collect();
    if picked.is_empty() {
        return (None, counts);
    }
    let of = Of {
        wanted,
        browse_id: Some(&hit.browse_id),
        album_tracks: Some(page.tracks.len() as u32),
        queued,
    };
    let items: Vec<DownloadableItem> = (picked.iter())
        .map(|&(t, c)| {
            let track = playable[c];
            let seen = Seen {
                video_id: track.video_id.as_deref().unwrap_or_default(),
                title: &track.title,
                artists: artists_of(track, page),
                album: Some(&page.title),
                duration_s: track.duration_s,
                explicit: track.explicit,
            };
            let scored = scores[c][t].expect("an assigned pair has a score");
            of.item(&seen, t, scored)
        })
        .collect();
    let group = group_of(
        items,
        wanted.tracks.len(),
        &hit.browse_id,
        &source_of(Some(&hit.browse_id), ""),
        &page.title,
        &page.artists,
    );
    (Some(group), counts)
}

/// One group per song a track search found that is the wanted track `index`, best first, and
/// what was read. Between two songs that match as well, the explicit one comes first, as on an
/// album.
pub(crate) fn song_groups(
    songs: &[Song],
    index: usize,
    wanted: &Wanted,
    keep_below_floor: bool,
    queued: u64,
) -> (Vec<DownloadableGroup>, Counts) {
    let mut counts = Counts::default();
    let mut groups = Vec::new();
    for song in songs {
        if !audio(
            song.video_type.as_deref(),
            song.available,
            Some(&song.video_id),
        ) {
            counts.not_audio += 1;
            continue;
        }
        counts.weighed += 1;
        let album = song.album.as_ref();
        let candidate = Candidate {
            title: &song.title,
            artists: song.artists.iter().collect(),
            album: album.map(|a| a.name.as_str()),
            duration_s: song.duration_s,
        };
        let browse_id = album.and_then(|a| a.browse_id.as_deref());
        let src = source_of(browse_id, &song.video_id);
        let Some(scored) = score_as(&candidate, index, wanted) else {
            counts.below_floor += 1;
            let reason = score_miss_reason(&candidate, index, wanted);
            counts
                .rejections
                .record(src, song.title.clone(), None, reason);
            continue;
        };
        if scored.under() {
            counts.below_floor += 1;
            let reason = if scored.vetoed {
                REASON_VERSION_VETO
            } else {
                REASON_BELOW_THRESHOLD
            };
            counts
                .rejections
                .record(src, song.title.clone(), Some(scored.score as f32), reason);
            if !keep_below_floor {
                continue;
            }
        }
        let of = Of {
            wanted,
            browse_id,
            album_tracks: None,
            queued,
        };
        let seen = Seen {
            video_id: &song.video_id,
            title: &song.title,
            artists: song.artists.iter().collect(),
            album: album.map(|a| a.name.as_str()),
            duration_s: song.duration_s,
            explicit: song.explicit,
        };
        let item = of.item(&seen, index, scored);
        let title = album.map_or(song.title.as_str(), |a| a.name.as_str());
        let group = group_of(
            vec![item],
            1,
            &song.video_id,
            &source_of(browse_id, &song.video_id),
            title,
            &song.artists,
        );
        groups.push((song.explicit, group));
    }
    // Stable: songs alike in both keep YouTube Music's order.
    groups.sort_by(|(a_explicit, a), (b_explicit, b)| {
        (b.score.partial_cmp(&a.score))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b_explicit.cmp(a_explicit))
    });
    (groups.into_iter().map(|(_, g)| g).collect(), counts)
}

fn group_of(
    items: Vec<DownloadableItem>,
    wanted: usize,
    group_id: &str,
    source: &str,
    title: &str,
    artists: &[String],
) -> DownloadableGroup {
    let mean_match = items.iter().map(|i| i.match_score).sum::<f64>() / items.len() as f64;
    let completeness = items.len() as f64 / wanted.max(1) as f64;
    DownloadableGroup {
        source: source.to_string(),
        group_id: group_id.to_string(),
        title: title.to_string(),
        artist: (!artists.is_empty()).then(|| artists.join(", ")),
        item_count: items.len(),
        total_size: 0,
        score: group_score(mean_match, completeness, quality_score()),
        quality: "opus".to_string(),
        items,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ytdlp::protocol::Answer;

    /// The answer the helper printed from a saved YouTube Music answer.
    pub(crate) fn answer(name: &str) -> Answer {
        let text = match name {
            "slint-albums" => include_str!("fixtures/slint-albums.ndjson"),
            "slint-spiderland" => include_str!("fixtures/slint-spiderland.ndjson"),
            "slint-songs" => include_str!("fixtures/slint-songs.ndjson"),
            "monkeys-songs" => include_str!("fixtures/monkeys-songs.ndjson"),
            "luther-songs" => include_str!("fixtures/luther-songs.ndjson"),
            other => panic!("no fixture {other}"),
        };
        serde_json::from_str(text.trim()).unwrap()
    }

    fn albums(name: &str) -> Vec<AlbumHit> {
        match answer(name) {
            Answer::Albums(albums) => albums,
            other => panic!("{other:?}"),
        }
    }

    fn page(name: &str) -> AlbumPage {
        match answer(name) {
            Answer::Album(page) => page,
            other => panic!("{other:?}"),
        }
    }

    pub(crate) fn songs(name: &str) -> Vec<Song> {
        match answer(name) {
            Answer::Songs(songs) => songs,
            other => panic!("{other:?}"),
        }
    }

    pub(crate) fn track(title: &str, artist: &str, album: Option<&str>, length: &str) -> Track {
        Track {
            id: title.into(),
            title: title.into(),
            artist: artist.into(),
            album_id: None,
            album_title: album.map(str::to_string),
            release_date: None,
            duration: Some(length.into()),
            mbid: Some(format!("rec-{title}")),
            release_mbid: None,
        }
    }

    /// Spiderland as MusicBrainz lists it: its tracks and their lengths.
    pub(crate) fn spiderland() -> Wanted {
        let tracks = [
            ("Breadcrumb Trail", "5:55"),
            ("Nosferatu Man", "5:35"),
            ("Don, Aman", "6:28"),
            ("Washer", "8:50"),
            ("For Dinner…", "5:05"),
            ("Good Morning, Captain", "7:39"),
        ];
        Wanted {
            album: Some(shared::metadata::Album {
                id: "spiderland".into(),
                title: "Spiderland".into(),
                artist: "Slint".into(),
                release_date: Some("1991-03-27".into()),
                mbid: Some("rel-spiderland".into()),
                cover_url: None,
                release_types: None,
                first_release_date: None,
            }),
            tracks: tracks
                .iter()
                .map(|(title, length)| track(title, "Slint", Some("Spiderland"), length))
                .collect(),
        }
    }

    #[test]
    fn spiderland_is_found_whole_and_scores_about_eight_tenths() {
        let wanted = spiderland();
        let hits = albums("slint-albums");
        let shortlisted = shortlist(&hits, &wanted);
        // Tweez is Slint's too, but not the album.
        assert_eq!(
            shortlisted
                .iter()
                .map(|h| h.title.as_str())
                .collect::<Vec<_>>(),
            ["Spiderland"]
        );
        let (group, counts) =
            album_group(shortlisted[0], &page("slint-spiderland"), &wanted, false, 2);
        let group = group.unwrap();
        assert_eq!(group.items.len(), 6);
        assert_eq!(group.source, "ytm:MPREb_ur8iGYISD11");
        assert!((0.78..=0.81).contains(&group.score), "{}", group.score);
        assert_eq!(
            counts,
            Counts {
                weighed: 6,
                not_audio: 0,
                below_floor: 0,
                ..Counts::default()
            }
        );
        let first = &group.items[0];
        assert_eq!(
            (
                first.title.as_str(),
                first.artist.as_str(),
                first.album.as_str()
            ),
            ("Breadcrumb Trail", "Slint", "Spiderland")
        );
        assert_eq!(first.id, "Slint - Breadcrumb Trail [gBfoQjJ7guk].opus");
        assert_eq!(first.facts.source_queue_length, Some(2));
        assert_eq!(first.facts.backend_facts["estimated_kbps"], 130);
        let data: YtTrack = serde_json::from_str(first.backend_data.as_deref().unwrap()).unwrap();
        assert_eq!(data.video_id, "gBfoQjJ7guk");
        assert_eq!(data.wanted.track_no, Some(1));
        assert_eq!(data.wanted.track_total, Some(6));
        assert_eq!(data.wanted.release_mbid.as_deref(), Some("rel-spiderland"));
        assert_eq!((data.title_score, data.album_score), (1.0, 1.0));
        // Every file is Opus at about 130 kbps, under every lossless file and MP3 of 128 kbps.
        assert!(group.items.iter().all(|i| i.quality_score == 0.5));
        assert!(quality_score() < format_score("mp3", Some(128)));
    }

    #[test]
    fn a_track_search_takes_the_original_album_over_its_remaster() {
        let wanted = Wanted {
            album: None,
            tracks: vec![track(
                "Breadcrumb Trail",
                "Slint",
                Some("Spiderland"),
                "5:56",
            )],
        };
        let (groups, _) = song_groups(&songs("slint-songs"), 0, &wanted, false, 0);
        let mut groups = groups;
        groups.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        let best: Vec<(&str, &str)> = groups
            .iter()
            .take(2)
            .map(|g| {
                (
                    g.items[0].facts.backend_facts["yt_album"].as_str().unwrap(),
                    g.group_id.as_str(),
                )
            })
            .collect();
        assert_eq!(
            best,
            [
                ("Spiderland", "gBfoQjJ7guk"),
                ("Spiderland (remastered)", "7jst4--Wsv8")
            ]
        );
        assert!(groups[0].score > groups[1].score);
        // The alternate "Dry" mix ranks under both.
        let dry = groups.iter().position(|g| g.group_id == "XF6xT2Cya2I");
        assert!(dry.is_none_or(|at| at > 1), "{dry:?}");
    }

    #[test]
    fn sped_up_and_fast_versions_are_never_a_match() {
        let wanted = Wanted {
            album: None,
            tracks: vec![track(
                "Monkeys Spinning Monkeys",
                "Kevin MacLeod",
                None,
                "2:05",
            )],
        };
        let (groups, counts) = song_groups(&songs("monkeys-songs"), 0, &wanted, false, 0);
        let ids: Vec<&str> = groups.iter().map(|g| g.group_id.as_str()).collect();
        assert!(ids.contains(&"MdzF0rFOC9Y"), "{ids:?}");
        // (Fast) at 87 s and (SPEED UP) at 82 s.
        assert!(!ids.contains(&"i0hvIWcVJes"), "{ids:?}");
        assert!(!ids.contains(&"Q-M4-CI0QV8"), "{ids:?}");
        assert!(counts.below_floor > 0);

        // Kept when the search asks for those, flagged.
        let (kept, _) = song_groups(&songs("monkeys-songs"), 0, &wanted, true, 0);
        let sped = kept.iter().find(|g| g.group_id == "kLsmu7V3oi8").unwrap();
        assert!(sped.items[0].below_floor);
    }

    #[test]
    fn a_track_far_from_the_wanted_length_is_dropped() {
        // Breadcrumb Trail is 356 s; one wanted at 300 s is another take.
        let wanted = Wanted {
            album: None,
            tracks: vec![track(
                "Breadcrumb Trail",
                "Slint",
                Some("Spiderland"),
                "5:00",
            )],
        };
        let (groups, _) = song_groups(&songs("slint-songs"), 0, &wanted, true, 0);
        assert!(groups.is_empty(), "{groups:?}");
        assert_eq!(duration_score(300, 356), 0.0);
    }

    #[test]
    fn a_music_video_or_an_unplayable_track_is_not_audio() {
        let wanted = spiderland();
        let mut page = page("slint-spiderland");
        page.tracks[0].video_type = Some("MUSIC_VIDEO_TYPE_OMV".into());
        page.tracks[1].available = false;
        let hits = albums("slint-albums");
        let (group, counts) = album_group(&hits[0], &page, &wanted, false, 0);
        assert_eq!(counts.not_audio, 2);
        assert_eq!(counts.weighed, 4);
        let group = group.unwrap();
        assert_eq!(group.items.len(), 4);
        assert!(!group.items.iter().any(|i| i.title == "Breadcrumb Trail"));
    }

    #[test]
    fn an_explicit_and_a_clean_version_alike_go_to_the_explicit_one() {
        let wanted = Wanted {
            album: None,
            tracks: vec![track("luther", "Kendrick Lamar", Some("GNX"), "2:58")],
        };
        let mut found = songs("luther-songs");
        let mut clean = found[0].clone();
        clean.video_id = "cleanluther".into();
        clean.explicit = false;
        found[0].explicit = true;
        found.insert(0, clean);
        let page = AlbumPage {
            title: "GNX".into(),
            artists: vec!["Kendrick Lamar".into()],
            tracks: found
                .iter()
                .take(2)
                .map(|s| AlbumTrack {
                    video_id: Some(s.video_id.clone()),
                    title: s.title.clone(),
                    duration_s: s.duration_s,
                    available: true,
                    video_type: s.video_type.clone(),
                    artists: s.artists.clone(),
                    explicit: s.explicit,
                })
                .collect(),
        };
        let hit = AlbumHit {
            browse_id: "MPREb_XGvjZBLVuzr".into(),
            title: "GNX".into(),
            artists: vec!["Kendrick Lamar".into()],
        };
        let album = Wanted {
            album: Some(shared::metadata::Album {
                id: "gnx".into(),
                title: "GNX".into(),
                artist: "Kendrick Lamar".into(),
                release_date: None,
                mbid: None,
                cover_url: None,
                release_types: None,
                first_release_date: None,
            }),
            ..wanted
        };
        let (group, _) = album_group(&hit, &page, &album, false, 0);
        assert_eq!(
            group.unwrap().items[0].facts.backend_facts["explicit"],
            true
        );

        // A track search puts the explicit one first too, though YouTube Music listed it second.
        let (groups, _) = song_groups(&found[..2], 0, &album, false, 0);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].score, groups[1].score);
        assert_eq!(groups[0].items[0].facts.backend_facts["explicit"], true);
    }

    #[test]
    fn an_item_name_has_no_separator_and_keeps_its_video_id() {
        assert_eq!(
            item_name("AC/DC", "What's Next to the Moon?", "abcdefghijk"),
            "AC_DC - What's Next to the Moon_ [abcdefghijk].opus"
        );
        assert_eq!(
            item_name(" ", "Intro: Part 1", "abcdefghijk"),
            "Intro_ Part 1 [abcdefghijk].opus"
        );
        let long = item_name("Artist", &"é".repeat(200), "abcdefghijk");
        assert!(long.ends_with(" [abcdefghijk].opus"), "{long}");
        assert!(long.len() <= NAME_MAX_BYTES + " [abcdefghijk].opus".len());
        assert!(!long.contains(['/', '\\']));
    }

    #[test]
    fn only_a_youtube_video_id_is_one() {
        assert!(valid_video_id("MdzF0rFOC9Y"));
        assert!(valid_video_id("7jst4--Wsv8"));
        for bad in ["", "short", "MdzF0rFOC9Y1", "../../etc/x", "MdzF0rFOC9/"] {
            assert!(!valid_video_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_search_names_its_target() {
        assert!(matches!(
            Wanted::of(&SearchSpec::default()),
            Err(YtDlpError::NoSearchTarget)
        ));
        let wanted = spiderland();
        assert_eq!(wanted.album_query(None).unwrap(), "Slint Spiderland");
        assert_eq!(wanted.album_query(Some(" slint ")).unwrap(), "slint");
        let many = SearchSpec::of(None, vec![track("a", "b", None, "1:00"); 20]);
        assert_eq!(Wanted::of(&many).unwrap().tracks.len(), TRACKS_SEARCHED);
        assert_eq!(Wanted::of(&many).unwrap().song_query(0, None), "b a");
    }
}
