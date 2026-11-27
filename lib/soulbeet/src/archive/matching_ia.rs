//! Which archive.org items and files a search offers, and which wanted track each file is.
//!
//! An item is one group. Its files are matched by their own tags (title, track number, length)
//! where slskd guesses from a path: uploaders' tags can be wrong, so a weak title still matches
//! by its track number and length, and a file far from the wanted length never does.

use super::client::{Doc, ItemFile};
use super::error::ArchiveError;
use super::query::Target;
use crate::audio::AUDIO_EXTENSIONS;
use crate::matching::assign::assign_tracks;
use crate::matching::diagnostics::{
    RejectionCollector, REASON_BELOW_THRESHOLD, REASON_DURATION_DELTA, REASON_TITLE_MISMATCH,
    REASON_VERSION_VETO,
};
use crate::matching::duration::{duration_score, DURATION_WEIGHT};
use crate::matching::path::WantedTrack;
use crate::matching::score::{group_score, MIN_SCORE_THRESHOLD};
use crate::matching::text::{clean_name, containment_sim, dice_sim, jaccard_sim, CleanedText};
use crate::matching::version::version_veto;
use regex::Regex;
use shared::archive::{ArchiveFile, BACKEND_ID};
use shared::download::{format_score, DownloadableGroup, DownloadableItem, FileFacts, SearchSpec};
use std::collections::HashMap;
use std::sync::LazyLock;

/// The items a search reads the files of, at most.
pub(crate) const SHORTLIST: usize = 8;

/// An item's creator must name the wanted artist this well, unless it names the wanted release.
const ARTIST_GATE: f64 = 0.6;

const TITLE_WEIGHT: f64 = 0.4;
const ARTIST_WEIGHT: f64 = 0.2;
const ALBUM_WEIGHT: f64 = 0.4;
/// Under this, an album score says the tags and the item name no album, and is not held
/// against the file.
const ALBUM_INFO_THRESHOLD: f64 = 0.25;
/// What a track number that agrees gives a title too weak to match on its own.
const NUMBER_CREDIT: f64 = 0.7;
/// A file this many seconds off the wanted length is another take.
const LENGTH_LIMIT_S: u32 = 30;
/// A file whose title shares no word with the wanted one matches by number only this close.
const NUMBER_ONLY_LENGTH_S: u32 = 3;

/// Format labels archive.org gives items that have audio, in any case.
const AUDIO_LABELS: &[&str] = &[
    "flac",
    "mp3",
    "ogg",
    "mpeg-4",
    "mpeg4",
    "apple lossless",
    "aac",
    "wave",
    "windows media",
    "opus",
];

static RE_LEAD_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:[A-D])?\d{1,3}(?:\s*[.\-)]\s*|\s+)").unwrap());

/// What a search is for, as its files are scored against it.
#[derive(Debug, Clone)]
pub(crate) struct Wanted {
    pub artist: String,
    /// The album the search names, or the album of the one track searched.
    pub album: Option<String>,
    pub tracks: Vec<WantedTrack>,
    /// The MusicBrainz release the search names.
    pub release: Option<String>,
}

impl Wanted {
    pub(crate) fn of(spec: &SearchSpec) -> Result<Wanted, ArchiveError> {
        let tracks = spec
            .tracks
            .iter()
            .map(|t| WantedTrack {
                title: t.title.clone(),
                duration_s: t.duration_secs(),
            })
            .collect();
        let nonempty = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        match (&spec.album, spec.tracks.as_slice()) {
            (Some(album), _) => Ok(Wanted {
                artist: album.artist.trim().to_string(),
                album: nonempty(&album.title),
                tracks,
                release: album.mbid.clone(),
            }),
            (None, [track]) => Ok(Wanted {
                artist: track.artist.trim().to_string(),
                album: track.album_title.as_deref().and_then(nonempty),
                tracks,
                release: track.release_mbid.clone(),
            }),
            (None, tracks) => Err(ArchiveError::NoSearchTarget {
                tracks: tracks.len(),
            }),
        }
    }

    /// Whether the search is for an album's tracks: then an item with less than half of them
    /// is not the album.
    fn is_album(&self) -> bool {
        self.tracks.len() > 1
    }

    /// What the search asks archive.org for: the album, or the one track.
    pub(crate) fn target(&self) -> Target<'_> {
        match (&self.album, self.tracks.first()) {
            (None, Some(track)) => Target::Track {
                artist: &self.artist,
                title: &track.title,
            },
            (album, _) => Target::Album {
                artist: &self.artist,
                album: album.as_deref().unwrap_or_default(),
            },
        }
    }
}

