//! The release notes in CHANGELOG.md, which the "What's new" dialog shows.
//!
//! The file is written for the people who use Soulbeet, in a small fixed
//! format, one section per release, newest first:
//!
//! ```text
//! # Changelog
//!
//! ## 0.9.0 - 2026-12-01            (or `## Unreleased`, only as the first section)
//!
//! ### The headline of the release
//!
//! One intro line.
//!
//! - **Short title**: One plain sentence.   (3 to 5 of these)
//!
//! [Link text](https://...)                 (optional)
//! ```
//!
//! Text may hold `code`, and no other markup. The parser refuses every other
//! line, and a test parses the shipped file, so a malformed one cannot ship.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::LazyLock;

/// CHANGELOG.md as this build embeds it.
const SOURCE: &str = include_str!("../../../CHANGELOG.md");

/// The version of this build, as Cargo.toml names it.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

const MIN_HIGHLIGHTS: usize = 3;
const MAX_HIGHLIGHTS: usize = 5;

static CHANGELOG: LazyLock<Changelog> =
    LazyLock::new(|| parse(SOURCE).unwrap_or_else(|e| panic!("CHANGELOG.md, {e}")));

/// The embedded CHANGELOG.md, parsed on first use.
pub fn changelog() -> &'static Changelog {
    &CHANGELOG
}

/// The version of this build, compared as a release.
pub fn running_version() -> Version {
    Version::parse(APP_VERSION).unwrap_or_else(|e| panic!("Cargo.toml version, {e}"))
}

/// A `major.minor.patch` version. A pre-release suffix (`0.9.0-rc.1`) is
/// read and dropped, so a pre-release compares as its numeric triple.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn parse(text: &str) -> Result<Version, String> {
        let invalid = || format!("'{text}' is not a major.minor.patch version");
        let core = match text.split_once('-') {
            Some((core, pre)) => {
                let pre_ok = !pre.is_empty()
                    && pre.split('.').all(|part| {
                        !part.is_empty()
                            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    });
                if !pre_ok {
                    return Err(invalid());
                }
                core
            }
            None => text,
        };
        let number = |part: &str| -> Result<u64, String> {
            let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            let leading_zero = part.len() > 1 && part.starts_with('0');
            if !digits || leading_zero {
                return Err(invalid());
            }
            part.parse().map_err(|_| invalid())
        };
        let mut parts = core.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };
        Ok(Version {
            major: number(major)?,
            minor: number(minor)?,
            patch: number(patch)?,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A calendar day, written `YYYY-MM-DD` in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

impl Date {
    fn parse(text: &str) -> Result<Date, String> {
        let invalid = || format!("'{text}' is not a YYYY-MM-DD date");
        let bytes = text.as_bytes();
        let shape_ok = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        if !shape_ok {
            return Err(invalid());
        }
        let year: u16 = text[0..4].parse().map_err(|_| invalid())?;
        let month: u8 = text[5..7].parse().map_err(|_| invalid())?;
        let day: u8 = text[8..10].parse().map_err(|_| invalid())?;
        let leap =
            (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return Err(invalid()),
        };
        if day == 0 || day > days {
            return Err(invalid());
        }
        Ok(Date { year, month, day })
    }
}

/// `1 Dec 2026`: the same in every locale.
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match usize::from(self.month)
            .checked_sub(1)
            .and_then(|i| MONTHS.get(i))
        {
            Some(month) => write!(f, "{} {month} {}", self.day, self.year),
            None => write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day),
        }
    }
}

