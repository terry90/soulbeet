//! Shared canonical text normalization, version/variant classification, karaoke/tribute
//! filtering, and compilation detection for catalog search and local-vs-remote deduplication.
//!
//! Shared across server (Deezer + MusicBrainz), `client-core` (Desktop + Android), and Web WASM
//! so every surface agrees on which tracks/albums are duplicates.

use crate::recommendation::discovery_name;

/// Semantic version kind of a track recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackVariantKind {
    Studio,
    Live,
    Remix,
    Acoustic,
    Demo,
    Instrumental,
    Karaoke,
}

impl TrackVariantKind {
    /// Short human-readable badge label for non-studio variants shown in UI.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Studio => None,
            Self::Live => Some("Live"),
            Self::Remix => Some("Remix"),
            Self::Acoustic => Some("Acoustic"),
            Self::Demo => Some("Demo"),
            Self::Instrumental => Some("Instrumental"),
            Self::Karaoke => Some("Karaoke"),
        }
    }
}

/// Which non-studio variants the user's query explicitly requested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryVariantIntent {
    pub live: bool,
    pub remix: bool,
    pub acoustic: bool,
    pub demo: bool,
    pub instrumental: bool,
    pub karaoke: bool,
    pub cover: bool,
}

impl QueryVariantIntent {
    /// Inspects the user's search query for explicit variant keywords (word-boundary matched).
    pub fn from_query(query: &str) -> Self {
        let folded = fold_search_text(query);
        let words: Vec<&str> = folded.split_whitespace().collect();
        let has = |w: &str| words.contains(&w);
        Self {
            live: has("live") || has("unplugged") || has("concert") || has("session"),
            remix: has("remix") || has("mix") || has("edit") || has("bootleg") || has("mashup"),
            acoustic: has("acoustic") || has("unplugged") || has("piano") || has("stripped"),
            demo: has("demo") || has("rehearsal"),
            instrumental: has("instrumental") || has("backing"),
            karaoke: has("karaoke") || has("tribute"),
            cover: has("cover") || has("tribute"),
        }
    }

    pub fn allows(&self, kind: TrackVariantKind) -> bool {
        match kind {
            TrackVariantKind::Studio => true,
            TrackVariantKind::Live => self.live,
            TrackVariantKind::Remix => self.remix,
            TrackVariantKind::Acoustic => self.acoustic,
            TrackVariantKind::Demo => self.demo,
            TrackVariantKind::Instrumental => self.instrumental,
            TrackVariantKind::Karaoke => self.karaoke || self.cover,
        }
    }
}

/// Folds text for search matching and deduplication:
/// - converts dashes/slashes/bullets into spaces (so `"Lumière-The Departure"` == `"Lumière - The Departure"`)
/// - strips diacritics/combining marks (`"Blue Öyster Cult"` -> `"blue oyster cult"`)
/// - strips apostrophes/punctuation without inserting gaps (`"Don’t"` -> `"dont"`, `"(Don't Fear)"` -> `"dont fear"`)
/// - collapses whitespace and lowercases.
pub fn fold_search_text(s: &str) -> String {
    let spaced: String = s
        .chars()
        .map(|c| match c {
            '-' | '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}'
            | '/' | '\u{2022}' | '\u{00B7}' | '_' => ' ',
            _ => c,
        })
        .collect();
    discovery_name(&spaced)
}