/// How well `text` names `wanted`: the word overlap slskd scores folder names with.
fn name_score(text: &str, wanted: &str) -> f64 {
    let (text, wanted) = (CleanedText::new(text), CleanedText::new(wanted));
    (jaccard_sim(&text, &wanted) + containment_sim(&text, &wanted)) / 2.0
}

/// How much of `wanted`'s words one of `creators` holds.
fn artist_score<'a>(creators: impl IntoIterator<Item = &'a str>, wanted: &str) -> f64 {
    let wanted = CleanedText::new(wanted);
    creators
        .into_iter()
        .map(|c| containment_sim(&CleanedText::new(c), &wanted))
        .fold(0.0, f64::max)
}

/// How well a title names a wanted one: slskd's title score.
fn title_score(title: &str, wanted: &CleanedText) -> f64 {
    let title = CleanedText::new(title);
    dice_sim(&title, wanted) * 0.6 + containment_sim(&title, wanted) * 0.4
}

/// An item the search found, and whether it was found by the wanted release's id.
#[derive(Debug, Clone)]
pub(crate) struct Found {
    pub doc: Doc,
    pub release_hit: bool,
}

/// Whether the item has an audio format at all.
fn has_audio(doc: &Doc) -> bool {
    (doc.format.0.iter()).any(|label| {
        let label = label.to_lowercase();
        AUDIO_LABELS.iter().any(|a| label.contains(a))
    })
}

/// The items worth reading the files of, best first: release-id hits, then the rest by how well
/// their title names the album or the track. An item is left out when the backend never offers
/// its files, when it has no audio, or when its creator is not the wanted artist (a release-id
/// hit is kept whatever its creator, since names are romanised differently).
pub(crate) fn shortlist(found: Vec<Found>, wanted: &Wanted) -> Vec<Found> {
    let mut seen = std::collections::HashSet::new();
    let named = wanted
        .album
        .clone()
        .or_else(|| wanted.tracks.first().map(|t| t.title.clone()))
        .unwrap_or_default();
    let mut kept: Vec<(f64, Found)> = found
        .into_iter()
        .filter(|f| seen.insert(f.doc.identifier.clone()))
        .filter(|f| !f.doc.withheld() && has_audio(&f.doc))
        .filter(|f| {
            f.release_hit
                || artist_score(f.doc.creator.0.iter().map(String::as_str), &wanted.artist)
                    >= ARTIST_GATE
        })
        .map(|f| (name_score(f.doc.title.get().unwrap_or_default(), &named), f))
        .collect();
    // Stable: equal items keep archive.org's order, most downloaded first.
    kept.sort_by(|(a, fa), (b, fb)| {
        fb.release_hit
            .cmp(&fa.release_hit)
            .then_with(|| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal))
    });
    kept.into_iter().take(SHORTLIST).map(|(_, f)| f).collect()
}

/// `length` in seconds: archive.org writes originals' as seconds (`"320.36"`) and derivatives'
/// as `mm:ss` (`"05:20"`), and longer ones as `h:mm:ss`.
pub(crate) fn parse_length(length: &str) -> Option<f64> {
    let length = length.trim();
    if length.contains(':') {
        let parts: Vec<&str> = length.split(':').collect();
        if parts.len() > 3
            || parts
                .iter()
                .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        return Some(parts.iter().try_fold(0u64, |acc, p| {
            acc.checked_mul(60)?.checked_add(p.parse().ok()?)
        })? as f64);
    }
    length
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s > 0.0)
}

/// A track number tag: `"04"`, `"4"`, `"4/10"`.
fn track_number(track: &str) -> Option<u32> {
    let number = track.trim().split('/').next()?.trim();
    number.parse().ok().filter(|n| *n > 0)
}

/// An original audio file of an item the backend may offer.
#[derive(Debug, Clone)]
pub(crate) struct Original {
    pub name: String,
    pub extension: String,
    pub format: String,
    pub size: u64,
    pub md5: String,
    pub length_s: Option<f64>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
}

impl Original {
    /// The file's title from its name: the last segment, its extension and leading track
    /// number gone.
    fn stem_title(&self) -> String {
        let base = self.name.rsplit('/').next().unwrap_or(&self.name);
        let stem = base
            .rsplit_once('.')
            .map_or(base, |(stem, _)| stem)
            .replace('_', " ");
        clean_name(&RE_LEAD_NUMBER.replace(&stem, ""))
    }