/// A piece of text: plain words or `code`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Span {
    Plain(String),
    Code(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text(pub Vec<Span>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub date: Date,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Highlight {
    pub title: Text,
    pub text: Text,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub text: String,
    pub url: String,
}

/// One `##` section of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// None for the `## Unreleased` section.
    pub release: Option<Release>,
    pub headline: Text,
    pub intro: Text,
    pub highlights: Vec<Highlight>,
    pub link: Option<Link>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changelog {
    /// Newest first.
    pub sections: Vec<Section>,
}

impl Changelog {
    /// The released sections newer than `seen` and no newer than `running`,
    /// newest first: what a user who last closed the notes at `seen` has not
    /// read yet. The Unreleased section is never among them.
    pub fn unseen(&self, seen: Version, running: Version) -> Vec<&Section> {
        self.sections
            .iter()
            .filter(|s| {
                s.release
                    .is_some_and(|r| r.version > seen && r.version <= running)
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based.
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// Characters markdown reads as markup outside `code`. The app shows text as
/// it is, so they would look different here and on GitHub.
const MARKUP: [char; 5] = ['*', '[', ']', '<', '\\'];

fn parse_text(text: &str) -> Result<Text, String> {
    if text.trim().is_empty() {
        return Err("text is empty".to_string());
    }
    if text.trim() != text {
        return Err("text starts or ends with a space".to_string());
    }
    let pieces: Vec<&str> = text.split('`').collect();
    if pieces.len().is_multiple_of(2) {
        return Err("a `code` span is not closed".to_string());
    }
    let mut spans = Vec::new();
    for (i, piece) in pieces.into_iter().enumerate() {
        if i % 2 == 1 {
            if piece.is_empty() {
                return Err("a `code` span is empty".to_string());
            }
            spans.push(Span::Code(piece.to_string()));
        } else if let Some(c) = piece.chars().find(|c| MARKUP.contains(c)) {
            return Err(format!("'{c}' is markup; only `code` is allowed in text"));
        } else if !piece.is_empty() {
            spans.push(Span::Plain(piece.to_string()));
        }
    }
    Ok(Text(spans))
}

fn parse_release(heading: &str) -> Result<Option<Release>, String> {
    if heading == "Unreleased" {
        return Ok(None);
    }
    let (version, date) = heading
        .split_once(" - ")
        .ok_or("a release heading is '## <version> - <YYYY-MM-DD>' or '## Unreleased'")?;
    let parsed = Version::parse(version)?;
    if parsed.to_string() != version {
        return Err(format!(
            "release '{version}' is written as a plain major.minor.patch version"
        ));
    }
    Ok(Some(Release {
        version: parsed,
        date: Date::parse(date)?,
    }))
}

fn parse_highlight(item: &str) -> Result<Highlight, String> {
    let shape = "a highlight is '- **Short title**: One plain sentence.'";
    let rest = item.strip_prefix("**").ok_or(shape)?;
    let (title, text) = rest.split_once("**: ").ok_or(shape)?;
    if !text.ends_with('.') {
        return Err("a highlight's sentence ends with a period".to_string());
    }
    Ok(Highlight {
        title: parse_text(title)?,
        text: parse_text(text)?,
    })
}

fn parse_link(line: &str) -> Result<Link, String> {
    let shape = "the link is '[Link text](https://...)', alone on its line";
    let rest = line.strip_prefix('[').ok_or(shape)?;
    let (text, rest) = rest.split_once("](").ok_or(shape)?;
    let url = rest.strip_suffix(')').ok_or(shape)?;
    if text.trim().is_empty() || text.trim() != text || text.contains(['[', ']', '`', '*']) {
        return Err("link text is plain words".to_string());
    }
    let host = url
        .strip_prefix("https://")
        .ok_or("a link starts with https://")?;
    if host.is_empty() || url.contains(|c: char| c.is_whitespace() || "()<>`".contains(c)) {
        return Err(format!("'{url}' is not a usable link"));
    }
    Ok(Link {
        text: text.to_string(),
        url: url.to_string(),
    })
}

/// What a line of the file is.
enum Line<'a> {
    Blank,
    Title,
    Release(&'a str),
    Headline(&'a str),
    Highlight(&'a str),
    Link(&'a str),
    Prose(&'a str),
}

fn classify(line: &str) -> Result<Line<'_>, String> {
    if line.is_empty() {
        return Ok(Line::Blank);
    }
    if line.trim_end() != line {
        return Err("the line ends with spaces".to_string());
    }
    if line.trim_start() != line {
        return Err("the line starts with spaces".to_string());
    }
    if let Some(rest) = line.strip_prefix("### ") {
        return Ok(Line::Headline(rest));
    }
    if let Some(rest) = line.strip_prefix("## ") {
        return Ok(Line::Release(rest));
    }
    if line.starts_with("# ") {
        return Ok(Line::Title);
    }
    if let Some(rest) = line.strip_prefix("- ") {
        return Ok(Line::Highlight(rest));
    }
    if line.starts_with('[') {
        return Ok(Line::Link(line));
    }
    let numbered = line
        .split_once(['.', ')'])
        .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let fence = line.starts_with("```") || line.starts_with("~~~");
    if numbered || fence || line.starts_with(['#', '-', '+', '*', '>', '|', '=', '<']) {
        return Err("only the changelog's own kinds of lines are allowed".to_string());
    }
    Ok(Line::Prose(line))
}

/// Where the parser is within a section.
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Title,
    FirstSection,
    Headline,
    Intro,
    Highlights,
    /// After the 3rd highlight: more highlights, the link or the next section.
    SectionEnd,
    /// After the link: only the next section.
    NextSection,
}

struct Draft {
    release: Option<Release>,
    line: usize,
    headline: Option<Text>,
    intro: Option<Text>,
    highlights: Vec<Highlight>,
    link: Option<Link>,
}

/// Reads a whole changelog, or says which line breaks the format.
pub fn parse(source: &str) -> Result<Changelog, ParseError> {
    let mut sections: Vec<Section> = Vec::new();
    let mut draft: Option<Draft> = None;
    let mut expect = Expect::Title;
    let mut previous_blank = true;

    let finish = |draft: Draft, sections: &mut Vec<Section>| -> Result<(), ParseError> {
        let line = draft.line;
        let error = |message: String| ParseError { line, message };
        let (Some(headline), Some(intro)) = (draft.headline, draft.intro) else {
            return Err(error(
                "a section needs a ### headline and an intro line".to_string(),
            ));
        };
        if draft.highlights.len() < MIN_HIGHLIGHTS {
            return Err(error(format!(
                "a section has {MIN_HIGHLIGHTS} to {MAX_HIGHLIGHTS} highlights, this one has {}",
                draft.highlights.len()
            )));
        }
        if let Some(newer) = sections.last() {
            let order = match (newer.release, draft.release) {
                (_, None) => Err("## Unreleased is only allowed as the first section"),
                (None, Some(_)) => Ok(()),
                (Some(n), Some(o)) if o.version >= n.version => {
                    Err("sections go from the newest version to the oldest, without repeats")
                }
                (Some(n), Some(o)) if o.date > n.date => {
                    Err("an older version cannot have a later date")
                }
                _ => Ok(()),
            };
            order.map_err(|m| error(m.to_string()))?;
        }
        sections.push(Section {
            release: draft.release,
            headline,
            intro,
            highlights: draft.highlights,
            link: draft.link,
        });
        Ok(())
    };

    for (index, raw) in source.lines().enumerate() {
        let number = index + 1;
        let error = |message: String| ParseError {
            line: number,
            message,
        };
        let line = classify(raw).map_err(error)?;
        let blank = matches!(line, Line::Blank);
        match (expect, line) {
            (_, Line::Blank) => {}
            (Expect::Title, Line::Title) => expect = Expect::FirstSection,
            (Expect::Title, _) => {
                return Err(error("the file starts with a '# ' title line".to_string()))
            }
            (
                Expect::FirstSection | Expect::SectionEnd | Expect::NextSection,
                Line::Release(heading),
            ) => {
                if let Some(done) = draft.take() {
                    finish(done, &mut sections)?;
                }
                draft = Some(Draft {
                    release: parse_release(heading).map_err(error)?,
                    line: number,
                    headline: None,
                    intro: None,
                    highlights: Vec::new(),
                    link: None,
                });
                expect = Expect::Headline;
            }
            (Expect::Headline, Line::Headline(text)) => {
                let d = draft.as_mut().expect("a section is open");
                d.headline = Some(parse_text(text).map_err(error)?);
                expect = Expect::Intro;
            }
            (Expect::Intro, Line::Prose(text)) => {
                let d = draft.as_mut().expect("a section is open");
                d.intro = Some(parse_text(text).map_err(error)?);
                expect = Expect::Highlights;
            }
            (Expect::Highlights | Expect::SectionEnd, Line::Highlight(item)) => {
                let d = draft.as_mut().expect("a section is open");
                if d.highlights.len() == MAX_HIGHLIGHTS {
                    return Err(error(format!(
                        "a section has at most {MAX_HIGHLIGHTS} highlights"
                    )));
                }
                d.highlights.push(parse_highlight(item).map_err(error)?);
                if d.highlights.len() >= MIN_HIGHLIGHTS {
                    expect = Expect::SectionEnd;
                }
            }
            (Expect::SectionEnd, Line::Link(text)) => {
                if !previous_blank {
                    return Err(error(
                        "a blank line comes before the link, or GitHub folds it into the \
                         last highlight"
                            .to_string(),
                    ));
                }
                let d = draft.as_mut().expect("a section is open");
                d.link = Some(parse_link(text).map_err(error)?);
                expect = Expect::NextSection;
            }
            (_, _) => {
                let wanted = match expect {
                    Expect::Title => unreachable!("handled above"),
                    Expect::FirstSection => "a '## ' section heading",
                    Expect::Headline => "the '### ' headline",
                    Expect::Intro => "one intro line",
                    Expect::Highlights => "a '- **Title**: sentence.' highlight",
                    Expect::SectionEnd => "a highlight, the link or the next section",
                    Expect::NextSection => "the next '## ' section",
                };
                return Err(error(format!("expected {wanted} here")));
            }
        }
        previous_blank = blank;
    }

    let end = source.lines().count();
    match draft {
        Some(done) => finish(done, &mut sections)?,
        None => {
            return Err(ParseError {
                line: end.max(1),
                message: "the file has no '## ' section".to_string(),
            })
        }
    }
    Ok(Changelog { sections })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    const HIGHLIGHTS: &str = "- **One**: First.\n- **Two**: Second.\n- **Three**: Third.\n";

    fn section(heading: &str) -> String {
        format!("## {heading}\n\n### Headline\n\nIntro.\n\n{HIGHLIGHTS}\n")
    }

    fn file(headings: &[&str]) -> String {
        let mut text = "# Changelog\n\n".to_string();
        for h in headings {
            text.push_str(&section(h));
        }
        text
    }

    fn error_of(source: &str) -> ParseError {
        parse(source).expect_err("the parser accepted it")
    }

    #[test]
    fn the_shipped_changelog_parses() {
        let parsed = parse(SOURCE).unwrap_or_else(|e| panic!("CHANGELOG.md, {e}"));
        assert!(!parsed.sections.is_empty());
        // What the app runs as must be readable as a version too.
        running_version();
    }

    #[test]
    fn a_full_section_reads_into_its_parts() {
        let source = "# Changelog\n\n## 0.9.0 - 2026-12-01\n\n### Ask for `music`\n\n\
            Soulbeet now does `this` and that.\n\n\
            - **Plain words**: Ask for an album.\n\
            - **Keeps going**: Downloads carry on after `the chat` ends.\n\
            - **Your account**: Each person connects.\n\
            - **Fourth**: Four.\n\n\
            [How to set it up](https://github.com/terry90/soulbeet#mcp-server)\n";
        let log = parse(source).unwrap();
        assert_eq!(log.sections.len(), 1);
        let s = &log.sections[0];
        assert_eq!(
            s.release,
            Some(Release {
                version: v("0.9.0"),
                date: Date {
                    year: 2026,
                    month: 12,
                    day: 1
                }
            })
        );
        assert_eq!(
            s.headline,
            Text(vec![
                Span::Plain("Ask for ".into()),
                Span::Code("music".into())
            ])
        );
        assert_eq!(
            s.intro,
            Text(vec![
                Span::Plain("Soulbeet now does ".into()),
                Span::Code("this".into()),
                Span::Plain(" and that.".into()),
            ])
        );
        assert_eq!(s.highlights.len(), 4);
        assert_eq!(
            s.highlights[1],
            Highlight {
                title: Text(vec![Span::Plain("Keeps going".into())]),
                text: Text(vec![
                    Span::Plain("Downloads carry on after ".into()),
                    Span::Code("the chat".into()),
                    Span::Plain(" ends.".into()),
                ]),
            }
        );
        assert_eq!(
            s.link,
            Some(Link {
                text: "How to set it up".into(),
                url: "https://github.com/terry90/soulbeet#mcp-server".into(),
            })
        );
    }

    #[test]
    fn sections_keep_their_order_and_unreleased_has_no_release() {
        let log = parse(&file(&[
            "Unreleased",
            "0.10.0 - 2027-01-02",
            "0.9.0 - 2026-12-01",
        ]))
        .unwrap();
        let releases: Vec<_> = log.sections.iter().map(|s| s.release).collect();
        assert_eq!(releases[0], None);
        assert_eq!(releases[1].unwrap().version, v("0.10.0"));
        assert_eq!(releases[2].unwrap().version, v("0.9.0"));
        // Blank lines between parts are optional, except before the link.
        let tight =
            "# Changelog\n## Unreleased\n### H\nIntro.\n- **A**: A.\n- **B**: B.\n- **C**: C.\n";
        assert_eq!(parse(tight).unwrap().sections.len(), 1);
    }

    #[test]
    fn every_kind_of_bad_line_is_refused_with_its_line_number() {
        // A section up to its headline: what follows starts on line 7.
        let headed = |rest: &str| format!("# Changelog\n\n## Unreleased\n\n### H\n\n{rest}");
        let intro = |line: &str| headed(&format!("{line}\n\n{HIGHLIGHTS}"));
        let ending = |rest: &str| headed(&format!("Intro.\n\n{HIGHLIGHTS}{rest}"));
        let two = "Intro.\n\n- **A**: A.\n- **B**: B.\n";
        let cases: Vec<(String, usize, &str)> = vec![
            ("## Unreleased\n".into(), 1, "title"),
            ("# Changelog\n".into(), 1, "no '## ' section"),
            ("# Changelog\n\nSome words.\n".into(), 3, "section heading"),
            ("# Changelog\n\n## Next\n".into(), 3, "release heading"),
            (
                "# Changelog\n\n## 0.9 - 2026-12-01\n".into(),
                3,
                "major.minor.patch",
            ),
            (
                "# Changelog\n\n## 0.9.0-rc.1 - 2026-12-01\n".into(),
                3,
                "plain major",
            ),
            (
                "# Changelog\n\n## 0.9.0 - 2026-13-01\n".into(),
                3,
                "YYYY-MM-DD",
            ),
            (
                "# Changelog\n\n## 0.9.0 - 2026-02-30\n".into(),
                3,
                "YYYY-MM-DD",
            ),
            (
                "# Changelog\n\n## 0.9.0 - 1 Dec 2026\n".into(),
                3,
                "YYYY-MM-DD",
            ),
            (
                "# Changelog\n\n## Unreleased\n\nIntro.\n".into(),
                5,
                "headline",
            ),
            (
                "# Changelog\n\n## Unreleased\n\n### H\n".into(),
                3,
                "intro line",
            ),
            (headed("- **A**: A.\n"), 7, "intro"),
            (headed("One.\nTwo.\n"), 8, "highlight"),
            (
                headed(&format!("Intro.\n\n- A plain item.\n{HIGHLIGHTS}")),
                9,
                "Short title",
            ),
            (
                headed(&format!("Intro.\n\n- **A**: No period\n{HIGHLIGHTS}")),
                9,
                "period",
            ),
            (intro("Intro with *emphasis*."), 7, "markup"),
            (intro("An [inline](https://x.y) link."), 7, "markup"),
            (intro("Unclosed `code."), 7, "not closed"),
            (intro("Empty `` code."), 7, "empty"),
            (intro("Intro. "), 7, "ends with spaces"),
            (intro("  Intro."), 7, "starts with spaces"),
            (intro("1. Intro."), 7, "own kinds of lines"),
            (intro("> Intro."), 7, "own kinds of lines"),
            (
                headed(&format!("{two}\n## 0.8.0 - 2026-01-01\n")),
                12,
                "highlight",
            ),
            (headed(two), 3, "3 to 5 highlights"),
            (
                ending("- **D**: D.\n- **E**: E.\n- **F**: F.\n"),
                14,
                "at most 5",
            ),
            (ending("[Docs](https://x.y)\n"), 12, "blank line"),
            (ending("\n[Docs](http://x.y)\n"), 13, "https://"),
            (
                ending("\n[Docs](https://x.y) and more\n"),
                13,
                "alone on its line",
            ),
            (
                ending("\n[Docs](https://x.y)\n\n[More](https://x.y)\n"),
                15,
                "next '## '",
            ),
            (ending("\nA closing line.\n"), 13, "highlight, the link"),
            (
                file(&["0.9.0 - 2026-12-01", "Unreleased"]),
                13,
                "first section",
            ),
            (file(&["Unreleased", "Unreleased"]), 13, "first section"),
            (
                file(&["0.9.0 - 2026-12-01", "0.9.0 - 2026-11-01"]),
                13,
                "without repeats",
            ),
            (
                file(&["0.8.0 - 2026-12-01", "0.9.0 - 2026-11-01"]),
                13,
                "newest version",
            ),
            (
                file(&["0.9.0 - 2026-11-01", "0.8.0 - 2026-12-01"]),
                13,
                "later date",
            ),
        ];
        for (source, line, words) in cases {
            let e = error_of(&source);
            assert!(
                e.line == line && e.message.contains(words),
                "expected line {line} with '{words}', got {e} for:\n{source}"
            );
        }
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.9.1") > v("0.9.0"));
        assert_eq!(v("0.9.0-rc.1"), v("0.9.0"));
        assert_eq!(v("0.9.0-rc.1").to_string(), "0.9.0");
        assert_eq!(v("12.0.3").to_string(), "12.0.3");
    }

    #[test]
    fn unparseable_versions_are_errors() {
        for bad in [
            "",
            "0.9",
            "0.9.0.1",
            "v0.9.0",
            "0.9.x",
            "0.09.0",
            "0.9.0-",
            "0.9.0-rc..1",
            "0.9.0 ",
            "-1.0.0",
            "0.9.0+build",
        ] {
            assert!(Version::parse(bad).is_err(), "'{bad}' parsed");
        }
    }

    #[test]
    fn dates_show_as_day_month_year() {
        assert_eq!(Date::parse("2026-12-01").unwrap().to_string(), "1 Dec 2026");
        assert_eq!(
            Date::parse("2028-02-29").unwrap().to_string(),
            "29 Feb 2028"
        );
        assert!(Date::parse("2027-02-29").is_err());
        assert!(Date::parse("2026-00-10").is_err());
    }

    #[test]
    fn a_user_sees_the_releases_after_theirs_up_to_the_running_one() {
        let log = parse(&file(&[
            "Unreleased",
            "0.11.0 - 2027-02-01",
            "0.10.0 - 2027-01-01",
            "0.9.0 - 2026-12-01",
        ]))
        .unwrap();
        let shown = |seen: &str, running: &str| -> Vec<String> {
            log.unseen(v(seen), v(running))
                .into_iter()
                .map(|s| s.release.unwrap().version.to_string())
                .collect()
        };
        // Up to date, or ahead of a server that was downgraded: nothing.
        assert!(shown("0.11.0", "0.11.0").is_empty());
        assert!(shown("0.11.0", "0.10.0").is_empty());
        // Behind: every release since, newest first; never Unreleased.
        assert_eq!(shown("0.8.0", "0.11.0"), ["0.11.0", "0.10.0", "0.9.0"]);
        assert_eq!(shown("0.9.0", "0.11.0"), ["0.11.0", "0.10.0"]);
        // Notes of a version newer than the one running wait for it.
        assert_eq!(shown("0.8.0", "0.10.0"), ["0.10.0", "0.9.0"]);
        // A pre-release of 0.11.0 already shows the 0.11.0 notes.
        assert_eq!(shown("0.10.0", "0.11.0-rc.1"), ["0.11.0"]);
        // A file with only Unreleased never opens on its own.
        let next = parse(&file(&["Unreleased"])).unwrap();
        assert!(next.unseen(v("0.0.0"), v("99.0.0")).is_empty());
    }

    /// A directory of the test binary's own, under the build's target dir.
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap();
        let dir = exe
            .parent()
            .unwrap()
            .join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_stamp(file: &std::path::Path, version: &str, date: &str) -> std::process::Output {
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/stamp_changelog.sh"
        );
        std::process::Command::new("bash")
            .args([script, version, date])
            .arg(file)
            .output()
            .unwrap()
    }

    fn stamp(file: &std::path::Path, version: &str, date: &str) {
        let run = run_stamp(file, version, date);
        assert!(run.status.success(), "{run:?}");
    }

    #[test]
    fn the_release_script_stamps_the_unreleased_section() {
        let dir = scratch_dir("stamp-changelog");
        let path = dir.join("CHANGELOG.md");

        std::fs::write(&path, file(&["Unreleased", "0.9.0 - 2026-12-01"])).unwrap();
        stamp(&path, "0.10.0", "2027-01-05");
        let stamped = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            stamped.sections[0].release,
            Some(Release {
                version: v("0.10.0"),
                date: Date {
                    year: 2027,
                    month: 1,
                    day: 5
                }
            })
        );
        assert_eq!(stamped.sections[1].release.unwrap().version, v("0.9.0"));

        // The shipped file stays valid once stamped.
        std::fs::write(&path, SOURCE).unwrap();
        stamp(&path, "99.0.0", "2099-01-01");
        parse(&std::fs::read_to_string(&path).unwrap()).unwrap();

        // Without an Unreleased section nothing changes.
        let released = file(&["0.9.0 - 2026-12-01"]);
        std::fs::write(&path, &released).unwrap();
        stamp(&path, "0.10.0", "2027-01-05");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), released);

        // A missing file fails the release instead of skipping the stamp.
        std::fs::remove_file(&path).unwrap();
        let run = run_stamp(&path, "0.10.0", "2027-01-05");
        assert!(!run.status.success(), "{run:?}");
        assert!(!path.exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn date_display_guards_out_of_bounds_month() {
        assert_eq!(
            Date {
                year: 2026,
                month: 0,
                day: 5,
            }
            .to_string(),
            "2026-00-05"
        );
        assert_eq!(
            Date {
                year: 2026,
                month: 13,
                day: 9,
            }
            .to_string(),
            "2026-13-09"
        );
        assert_eq!(
            Date {
                year: 2026,
                month: 255,
                day: 1,
            }
            .to_string(),
            "2026-255-01"
        );
    }
}
