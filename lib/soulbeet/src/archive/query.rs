//! The text of archive.org's advanced search queries.

/// Every query keeps out what the backend never offers: anything but audio, stream-only and
/// restricted items, and the Live Music Archive.
pub(crate) const FILTERS: &str = "mediatype:audio AND -collection:stream_only \
     AND -access-restricted-item:true AND -collection:etree";

/// The fields a search reads of each item.
pub(crate) const FIELDS: &[&str] = &[
    "identifier",
    "title",
    "creator",
    "collection",
    "format",
    "downloads",
    "external-identifier",
    "licenseurl",
    "access-restricted-item",
];

/// The items a search reads at most per query, most downloaded first.
pub(crate) const ROWS: u32 = 50;

/// `text` as plain text in a Lucene query: a backslash before each character the syntax uses,
/// so "AC/DC" and "Sunn O)))" search for themselves.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `text` as one phrase.
fn phrase(text: &str) -> String {
    format!("\"{}\"", escape(text.trim()))
}

/// Each word of `text`, all of them wanted.
fn words(text: &str) -> String {
    text.split_whitespace()
        .map(escape)
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// What a search asks archive.org for, before the filters.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target<'a> {
    /// An album, or a track by the album it is on.
    Album { artist: &'a str, album: &'a str },
    /// A track no album names.
    Track { artist: &'a str, title: &'a str },
}

impl Target<'_> {
    /// Whether the target names no title to search for.
    pub(crate) fn is_blank(&self) -> bool {
        match self {
            Target::Album { album: title, .. } | Target::Track { title, .. } => {
                title.trim().is_empty()
            }
        }
    }

    /// The clause for the target, its title as one phrase.
    pub(crate) fn phrase(&self) -> String {
        match self {
            Target::Album { artist, album } => {
                format!("creator:({}) AND title:({})", phrase(artist), phrase(album))
            }
            Target::Track { artist, title } => {
                format!("creator:({}) AND ({})", phrase(artist), phrase(title))
            }
        }
    }

    /// The clause again with the title's words unquoted, for when the phrase finds nothing;
    /// None when the title has one word, which the phrase already searched.
    pub(crate) fn unquoted(&self) -> Option<String> {
        let (artist, title) = match self {
            Target::Album { artist, album } => (artist, album),
            Target::Track { artist, title } => (artist, title),
        };
        if title.split_whitespace().count() < 2 {
            return None;
        }
        Some(match self {
            Target::Album { .. } => {
                format!("creator:({}) AND title:({})", phrase(artist), words(title))
            }
            Target::Track { .. } => format!("creator:({}) AND ({})", phrase(artist), words(title)),
        })
    }
}

/// The clause for the items that name the MusicBrainz release `mbid`; None for a text that is
/// no MusicBrainz id.
pub(crate) fn release(mbid: &str) -> Option<String> {
    let mbid = mbid.trim();
    let plain = mbid.len() == 36 && mbid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    plain.then(|| format!("external-identifier:\"urn:mb_release_id:{mbid}\""))
}

/// A caller's own text, its words all wanted.
pub(crate) fn text(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| format!("({})", words(text)))
}

/// `clause` with the filters every query carries.
pub(crate) fn filtered(clause: &str) -> String {
    format!("{clause} AND {FILTERS}")
}

/// The path and query of an advanced search for `q`, reading `rows` items.
pub(crate) fn search_path(q: &str, rows: u32) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("q", q);
    for field in FIELDS {
        query.append_pair("fl[]", field);
    }
    query
        .append_pair("rows", &rows.to_string())
        .append_pair("sort[]", "downloads desc")
        .append_pair("output", "json");
    format!("/advancedsearch.php?{}", query.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lucene_syntax_in_names_is_escaped() {
        assert_eq!(escape("AC/DC"), "AC\\/DC");
        assert_eq!(escape("Sunn O)))"), "Sunn O\\)\\)\\)");
        assert_eq!(escape("\"quoted\""), "\\\"quoted\\\"");
        assert_eq!(escape("a+b-c&&d||e!f"), "a\\+b\\-c\\&\\&d\\|\\|e\\!f");
        assert_eq!(escape("{[^~*?:\\]}"), "\\{\\[\\^\\~\\*\\?\\:\\\\\\]\\}");
        assert_eq!(escape("Björk"), "Björk");
    }

    #[test]
    fn an_album_is_asked_as_two_phrases_then_its_words() {
        let album = Target::Album {
            artist: "AC/DC",
            album: "Back in Black",
        };
        assert_eq!(
            album.phrase(),
            "creator:(\"AC\\/DC\") AND title:(\"Back in Black\")"
        );
        assert_eq!(
            album.unquoted().unwrap(),
            "creator:(\"AC\\/DC\") AND title:(Back AND in AND Black)"
        );
        let track = Target::Track {
            artist: "Nine Inch Nails",
            title: "Letting You",
        };
        assert_eq!(
            track.phrase(),
            "creator:(\"Nine Inch Nails\") AND (\"Letting You\")"
        );
        assert_eq!(
            track.unquoted().unwrap(),
            "creator:(\"Nine Inch Nails\") AND (Letting AND You)"
        );
        let one_word = Target::Album {
            artist: "Duster",
            album: "Stratosphere",
        };
        assert_eq!(one_word.unquoted(), None);
    }

    #[test]
    fn every_query_keeps_out_stream_only_restricted_and_live_music_archive_items() {
        let q = filtered(
            &Target::Album {
                artist: "Daft Punk",
                album: "Discovery",
            }
            .phrase(),
        );
        assert_eq!(
            q,
            "creator:(\"Daft Punk\") AND title:(\"Discovery\") AND mediatype:audio AND \
             -collection:stream_only AND -access-restricted-item:true AND -collection:etree"
        );
    }

    #[test]
    fn a_release_is_asked_by_its_id_and_only_a_real_id_is_sent() {
        assert_eq!(
            release("c7ba938a-d3b3-4eee-9fc9-8c51eec86f0a").as_deref(),
            Some("external-identifier:\"urn:mb_release_id:c7ba938a-d3b3-4eee-9fc9-8c51eec86f0a\"")
        );
        assert_eq!(release("c7ba938a\" OR identifier:*"), None);
        assert_eq!(release(""), None);
        assert_eq!(text("  "), None);
        assert_eq!(
            text("duster moon age").unwrap(),
            "(duster AND moon AND age)"
        );
    }

    #[test]
    fn a_search_path_encodes_the_query_and_asks_for_json() {
        let path = search_path("creator:(\"AC\\/DC\")", 50);
        assert!(
            path.starts_with("/advancedsearch.php?q=creator%3A%28%22AC%5C%2FDC%22%29&"),
            "{path}"
        );
        assert!(path.contains("&fl%5B%5D=identifier&"), "{path}");
        assert!(
            path.ends_with("&rows=50&sort%5B%5D=downloads+desc&output=json"),
            "{path}"
        );
    }
}