    fn length_secs(&self) -> Option<u32> {
        self.length_s.map(|s| s.round() as u32)
    }

    /// A lossy file's bitrate from its size and length: archive.org labels every uploaded MP3
    /// "VBR MP3" with no rate.
    fn estimated_kbps(&self) -> Option<u32> {
        if matches!(self.extension.as_str(), "flac" | "wav") {
            return None;
        }
        let secs = self.length_s?;
        Some((self.size as f64 * 8.0 / secs / 1000.0).round() as u32)
    }

    fn quality_score(&self) -> f64 {
        format_score(&self.extension, self.estimated_kbps()).min(1.0)
    }
}

/// What an item's file list holds.
#[derive(Debug, Default)]
pub(crate) struct Sorted {
    pub offered: Vec<Original>,
    /// Original files with an audio extension, offered or not.
    pub audio: u32,
    /// Other original files.
    pub not_audio: u32,
}

/// The files of an item the backend may offer: its uploader's own audio files (never
/// archive.org's derivatives), not private, with a size and an md5 to check them by.
pub(crate) fn originals(files: &[ItemFile]) -> Sorted {
    let mut sorted = Sorted::default();
    for file in files.iter().filter(|f| f.source.get() == Some("original")) {
        let extension = file
            .name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        if !AUDIO_EXTENSIONS.contains(&extension.as_str()) {
            sorted.not_audio += 1;
            continue;
        }
        sorted.audio += 1;
        let size = file.size.get().and_then(|s| s.parse::<u64>().ok());
        let md5 = file
            .md5
            .get()
            .map(str::to_ascii_lowercase)
            .filter(|m| m.len() == 32 && m.bytes().all(|b| b.is_ascii_hexdigit()));
        let (Some(size), Some(md5)) = (size.filter(|s| *s > 0), md5) else {
            continue;
        };
        if file.private.get() == Some("true") {
            continue;
        }
        sorted.offered.push(Original {
            name: file.name.clone(),
            extension,
            format: file.format.get().unwrap_or_default().to_string(),
            size,
            md5,
            length_s: file.length.get().and_then(parse_length),
            title: file.title.get().map(str::to_string),
            artist: (file.artist.get().or(file.creator.get())).map(str::to_string),
            album: file.album.get().map(str::to_string),
            track: file.track.get().and_then(track_number),
        });
    }
    sorted
}

/// `file`'s score as the wanted track `index`, or None when it is not that track. `creators` and
/// `item_title` are its item's.
fn score_as(
    file: &Original,
    index: usize,
    track: &WantedTrack,
    wanted: &Wanted,
    creators: &[String],
    item_title: &str,
) -> Option<f64> {
    let wanted_title = CleanedText::new(&track.title);
    let tag = file
        .title
        .as_deref()
        .map_or(0.0, |t| title_score(t, &wanted_title));
    let stem = title_score(&file.stem_title(), &wanted_title);
    let named = file.title.clone().unwrap_or_else(|| file.stem_title());
    if version_veto(&named, &track.title) {
        return None;
    }
    let lengths = track.duration_s.zip(file.length_secs());
    if lengths.is_some_and(|(w, f)| w.abs_diff(f) >= LENGTH_LIMIT_S) {
        return None;
    }
    let number_agrees = wanted.is_album() && file.track == u32::try_from(index + 1).ok();
    let title = tag.max(stem);
    if title == 0.0
        && !(number_agrees && lengths.is_some_and(|(w, f)| w.abs_diff(f) <= NUMBER_ONLY_LENGTH_S))
    {
        return None;
    }
    let title = if number_agrees {
        title.max(NUMBER_CREDIT)
    } else {
        title
    };

    let mut sum = title * TITLE_WEIGHT;
    let mut weight = TITLE_WEIGHT;
    if let Some((w, f)) = lengths {
        sum += duration_score(w, f) * DURATION_WEIGHT;
        weight += DURATION_WEIGHT;
    }
    let artist = artist_score(
        file.artist
            .iter()
            .map(String::as_str)
            .chain(creators.iter().map(String::as_str)),
        &wanted.artist,
    );
    sum += artist * ARTIST_WEIGHT;
    weight += ARTIST_WEIGHT;
    if let Some(album) = &wanted.album {
        let score = name_score(item_title, album).max(
            file.album
                .as_deref()
                .map_or(0.0, |tag| name_score(tag, album)),
        );
        sum += score * ALBUM_WEIGHT;
        if score > ALBUM_INFO_THRESHOLD {
            weight += ALBUM_WEIGHT;
        }
    }
    Some(sum / weight)
}

