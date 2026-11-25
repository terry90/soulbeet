//! Versions of a recording that are not the one wanted: a live take, a remix, a cover.

/// Words that name another version, matched as whole words in any case.
const WORD_MARKERS: &[&str] = &[
    "live",
    "remix",
    "sped up",
    "speed up",
    "slowed",
    "reverb",
    "nightcore",
    "8d",
    "instrumental",
    "karaoke",
    "cover",
    "acoustic",
    "demo",
    "edit",
    "mix",
    "version",
];

/// Chinese markers of another version, matched anywhere: the language has no spaces between
/// words.
const TEXT_MARKERS: &[&str] = &["伴奏", "原唱", "翻唱", "钢琴版", "女声版", "纯音乐"];

/// The lowercase words of `title`, split at every character that is not a letter or a digit.
fn words(title: &str) -> Vec<String> {
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `words` holds `marker`'s words one after another.
fn has_words(words: &[String], marker: &str) -> bool {
    let marker: Vec<&str> = marker.split(' ').collect();
    words
        .windows(marker.len())
        .any(|run| run.iter().zip(&marker).all(|(w, m)| w == m))
}

/// The markers `title` holds.
fn markers(title: &str) -> Vec<&'static str> {
    let words = words(title);
    let worded = WORD_MARKERS.iter().filter(|m| has_words(&words, m));
    let texted = TEXT_MARKERS.iter().filter(|m| title.contains(*m));
    worded.chain(texted).copied().collect()
}

/// Whether `candidate_title` names a version `wanted_title` does not: a candidate that is a
/// remix, live or a cover of the wanted recording is none of it, whatever else matches.
pub fn version_veto(candidate_title: &str, wanted_title: &str) -> bool {
    let wanted = markers(wanted_title);
    markers(candidate_title)
        .iter()
        .any(|marker| !wanted.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_the_wanted_title_does_not_name_is_vetoed() {
        for (candidate, wanted, vetoed) in [
            ("Wake Up", "Wake Up", false),
            ("Wake Up (Live at Wembley)", "Wake Up", true),
            ("Wake Up (Live at Wembley)", "Wake Up - Live", false),
            ("Wake Up [Club Remix]", "Wake Up", true),
            (
                "Monkeys Spinning Monkeys (SPEED UP)",
                "Monkeys Spinning Monkeys",
                true,
            ),
            (
                "Monkeys Spinning Monkeys - sped up",
                "Monkeys Spinning Monkeys",
                true,
            ),
            ("Kowloon (slowed + reverb)", "Kowloon", true),
            ("Kowloon (Nightcore)", "Kowloon", true),
            ("Kowloon 8D Audio", "Kowloon", true),
            ("Kowloon (Instrumental)", "Kowloon", true),
            ("Kowloon (Karaoke Version)", "Kowloon", true),
            ("Kowloon (Acoustic)", "Kowloon", true),
            ("Kowloon (Demo)", "Kowloon", true),
            ("Kowloon (Radio Edit)", "Kowloon", true),
            ("Kowloon (Extended Mix)", "Kowloon", true),
            ("Kowloon (Cover)", "Kowloon", true),
            // Whole words only.
            ("Alive", "Alive", false),
            ("Alive", "Kowloon", false),
            ("The Edition", "Kowloon", false),
            ("Mixtape", "Kowloon", false),
            ("Discovery", "Kowloon", false),
            ("Live Forever", "Live Forever", false),
            ("Speed of Sound", "Speed of Sound", false),
            ("Up", "Speed Up", false),
            // Each marker the candidate names must be in the wanted title.
            ("Kowloon (Live Remix)", "Kowloon (Live)", true),
            ("晴天 (伴奏)", "晴天", true),
            ("晴天 翻唱", "晴天", true),
            ("晴天 (钢琴版)", "晴天", true),
            ("晴天 女声版", "晴天", true),
            ("晴天 纯音乐", "晴天", true),
            ("晴天 原唱", "晴天", true),
            ("晴天 (钢琴版)", "晴天 钢琴版", false),
            ("晴天", "晴天", false),
        ] {
            assert_eq!(
                version_veto(candidate, wanted),
                vetoed,
                "{candidate:?} for {wanted:?}"
            );
        }
    }
}