/// Extracts parenthetical / bracketed / dash-suffix qualifier segments from a title.
/// Example: `"Watermark (Remastered 2009)"` -> `["remastered 2009"]`.
/// Example: `"(Don't Fear) The Reaper"` -> `[]` (leading parenthetical is part of the song title).
fn trailing_qualifiers(title: &str) -> Vec<String> {
    let mut out = Vec::new();
    let trimmed = title.trim();

    // Parenthetical and bracketed groups that do NOT start at index 0 (a leading group like
    // "(Don't Fear) The Reaper" or "(I Can't Get No) Satisfaction" is part of the core title).
    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (byte_idx, ch) = chars[i];
        let close = match ch {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(close_ch) = close {
            let mut j = i + 1;
            while j < chars.len() && chars[j].1 != close_ch {
                j += 1;
            }
            if j < chars.len() {
                if byte_idx > 0 {
                    let inner = &trimmed[byte_idx + ch.len_utf8()..chars[j].0];
                    let folded = fold_search_text(inner);
                    if !folded.is_empty() {
                        out.push(folded);
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    // Trailing dash suffix: "Title - Live at ...", "Title - 2009 Remaster"
    for sep in [" - ", " \u{2010} ", " \u{2013} ", " \u{2014} "] {
        if let Some((_, suffix)) = trimmed.rsplit_once(sep) {
            let folded = fold_search_text(suffix);
            if !folded.is_empty() {
                out.push(folded);
            }
            break;
        }
    }

    out
}

/// Returns true if a qualifier segment is purely a non-semantic edition/remaster/format tag
/// that should be stripped when forming a canonical title key.
fn is_edition_only_qualifier(q: &str) -> bool {
    let words: Vec<&str> = q.split_whitespace().collect();
    if words.is_empty() {
        return true;
    }
    let has = |w: &str| words.contains(&w);
    // Remaster / edition / mix-format / soundtrack attribution / featuring tags
    if has("remaster")
        || has("remastered")
        || has("deluxe")
        || has("explicit")
        || has("clean")
        || has("mono")
        || has("stereo")
        || has("bonus")
        || has("anniversary")
        || has("reissue")
        || q.starts_with("feat ")
        || q.starts_with("ft ")
        || q.starts_with("featuring ")
        || q.starts_with("from ")
        || q == "album version"
        || q == "single version"
        || q == "radio edit"
        || q == "original mix"
        || q == "original version"
        || q == "extended version"
        || q == "short version"
        || q == "lp version"
        || q == "digital remaster"
        || q == "5 1 mix"
        || q == "tv mix"
        || q.ends_with(" remaster")
        || q.ends_with(" remastered")
        || q.ends_with(" edition")
        || q.ends_with(" version") && (has("album") || has("single") || has("radio") || has("original"))
    {
        // Do NOT strip if it actually denotes a distinct variant!
        if is_live_qualifier(q)
            || is_remix_qualifier(q)
            || is_acoustic_qualifier(q)
            || is_demo_qualifier(q)
            || is_instrumental_qualifier(q)
            || is_karaoke_qualifier(q)
        {
            return false;
        }
        return true;
    }
    false
}

fn ends_with_live_marker(s: &str) -> bool {
    s.ends_with(" live")
        && !s.ends_with("long live")
        && !s.ends_with("night live")
        && !s.ends_with("to live")
}

fn is_live_qualifier(q: &str) -> bool {
    let words: Vec<&str> = q.split_whitespace().collect();
    let has = |w: &str| words.contains(&w);
    q == "live"
        || q.starts_with("live ")
        || ends_with_live_marker(q)
        || q.contains(" recorded live")
        || q.starts_with("recorded live")
        || q.contains("live at ")
        || q.contains("live in ")
        || q.contains("live on ")
        || q.contains("live from ")
        || q.contains("in concert")
        || q.contains("ao vivo")
        || q.contains("en vivo")
        || has("unplugged")
        || q.contains("bbc session")
        || q.contains("peel session")
        || q.contains("tiny desk")
        || q.contains("live version")
}

fn is_remix_qualifier(q: &str) -> bool {
    let words: Vec<&str> = q.split_whitespace().collect();
    words.contains(&"remix")
        || words.contains(&"rmx")
        || words.contains(&"vip")
        || words.contains(&"bootleg")
        || words.contains(&"mashup")
        || q.contains("dj mix")
        || q.contains("club mix")
        || q.contains("dub mix")
        || q.contains("rework")
        || q.contains("sped up")
        || q.contains("slowed")
}

fn is_acoustic_qualifier(q: &str) -> bool {
    let words: Vec<&str> = q.split_whitespace().collect();
    words.contains(&"acoustic")
        || q.contains("piano version")
        || q.contains("stripped")
        || q.contains("orchestral version")
}

fn is_demo_qualifier(q: &str) -> bool {
    let words: Vec<&str> = q.split_whitespace().collect();
    words.contains(&"demo")
        || q.contains("rehearsal")
        || q.contains("early take")
        || q.contains("alternate take")
        || q.contains("work in progress")
}

fn is_instrumental_qualifier(q: &str) -> bool {
    let words: Vec<&str> = q.split_whitespace().collect();
    words.contains(&"instrumental")
        || q.contains("backing track")
        || q.contains("minus one")
}

fn is_karaoke_qualifier(q: &str) -> bool {
    let words: Vec<&str> = q.split_whitespace().collect();
    words.contains(&"karaoke")
        || q.contains("in the style of")
        || q.contains("made famous by")
        || q.contains("originally performed by")
        || q.contains("tribute to")
        || q.contains("cover version")
        || q.ends_with(" cover")
        || q == "cover"
        || q.contains("demo vocal")
        || q.contains("8 bit")
        || q.contains("lullaby version")
        || q.contains("lullaby rendition")
        || q.contains("ringtone")
}

/// Returns true if a folded full album title unambiguously indicates a karaoke / tribute /
/// lullaby-rendition / backing-track release. Intentionally narrower than `is_karaoke_qualifier`
/// so legitimate album titles like `"Lullaby"`, `"Lullaby of Birdland"`, `"Under Cover"`, and
/// `"Deep Cover"` are never misclassified.
pub fn is_karaoke_album_title(folded_album: &str) -> bool {
    let words: Vec<&str> = folded_album.split_whitespace().collect();
    words.contains(&"karaoke")
        || folded_album.contains("in the style of")
        || folded_album.contains("made famous by")
        || folded_album.contains("originally performed by")
        || folded_album.contains("tribute to ")
        || folded_album.contains("lullaby rendition")
        || folded_album.contains("lullaby version")
        || folded_album.contains("rockabye baby")
        || folded_album.contains("8 bit")
        || folded_album.contains("backing track")
        || folded_album.contains("demo vocal")
        || folded_album.contains("cover version")
        || folded_album.contains("ringtone")
}

/// Returns true if `query` explicitly targets `artist`: either an exact folded match
/// (for single- or multi-word names) or a whole-word phrase match of the full
/// `canonical_artist_key(artist)` inside `fold_search_text(query)` (for multi-word names).
///
/// Used so directly searching `"Vitamin String Quartet"`, `"Rockabye Baby"`, or `"Sing King"`
/// bypasses `is_karaoke_or_tribute_artist` filtering across tracks, albums, and artists.
pub fn query_targets_artist(query: &str, artist: &str) -> bool {
    let fq = fold_search_text(query);
    let fa = canonical_artist_key(artist);
    if fq.is_empty() || fa.is_empty() {
        return false;
    }
    if fq == fa {
        return true;
    }
    let a_words: Vec<&str> = fa.split_whitespace().collect();
    if a_words.len() < 2 {
        return false;
    }
    let q_words: Vec<&str> = fq.split_whitespace().collect();
    if a_words.len() > q_words.len() {
        return false;
    }
    q_words
        .windows(a_words.len())
        .any(|w| w == a_words.as_slice())
}

/// Returns true if an artist name is a known karaoke/tribute farm name.
pub fn is_karaoke_or_tribute_artist(artist: &str) -> bool {
    let f = fold_search_text(artist);
    f.contains("karaoke")
        || f.contains("tribute band")
        || f.contains("tribute to ")
        || f.ends_with(" tribute")
        || f.contains("made famous")
        || f.contains("in the style of")
        || f.contains("acoustified hits")
        || f.contains("prosource")
        || f.contains("ameritz")
        || f.contains("party tyme")
        || f.contains("sing king")
        || f.contains("hit crew")
        || f.contains("vitamin string quartet")
        || f.contains("rockabye baby")
        || f.contains("lullaby baby")
        || f.contains("piano tribute")
        || f.contains("8 bit arcade")
        || f.contains("studio allstars")
        || f.contains("music box mania")
}

/// Classifies a track into its semantic variant kind using ONLY parenthetical/bracketed/dash
/// qualifiers, artist karaoke markers, album markers, and MusicBrainz disambiguation.
///
/// Crucially avoids naive substring checks on the core title so songs like
/// `"Stayin' Alive"`, `"Live Well"`, `"Stay Alive"`, `"They Live in You"`, and `"Saliverne"`
/// always classify as `TrackVariantKind::Studio`.
pub fn classify_track_variant(
    artist: &str,
    title: &str,
    album_title: Option<&str>,
    disambiguation: Option<&str>,
    is_live_release_only: bool,
) -> TrackVariantKind {
    if is_karaoke_or_tribute_artist(artist) {
        return TrackVariantKind::Karaoke;
    }

    let qualifiers = trailing_qualifiers(title);
    for q in &qualifiers {
        if is_karaoke_qualifier(q) {
            return TrackVariantKind::Karaoke;
        }
    }
    if let Some(alb) = album_title {
        let fa = fold_search_text(alb);
        if is_karaoke_album_title(&fa) {
            return TrackVariantKind::Karaoke;
        }
    }
    if let Some(d) = disambiguation {
        let fd = fold_search_text(d);
        if is_karaoke_qualifier(&fd) {
            return TrackVariantKind::Karaoke;
        }
    }

    for q in &qualifiers {
        if is_remix_qualifier(q) {
            return TrackVariantKind::Remix;
        }
    }
    for q in &qualifiers {
        if is_acoustic_qualifier(q) {
            return TrackVariantKind::Acoustic;
        }
    }
    for q in &qualifiers {
        if is_demo_qualifier(q) {
            return TrackVariantKind::Demo;
        }
    }
    for q in &qualifiers {
        if is_instrumental_qualifier(q) {
            return TrackVariantKind::Instrumental;
        }
    }
    for q in &qualifiers {
        if is_live_qualifier(q) {
            return TrackVariantKind::Live;
        }
    }

    if let Some(d) = disambiguation {
        let fd = fold_search_text(d);
        if is_remix_qualifier(&fd) {
            return TrackVariantKind::Remix;
        }
        if is_acoustic_qualifier(&fd) {
            return TrackVariantKind::Acoustic;
        }
        if is_demo_qualifier(&fd) {
            return TrackVariantKind::Demo;
        }
        if is_instrumental_qualifier(&fd) {
            return TrackVariantKind::Instrumental;
        }
        if is_live_qualifier(&fd) {
            return TrackVariantKind::Live;
        }
    }

    if is_live_release_only {
        return TrackVariantKind::Live;
    }

    if let Some(alb) = album_title {
        let fa = fold_search_text(alb);
        if is_live_album_title(&fa) {
            return TrackVariantKind::Live;
        }
    }

    TrackVariantKind::Studio
}

/// Returns true if an album title strongly indicates a live concert album.
pub fn is_live_album_title(folded_album: &str) -> bool {
    folded_album == "in concert"
        || folded_album.starts_with("live at ")
        || folded_album.starts_with("live in ")
        || folded_album.starts_with("live from ")
        || folded_album.starts_with("live on ")
        || folded_album.starts_with("live ")
            && folded_album
                .split_whitespace()
                .nth(1)
                .is_some_and(|w| w.chars().all(|c| c.is_ascii_digit()))
        || folded_album.contains(" live at ")
        || folded_album.contains(" live in ")
        || folded_album.contains(" live a l")
        || folded_album.contains(" mtv unplugged")
        || folded_album == "mtv unplugged"
        || ends_with_live_marker(folded_album)
        || folded_album.ends_with(" unplugged")
        || folded_album.contains("extended versions")
}

/// Returns true if an album title strongly indicates a compilation / greatest-hits / VA sampler.
pub fn is_compilation_album_title(album_title: &str) -> bool {
    let f = fold_search_text(album_title);
    if f.is_empty() {
        return false;
    }
    f.starts_with("the essential ")
        || f.starts_with("essential ")
        || f.starts_with("the ultimate ")
        || f.starts_with("ultimate ")
        || f.starts_with("the best of ")
        || f.starts_with("best of ")
        || f.starts_with("greatest hits")
        || f.contains(" greatest hits")
        || f.starts_with("the very best of ")
        || f.starts_with("very best of ")
        || f.starts_with("now thats what i call")
        || f.contains("classic rock")
        || f.contains("rock classics")
        || f.contains("rock anthems")
        || f.contains("guitar anthems")
        || f.contains("halloween classics")
        || f.contains("dad songs")
        || f.contains("worlds best dad")
        || f.contains("40 shots of rock")
        || f.contains("hit giganten")
        || f.contains("sommerhits")
        || f.contains("car songs")
        || f.contains("driving rock")
        || f.contains("gold collection")
        || f.contains("anthology")
        || f.contains("the collection")
        || f.ends_with(" collection")
        || f.contains("number ones")
        || f.contains("all time classics")
        || f.contains("70s rock")
        || f.contains("80s rock")
        || f.contains("90s rock")
        || f.contains("hits of the")
        || f.contains("100 hits")
        || f.contains("top hits")
        || f.contains("rock hits")
        || f.contains("party mix")
}

/// Strips non-semantic edition suffixes (`(Remastered 2009)`, `(Deluxe Edition)`, `- 2011 Remaster`,
/// `(feat. ...)`, `(Album Version)`, etc.) and returns the folded canonical title key.
///
/// Keeps leading parentheticals like `"(Don't Fear) The Reaper"` so both
/// `"(Don't Fear) The Reaper"` and `"Don’t Fear the Reaper"` normalize identically to
/// `"dont fear the reaper"`.
pub fn canonical_title_key(title: &str) -> String {
    let mut cleaned = strip_edition_suffixes(title);
    // If stripping removed everything (pathological title), fall back to full title.
    if cleaned.trim().is_empty() {
        cleaned = title.to_string();
    }
    fold_search_text(&cleaned)
}

/// Strips edition-only parenthetical/bracketed/dash suffixes from a title while keeping
/// meaningful song title words and distinct variant qualifiers.
fn strip_edition_suffixes(title: &str) -> String {
    let trimmed = title.trim();
    let mut s = String::with_capacity(trimmed.len());
    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut i = 0;

    while i < chars.len() {
        let (byte_idx, ch) = chars[i];
        let close = match ch {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(close_ch) = close {
            let mut j = i + 1;
            while j < chars.len() && chars[j].1 != close_ch {
                j += 1;
            }
            if j < chars.len() {
                if byte_idx > 0 {
                    let inner = &trimmed[byte_idx + ch.len_utf8()..chars[j].0];
                    let folded = fold_search_text(inner);
                    if is_edition_only_qualifier(&folded) {
                        i = j + 1;
                        continue;
                    }
                }
                // Keep this group's text
                s.push_str(&trimmed[byte_idx..chars[j].0 + close_ch.len_utf8()]);
                i = j + 1;
                continue;
            }
        }
        s.push(ch);
        i += 1;
    }

    // Strip trailing dash edition suffix if edition-only (e.g. "Watermark - 2009 Remaster")
    let mut res = s.trim().to_string();
    for sep in [" - ", " \u{2010} ", " \u{2013} ", " \u{2014} "] {
        if let Some((head, tail)) = res.rsplit_once(sep) {
            let folded_tail = fold_search_text(tail);
            if is_edition_only_qualifier(&folded_tail) && !head.trim().is_empty() {
                res = head.trim().to_string();
                break;
            }
        }
    }
    res
}

/// Strips variant qualifiers (`(Live ...)`, `(Acoustic)`, `(Remix)`, etc.) AND edition suffixes
/// to obtain the bare song key used to group variants under their parent studio song.
pub fn base_song_title_key(title: &str) -> String {
    let trimmed = title.trim();
    let mut s = String::with_capacity(trimmed.len());
    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut i = 0;

    while i < chars.len() {
        let (byte_idx, ch) = chars[i];
        let close = match ch {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(close_ch) = close {
            let mut j = i + 1;
            while j < chars.len() && chars[j].1 != close_ch {
                j += 1;
            }
            if j < chars.len() {
                if byte_idx > 0 {
                    let inner = &trimmed[byte_idx + ch.len_utf8()..chars[j].0];
                    let folded = fold_search_text(inner);
                    if is_edition_only_qualifier(&folded)
                        || is_live_qualifier(&folded)
                        || is_remix_qualifier(&folded)
                        || is_acoustic_qualifier(&folded)
                        || is_demo_qualifier(&folded)
                        || is_instrumental_qualifier(&folded)
                        || is_karaoke_qualifier(&folded)
                    {
                        i = j + 1;
                        continue;
                    }
                }
                s.push_str(&trimmed[byte_idx..chars[j].0 + close_ch.len_utf8()]);
                i = j + 1;
                continue;
            }
        }
        s.push(ch);
        i += 1;
    }

    let mut res = s.trim().to_string();
    for sep in [" - ", " \u{2010} ", " \u{2013} ", " \u{2014} "] {
        if let Some((head, tail)) = res.rsplit_once(sep) {
            let folded_tail = fold_search_text(tail);
            if (is_edition_only_qualifier(&folded_tail)
                || is_live_qualifier(&folded_tail)
                || is_remix_qualifier(&folded_tail)
                || is_acoustic_qualifier(&folded_tail)
                || is_demo_qualifier(&folded_tail)
                || is_instrumental_qualifier(&folded_tail)
                || is_karaoke_qualifier(&folded_tail))
                && !head.trim().is_empty()
            {
                res = head.trim().to_string();
                break;
            }
        }
    }
    let key = fold_search_text(&res);
    if key.is_empty() {
        fold_search_text(title)
    } else {
        key
    }
}

/// Canonical primary artist key: strips trailing `feat.` / `ft.` / `featuring` credits and folds.
pub fn canonical_artist_key(artist: &str) -> String {
    let lower = artist.trim();
    let folded = fold_search_text(lower);
    for marker in [" feat ", " ft ", " featuring ", " with "] {
        if let Some((primary, _)) = folded.split_once(marker) {
            let p = primary.trim();
            if !p.is_empty() {
                return p.to_string();
            }
        }
    }
    folded
}

/// Canonical album title key: strips edition suffixes like `(Deluxe Version)`, `(2009 Remaster)`,
/// `(Édition Deluxe)`, `(Original Soundtrack)`? Note: keeps `Original Soundtrack` so OSTs stay
/// distinct from non-OST albums, while stripping `(Deluxe)`, `(Remastered)`, `(Anniversary Edition)`.
pub fn canonical_album_key(album_title: &str) -> String {
    canonical_title_key(album_title)
}

/// Normalized track key for local-vs-remote deduplication: `(canonical_artist_key, canonical_title_key)`.
/// Also matches when the title is normalized down to `base_song_title_key` if both are Studio.
pub fn track_dedup_key(artist: &str, title: &str) -> (String, String) {
    (canonical_artist_key(artist), canonical_title_key(title))
}

/// Normalized album key for local-vs-remote deduplication: `(canonical_artist_key, canonical_album_key)`.
pub fn album_dedup_key(artist: &str, album_title: &str) -> (String, String) {
    (canonical_artist_key(artist), canonical_album_key(album_title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_reaper_variants_identically() {
        let a = canonical_title_key("(Don't Fear) The Reaper");
        let b = canonical_title_key("Don’t Fear the Reaper");
        let c = canonical_title_key("(Don’t) Fear the Reaper (Remastered)");
        let d = canonical_title_key("(Don't Fear) The Reaper - 2001 Remaster");
        assert_eq!(a, "dont fear the reaper");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a, d);
    }

    #[test]
    fn normalizes_accents_and_umlauts_from_prod_library() {
        assert_eq!(
            canonical_artist_key("Blue Öyster Cult"),
            canonical_artist_key("Blue Oyster Cult")
        );
        assert_eq!(
            canonical_artist_key("Hermanos Gutiérrez"),
            canonical_artist_key("Hermanos Gutierrez")
        );
        assert_eq!(
            canonical_artist_key("Gealdýr"),
            canonical_artist_key("Gealdyr")
        );
        assert_eq!(
            canonical_artist_key("SKÁLD"),
            canonical_artist_key("SKALD")
        );
        assert_eq!(
            canonical_title_key("What’s Up?"),
            canonical_title_key("What's Up")
        );
        assert_eq!(
            canonical_title_key("Watermark (Remastered 2009)"),
            canonical_title_key("Watermark")
        );
        assert_eq!(
            canonical_artist_key("Kelly Rowland feat. Lil Wayne"),
            canonical_artist_key("Kelly Rowland")
        );
    }

    #[test]
    fn never_misclassifies_false_positive_live_songs() {
        for (artist, title, album) in [
            ("Bee Gees", "Stayin' Alive", Some("Saturday Night Fever")),
            ("Palace", "Live Well", Some("So Long Forever")),
            ("José González", "Stay Alive", Some("Stay Alive")),
            ("Samuel E. Wright", "They Live in You", Some("The Lion King")),
            ("Ours Samplus", "Saliverne", Some("One Day One Beat")),
        ] {
            assert_eq!(
                classify_track_variant(artist, title, album, None, false),
                TrackVariantKind::Studio,
                "false positive on {artist} - {title}"
            );
        }
    }

    #[test]
    fn classifies_true_variants_and_karaoke_spam() {
        assert_eq!(
            classify_track_variant(
                "ProSource Karaoke",
                "(Don't Fear) The Reaper (In the Style of Blue Oyster Cult)",
                None,
                None,
                false
            ),
            TrackVariantKind::Karaoke
        );
        assert_eq!(
            classify_track_variant(
                "Acoustified Hits",
                "Don't Fear the Reaper (Acoustic Version) [Blue Öyster Cult Cover]",
                None,
                None,
                false
            ),
            TrackVariantKind::Karaoke
        );
        assert_eq!(
            classify_track_variant(
                "Alain Bashung",
                "Résidents de la République (Live à l'Olympia / 2008)",
                Some("Live à l'Olympia (Live / 2008)"),
                None,
                false
            ),
            TrackVariantKind::Live
        );
        assert_eq!(
            classify_track_variant(
                "OMI",
                "Cheerleader (Felix Jaehn Remix) (Radio Edit)",
                Some("Cheerleader (Felix Jaehn Remix) (Radio Edit)"),
                None,
                false
            ),
            TrackVariantKind::Remix
        );
        assert_eq!(
            classify_track_variant(
                "Serj Tankian",
                "Empty Walls (acoustic)",
                Some("Elect the Dead"),
                None,
                false
            ),
            TrackVariantKind::Acoustic
        );
        assert_eq!(
            classify_track_variant(
                "Blue Öyster Cult",
                "(Don’t Fear) The Reaper (demo)",
                Some("Agents of Fortune"),
                None,
                false
            ),
            TrackVariantKind::Demo
        );
    }

    #[test]
    fn base_song_title_groups_variants_with_studio() {
        assert_eq!(
            base_song_title_key("Résidents de la République (Live à l'Olympia / 2008)"),
            "residents de la republique"
        );
        assert_eq!(
            base_song_title_key("Empty Walls (acoustic)"),
            "empty walls"
        );
        assert_eq!(
            base_song_title_key("(Don't Fear) The Reaper (demo)"),
            "dont fear the reaper"
        );
    }

    #[test]
    fn detects_compilation_album_titles() {
        assert!(is_compilation_album_title("The Essential Blue Öyster Cult"));
        assert!(is_compilation_album_title(
            "Halloween Classics: Hellbent For Halloween"
        ));
        assert!(is_compilation_album_title("Worlds Best Dad 2006"));
        assert!(is_compilation_album_title("The Ultimate Bee Gees"));
        assert!(!is_compilation_album_title("Agents of Fortune"));
        assert!(!is_compilation_album_title(
            "Clair Obscur: Expedition 33 (Original Soundtrack)"
        ));
        assert!(!is_compilation_album_title("One Day One Beat"));
    }

    #[test]
    fn query_intent_respects_word_boundaries() {
        assert!(!QueryVariantIntent::from_query("Bee Gees Stayin Alive").live);
        assert!(!QueryVariantIntent::from_query("Ours Samplus Saliverne").live);
        assert!(QueryVariantIntent::from_query("Blue Oyster Cult The Reaper live").live);
        assert!(QueryVariantIntent::from_query("OMI Cheerleader remix").remix);
        assert!(QueryVariantIntent::from_query("Serj Tankian Empty Walls acoustic").acoustic);
    }

    #[test]
    fn never_misclassifies_legitimate_lullaby_cover_or_long_live_tracks() {
        for (artist, title, album) in [
            ("The Cure", "Lullaby", Some("Lullaby")),
            ("The Cure", "Lullaby", Some("Disintegration")),
            ("Sarah Vaughan", "Lullaby of Birdland", Some("Lullaby of Birdland")),
            ("Motörhead", "Under Cover", Some("Under Cover")),
            ("Ozzy Osbourne", "Under Cover", Some("Under Cover")),
            ("Dr. Dre", "Deep Cover", Some("Deep Cover")),
            ("Taylor Swift", "Long Live", Some("Long Live")),
            ("Taylor Swift", "Long Live", Some("Speak Now")),
            ("Various", "Theme", Some("Saturday Night Live")),
            ("Artist", "Song", Some("A Time to Live")),
        ] {
            assert_eq!(
                classify_track_variant(artist, title, album, None, false),
                TrackVariantKind::Studio,
                "false positive on {artist} - {title} ({album:?})"
            );
        }

        // Unambiguous karaoke/lullaby-rendition albums and qualifiers still classify as Karaoke.
        assert_eq!(
            classify_track_variant(
                "Some Studio",
                "Enter Sandman",
                Some("Lullaby Renditions of Metallica"),
                None,
                false
            ),
            TrackVariantKind::Karaoke
        );
        assert_eq!(
            classify_track_variant(
                "Some Studio",
                "Enter Sandman (Lullaby Version)",
                Some("Sleepy Time"),
                None,
                false
            ),
            TrackVariantKind::Karaoke
        );
        assert!(is_live_album_title("extraterrestrial live"));
        assert!(!is_live_album_title("long live"));
        assert!(!is_live_album_title("saturday night live"));
        assert!(!is_live_album_title("a time to live"));
    }

    #[test]
    fn query_targets_artist_matches_explicit_tribute_searches_only() {
        assert!(query_targets_artist(
            "Vitamin String Quartet",
            "Vitamin String Quartet"
        ));
        assert!(query_targets_artist(
            "Vitamin String Quartet Bohemian Rhapsody",
            "Vitamin String Quartet"
        ));
        assert!(query_targets_artist("Rockabye Baby", "Rockabye Baby!"));
        assert!(query_targets_artist("rockabye baby nirvana", "Rockabye Baby!"));
        assert!(query_targets_artist("Sing King", "Sing King"));
        assert!(query_targets_artist("Cher", "Cher"));
        assert!(!query_targets_artist("Cher Believe", "Cher"));
        assert!(!query_targets_artist(
            "Blue oyster the reaper",
            "ProSource Karaoke"
        ));
        assert!(!query_targets_artist(
            "Blue oyster the reaper",
            "Acoustified Hits"
        ));
        assert!(!query_targets_artist(
            "Blue oyster the reaper",
            "Vitamin String Quartet"
        ));
    }
}