/// The page of an item on archive.org.
pub(crate) fn item_url(identifier: &str) -> String {
    format!(
        "https://archive.org/details/{}",
        super::client::path_of(identifier)
    )
}

/// Why `score_as` returned `None` (pure diagnostics; scoring logic unchanged).
fn score_miss_reason(
    file: &Original,
    index: usize,
    track: &WantedTrack,
    wanted: &Wanted,
) -> &'static str {
    let named = file.title.clone().unwrap_or_else(|| file.stem_title());
    if version_veto(&named, &track.title) {
        return REASON_VERSION_VETO;
    }
    let lengths = track.duration_s.zip(file.length_secs());
    if lengths.is_some_and(|(w, f)| w.abs_diff(f) >= LENGTH_LIMIT_S) {
        return REASON_DURATION_DELTA;
    }
    let wanted_title = CleanedText::new(&track.title);
    let tag = file
        .title
        .as_deref()
        .map_or(0.0, |t| title_score(t, &wanted_title));
    let stem = title_score(&file.stem_title(), &wanted_title);
    let number_agrees = wanted.is_album() && file.track == u32::try_from(index + 1).ok();
    if tag.max(stem) == 0.0
        && !(number_agrees && lengths.is_some_and(|(w, f)| w.abs_diff(f) <= NUMBER_ONLY_LENGTH_S))
    {
        return REASON_TITLE_MISMATCH;
    }
    REASON_BELOW_THRESHOLD
}

/// One item as a group of the wanted tracks it has, how many of its offered files scored
/// under the floor, and bounded rejection diagnostics. A file under the floor is left out, or
/// kept and flagged `below_floor` when `keep_below_floor`; so is the whole group for an album
/// search when it has less than half the album. None when no file is kept.
pub(crate) fn group_with_diagnostics(
    found: &Found,
    sorted: &Sorted,
    wanted: &Wanted,
    keep_below_floor: bool,
) -> (Option<DownloadableGroup>, u32, RejectionCollector) {
    let doc = &found.doc;
    let item_title = doc.title.get().unwrap_or(&doc.identifier).to_string();
    let creators = &doc.creator.0;
    let files = &sorted.offered;
    // Every (file, track) score, None when the file is not the track.
    let scores: Vec<Vec<Option<f64>>> = files
        .iter()
        .map(|file| {
            (wanted.tracks.iter().enumerate())
                .map(|(i, track)| score_as(file, i, track, wanted, creators, &item_title))
                .collect()
        })
        .collect();
    let under = |s: f64| s < MIN_SCORE_THRESHOLD;
    let mut below_floor = 0u32;
    let mut rejections = RejectionCollector::default();
    for (file_idx, per_track) in scores.iter().enumerate() {
        if !per_track.iter().flatten().any(|s| !under(*s)) {
            below_floor += 1;
            let file = &files[file_idx];
            let best_score = per_track
                .iter()
                .flatten()
                .copied()
                .fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
            let (score, reason) = match best_score {
                Some(s) => (Some(s as f32), REASON_BELOW_THRESHOLD),
                None => {
                    let mut chosen = REASON_TITLE_MISMATCH;
                    for (i, track) in wanted.tracks.iter().enumerate() {
                        let r = score_miss_reason(file, i, track, wanted);
                        if r == REASON_VERSION_VETO {
                            chosen = r;
                            break;
                        }
                        if r == REASON_DURATION_DELTA {
                            chosen = r;
                        }
                    }
                    (None, chosen)
                }
            };
            rejections.record(doc.identifier.clone(), file.name.clone(), score, reason);
        }
    }
    let indexes: Vec<usize> = (0..files.len()).collect();
    let taken = assign_tracks(
        &wanted.tracks,
        &indexes,
        |&f, t, _| scores[f][t].filter(|s| keep_below_floor || !under(*s)),
        |&f| files[f].quality_score(),
    );
    let picked: Vec<(usize, usize, f64)> = (taken.into_iter().enumerate())
        .filter_map(|(t, took)| took.map(|(f, s)| (t, indexes[f], s)))
        .collect();
    if picked.is_empty() || wanted.tracks.is_empty() {
        return (None, below_floor, rejections);
    }
    let completeness = picked.len() as f64 / wanted.tracks.len() as f64;
    let short = wanted.is_album() && picked.len() * 2 < wanted.tracks.len();
    if short && !keep_below_floor {
        return (None, below_floor, rejections);
    }

    let of = ItemOf {
        found,
        item_title: &item_title,
        audio_files: sorted.audio,
        collections: doc.collections(),
    };
    let items: Vec<DownloadableItem> = picked
        .iter()
        .map(|&(t, f, score)| {
            of.item(
                &files[f],
                &wanted.tracks[t].title,
                score,
                short || under(score),
            )
        })
        .collect();
    let mean_match = picked.iter().map(|p| p.2).sum::<f64>() / picked.len() as f64;
    let mean_quality = items.iter().map(|i| i.quality_score).sum::<f64>() / items.len() as f64;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for i in &items {
        *counts.entry(i.quality.as_str()).or_default() += 1;
    }
    let quality = (counts.into_iter())
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(q, _)| q.to_string())
        .unwrap_or_default();
    let artist = (!creators.is_empty()).then(|| creators.join(", "));
    (
        Some(DownloadableGroup {
            source: doc.identifier.clone(),
            group_id: doc.identifier.clone(),
            title: item_title,
            artist,
            item_count: items.len(),
            total_size: items.iter().filter_map(|i| i.size).sum(),
            score: group_score(mean_match, completeness, mean_quality),
            quality,
            items,
        }),
        below_floor,
        rejections,
    )
}

#[cfg(test)]
pub(crate) fn group(
    found: &Found,
    sorted: &Sorted,
    wanted: &Wanted,
    keep_below_floor: bool,
) -> (Option<DownloadableGroup>, u32) {
    let (group, below_floor, _) = group_with_diagnostics(found, sorted, wanted, keep_below_floor);
    (group, below_floor)
}

/// What the items of one group share: its item and what is known of it.
struct ItemOf<'a> {
    found: &'a Found,
    item_title: &'a str,
    /// The item's original audio files.
    audio_files: u32,
    collections: Vec<String>,
}

impl ItemOf<'_> {
    /// `file` as the wanted track `wanted_title`, which it matched with `match_score`.
    fn item(
        &self,
        file: &Original,
        wanted_title: &str,
        match_score: f64,
        below_floor: bool,
    ) -> DownloadableItem {
        let doc = &self.found.doc;
        let mut backend_facts = serde_json::Map::new();
        backend_facts.insert("collections".into(), self.collections.clone().into());
        backend_facts.insert("format_label".into(), file.format.clone().into());
        if let Some(license) = doc.licenseurl.get() {
            backend_facts.insert("licenseurl".into(), license.into());
        }
        backend_facts.insert("mb_release_match".into(), self.found.release_hit.into());
        backend_facts.insert("item_url".into(), item_url(&doc.identifier).into());
        if let Some(downloads) = doc.downloads.get().and_then(|d| d.parse::<u64>().ok()) {
            backend_facts.insert("item_downloads".into(), downloads.into());
        }
        if let Some(kbps) = file.estimated_kbps() {
            backend_facts.insert("estimated_kbps".into(), kbps.into());
        }
        let data = ArchiveFile {
            identifier: doc.identifier.clone(),
            name: file.name.clone(),
            size: file.size,
            md5: file.md5.clone(),
        };
        DownloadableItem {
            id: format!("{}/{}", doc.identifier, file.name),
            backend: BACKEND_ID.to_string(),
            source: doc.identifier.clone(),
            // Failover pairs files of one track across sources by the wanted title.
            title: wanted_title.to_string(),
            artist: (file.artist.clone())
                .or_else(|| doc.creator.0.first().cloned())
                .unwrap_or_default(),
            album: (file.album.clone()).unwrap_or_else(|| self.item_title.to_string()),
            size: Some(file.size),
            duration: file.length_secs(),
            quality: file.extension.clone(),
            quality_score: file.quality_score(),
            backend_data: serde_json::to_string(&data).ok(),
            match_score,
            below_floor,
            facts: FileFacts {
                extension: Some(file.extension.clone()),
                folder_audio_files: Some(self.audio_files),
                source_queue_length: Some(0),
                backend_facts,
                ..FileFacts::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Files {
        result: Vec<ItemFile>,
    }

    #[derive(Deserialize)]
    struct Docs {
        response: DocList,
    }

    #[derive(Deserialize)]
    struct DocList {
        docs: Vec<Doc>,
    }

    fn files(json: &str) -> Vec<ItemFile> {
        serde_json::from_str::<Files>(json).unwrap().result
    }

    fn docs(json: &str) -> Vec<Doc> {
        serde_json::from_str::<Docs>(json).unwrap().response.docs
    }

    fn doc(json: &str, identifier: &str) -> Doc {
        docs(json)
            .into_iter()
            .find(|d| d.identifier == identifier)
            .unwrap()
    }

    const THE_SLIP_FILES: &str = include_str!("fixtures/files_the_slip.json");
    const S4: &str = include_str!("fixtures/s4_mbid_flac_dl.json");
    const S10: &str = include_str!("fixtures/s10_discovery.json");
    const DISCOVERY_FILES: &str = include_str!("fixtures/files_discovery-daft-punk-2001-flac.json");

    /// The Slip's tracklist as MusicBrainz has it.
    fn the_slip() -> Wanted {
        #[derive(Deserialize)]
        struct Release {
            id: String,
            title: String,
            artist: String,
            tracks: Vec<Track>,
        }
        #[derive(Deserialize)]
        struct Track {
            title: String,
            length: u32,
        }
        let release: Release =
            serde_json::from_str(include_str!("fixtures/mb_the_slip.json")).unwrap();
        Wanted {
            artist: release.artist,
            album: Some(release.title),
            tracks: (release.tracks.iter())
                .map(|t| WantedTrack {
                    title: t.title.clone(),
                    duration_s: Some((t.length + 500) / 1000),
                })
                .collect(),
            release: Some(release.id),
        }
    }

    /// The tracks an item's own tags name, in its files' order: a tracklist for the tests whose
    /// release MusicBrainz was not asked for.
    fn tagged(sorted: &Sorted) -> Vec<WantedTrack> {
        (sorted.offered.iter())
            .map(|f| WantedTrack {
                title: f.title.clone().unwrap(),
                duration_s: f.length_secs(),
            })
            .collect()
    }

    fn found(doc: Doc, release_hit: bool) -> Found {
        Found { doc, release_hit }
    }

    #[test]
    fn lengths_read_in_seconds_and_in_minutes() {
        assert_eq!(parse_length("320.36"), Some(320.36));
        assert_eq!(parse_length("05:20"), Some(320.0));
        assert_eq!(parse_length("1:05:20"), Some(3920.0));
        for bad in ["", "abc", ":20", "1:2:3:4", "-5", "0"] {
            assert_eq!(parse_length(bad), None, "{bad:?}");
        }
        assert_eq!(track_number("04"), Some(4));
        assert_eq!(track_number("4/10"), Some(4));
        assert_eq!(track_number("A1"), None);
    }

    #[test]
    fn only_an_uploaders_own_audio_files_are_offered() {
        let slip = originals(&files(THE_SLIP_FILES));
        assert_eq!(slip.offered.len(), 10);
        assert!(slip.offered.iter().all(|f| f.extension == "flac"));
        assert_eq!(slip.audio, 10);
        assert!(slip.not_audio >= 2, "the cover and the thumbnail");

        // Every original of the stream-only Grateful Dead tape is a private Shorten file.
        let gd = originals(&files(include_str!("fixtures/files_gd77-05-08.sbd.json")));
        assert!(gd.offered.is_empty());

        let mp3 = originals(&files(include_str!("fixtures/files_a0165062421_10.json")));
        assert_eq!(mp3.offered.len(), 17);
        assert!(mp3.offered.iter().all(|f| f.estimated_kbps() == Some(128)));

        let private: Vec<ItemFile> = serde_json::from_value(serde_json::json!([
            {"name": "a.flac", "source": "original", "size": "10", "md5": "1be92843604f79e3070a0cccd3145081", "private": "true"},
            {"name": "b.flac", "source": "original", "size": "0", "md5": "1be92843604f79e3070a0cccd3145081"},
            {"name": "c.flac", "source": "original", "size": "10"},
            {"name": "d.flac", "source": "derivative", "size": "10", "md5": "1be92843604f79e3070a0cccd3145081"},
            {"name": "e.aiff", "source": "original", "size": "10", "md5": "1be92843604f79e3070a0cccd3145081"},
        ]))
        .unwrap();
        let sorted = originals(&private);
        assert!(sorted.offered.is_empty());
        assert_eq!((sorted.audio, sorted.not_audio), (3, 1));
    }

    #[test]
    fn the_slip_matches_all_ten_tracks_under_musicbrainz_titles() {
        let wanted = the_slip();
        let sorted = originals(&files(THE_SLIP_FILES));
        let item = found(doc(S4, "nine_inch_nails_the_slip"), true);
        let (group, below) = group(&item, &sorted, &wanted, false);
        let group = group.unwrap();
        assert_eq!(below, 0);
        assert_eq!(group.items.len(), 10);
        let titles: Vec<&str> = group.items.iter().map(|i| i.title.as_str()).collect();
        let wanted_titles: Vec<&str> = wanted.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, wanted_titles);
        let discipline = &group.items[3];
        assert_eq!(
            (discipline.title.as_str(), discipline.id.as_str()),
            ("Discipline", "nine_inch_nails_the_slip/04_discipline.flac")
        );
        for item in &group.items {
            assert_eq!(item.backend, BACKEND_ID);
            assert!(!item.below_floor);
            assert!(item.match_score >= MIN_SCORE_THRESHOLD, "{item:?}");
            assert_eq!(item.facts.backend_facts["mb_release_match"], true);
            assert_eq!(
                item.facts.backend_facts["item_url"],
                "https://archive.org/details/nine_inch_nails_the_slip"
            );
            let data: ArchiveFile =
                serde_json::from_str(item.backend_data.as_deref().unwrap()).unwrap();
            assert_eq!(item.id, format!("{}/{}", data.identifier, data.name));
        }
        let mean = |f: fn(&DownloadableItem) -> f64| {
            group.items.iter().map(f).sum::<f64>() / group.items.len() as f64
        };
        assert_eq!(
            group.score,
            group_score(mean(|i| i.match_score), 1.0, mean(|i| i.quality_score))
        );
        assert_eq!(group.quality, "flac");
    }

    #[test]
    fn a_misspelt_title_matches_by_its_track_number_and_length() {
        let wanted = the_slip();
        let file = Original {
            name: "track4.flac".into(),
            extension: "flac".into(),
            format: "Flac".into(),
            size: 1,
            md5: "1be92843604f79e3070a0cccd3145081".into(),
            length_s: Some(259.18),
            title: Some("Descipline".into()),
            artist: Some("Nine Inch Nails".into()),
            album: Some("The Slip".into()),
            track: Some(4),
        };
        let creators = vec!["Nine Inch Nails".to_string()];
        let as_four = score_as(&file, 3, &wanted.tracks[3], &wanted, &creators, "The Slip");
        assert!(
            as_four.is_some_and(|s| s >= MIN_SCORE_THRESHOLD),
            "{as_four:?}"
        );
        // Another number, or a length far off, and the title alone does not match.
        assert_eq!(
            score_as(
                &Original {
                    track: Some(5),
                    ..file.clone()
                },
                3,
                &wanted.tracks[3],
                &wanted,
                &creators,
                "The Slip"
            ),
            None
        );
        assert_eq!(
            score_as(
                &Original {
                    length_s: Some(270.0),
                    ..file.clone()
                },
                3,
                &wanted.tracks[3],
                &wanted,
                &creators,
                "The Slip"
            ),
            None
        );
    }

    #[test]
    fn a_file_forty_seconds_longer_or_of_another_version_is_not_the_track() {
        let wanted = the_slip();
        let file = |title: &str, length| Original {
            name: "03_letting_you.flac".into(),
            extension: "flac".into(),
            format: "Flac".into(),
            size: 1,
            md5: "1be92843604f79e3070a0cccd3145081".into(),
            length_s: Some(length),
            title: Some(title.into()),
            artist: None,
            album: None,
            track: Some(3),
        };
        let creators = vec!["Nine Inch Nails".to_string()];
        let score =
            |f: &Original| score_as(f, 2, &wanted.tracks[2], &wanted, &creators, "The Slip");
        assert!(score(&file("Letting You", 229.5)).is_some());
        assert_eq!(score(&file("Letting You", 270.0)), None);
        assert_eq!(score(&file("Letting You (Live)", 229.5)), None);
    }

    #[test]
    fn an_uploaders_wrong_track_number_takes_nothing_away() {
        let sorted = originals(&files(include_str!("fixtures/files_dmp003.json")));
        let wanted = Wanted {
            artist: "Cymotec".into(),
            album: Some("Retrospective".into()),
            tracks: tagged(&sorted),
            release: None,
        };
        assert_eq!(sorted.offered[15].track, Some(23));
        let item = found(
            serde_json::from_value(serde_json::json!({
                "identifier": "dmp003",
                "title": "Cymotec - Retrospective",
                "creator": "Cymotec",
                "format": ["Flac", "VBR MP3"]
            }))
            .unwrap(),
            false,
        );
        let (group, _) = group(&item, &sorted, &wanted, false);
        let group = group.unwrap();
        assert_eq!(group.items.len(), 22);
        assert_eq!(
            (group.items[15].title.as_str(), group.items[15].id.as_str()),
            (
                "gnade der spaeten geburt",
                "dmp003/dmp003-16-gnade-der-spaeten-geburt.flac"
            )
        );
    }

    #[test]
    fn an_item_with_less_than_half_an_album_is_not_the_album() {
        let all = originals(&files(DISCOVERY_FILES));
        let wanted = Wanted {
            artist: "Daft Punk".into(),
            album: Some("Discovery".into()),
            tracks: tagged(&all),
            release: None,
        };
        assert_eq!(wanted.tracks.len(), 14);
        let five = Sorted {
            offered: all.offered[..5].to_vec(),
            audio: 5,
            not_audio: 0,
        };
        let item = found(doc(S10, "dp-discovery"), false);
        assert!(group(&item, &five, &wanted, false).0.is_none());
        let kept = group(&item, &five, &wanted, true).0.unwrap();
        assert_eq!(kept.items.len(), 5);
        assert!(kept.items.iter().all(|i| i.below_floor));
        let whole = group(&item, &all, &wanted, false).0.unwrap();
        assert_eq!(whole.items.len(), 14);
        assert!(whole.score > 0.9, "{}", whole.score);
    }

    #[test]
    fn the_shortlist_leaves_out_items_without_audio_or_by_another_artist_and_puts_release_hits_first(
    ) {
        let wanted = Wanted {
            artist: "Daft Punk".into(),
            album: Some("Discovery".into()),
            tracks: vec![],
            release: Some("c7ba938a-d3b3-4eee-9fc9-8c51eec86f0a".into()),
        };
        let mut items: Vec<Found> = docs(S10).into_iter().map(|d| found(d, false)).collect();
        // A release-id hit by another name is kept, and comes first.
        items.push(found(
            doc(
                include_str!("fixtures/s5_mbid_exact.json"),
                "ephemeris-h.u.v.a.-network-dissolving-time",
            ),
            true,
        ));
        items.push(found(doc(S4, "saw01x2"), false));
        let kept: Vec<String> = shortlist(items, &wanted)
            .into_iter()
            .map(|f| f.doc.identifier)
            .collect();
        assert_eq!(kept[0], "ephemeris-h.u.v.a.-network-dissolving-time");
        assert!(
            !kept.contains(&"discovery-daftpunk".to_string()),
            "a ZIP only: {kept:?}"
        );
        assert!(
            !kept.contains(&"14-too-long_202602".to_string()),
            "AIFF only: {kept:?}"
        );
        assert!(!kept.contains(&"saw01x2".to_string()), "brainsaw: {kept:?}");
        assert!(
            kept.contains(&"discovery_202507".to_string()),
            "MPEG-4 audio: {kept:?}"
        );
        assert!(
            kept.contains(&"2000-discovery-beta-version".to_string()),
            "{kept:?}"
        );
        assert!(kept.len() <= SHORTLIST);
    }

    #[test]
    fn a_lossy_file_carries_its_estimated_bitrate_and_a_search_spec_names_its_target() {
        let sorted = originals(&files(include_str!("fixtures/files_a0165062421_10.json")));
        let original = &sorted.offered[0];
        assert_eq!(original.stem_title(), "Moon Age");
        assert!((original.quality_score() - format_score("mp3", Some(128))).abs() < 1e-12);

        let track = shared::metadata::Track {
            id: "t".into(),
            title: "Letting You".into(),
            artist: "Nine Inch Nails".into(),
            album_id: None,
            album_title: Some("The Slip".into()),
            release_date: None,
            duration: Some("3:49".into()),
            mbid: None,
            release_mbid: Some("12b57d46-a192-499e-a91f-7da66790a1c1".into()),
        };
        let wanted = Wanted::of(&SearchSpec::of(None, vec![track.clone()])).unwrap();
        assert_eq!(
            wanted.target(),
            Target::Album {
                artist: "Nine Inch Nails",
                album: "The Slip"
            }
        );
        assert_eq!(
            wanted.release.as_deref(),
            Some("12b57d46-a192-499e-a91f-7da66790a1c1")
        );
        let alone = Wanted::of(&SearchSpec::of(
            None,
            vec![shared::metadata::Track {
                album_title: None,
                ..track
            }],
        ))
        .unwrap();
        assert_eq!(
            alone.target(),
            Target::Track {
                artist: "Nine Inch Nails",
                title: "Letting You"
            }
        );
    }
}
