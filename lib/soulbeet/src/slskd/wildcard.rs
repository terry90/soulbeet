//! The texts a Soulseek search is sent again with when nothing answered it.
//!
//! The Soulseek server keeps a list of excluded phrases and drops, unanswered, every search that
//! holds every word of one of them, in any order. slskd receives the list at login
//! (`Application.cs` `Client_ExcludedSearchPhrasesReceived`) and exposes it through no API, so a
//! dropped search reads as one with no response at all. A word whose first character is
//! replaced by `*` is a wildcard term, which the server does not match against the list, and
//! peers still answer files whose path holds the rest of the word. So a dropped search is sent
//! again with some words written as wildcards: any excluded phrase is broken by one of two
//! complementary texts.

/// The texts to send, in order, after `parts` (the artist and the title, or one free text)
/// drew no response: first every other wildcardable word of each part, starting with its first
/// (a whole artist name or title is the usual excluded phrase), then the wildcardable words
/// the first left as they were. A text of three or more words keeps one word that is not a
/// wildcard: peers answer junk to wildcards alone. Texts equal to the original or to an
/// earlier one are left out.
pub(super) fn retry_texts(parts: &[&str]) -> Vec<String> {
    let words: Vec<Vec<&str>> = parts
        .iter()
        .map(|part| part.split_whitespace().collect())
        .collect();
    let count: usize = words.iter().map(Vec::len).sum();
    let mut first = Vec::new();
    let mut second = Vec::new();
    for (p, part) in words.iter().enumerate() {
        let wildcardable = (0..part.len()).filter(|&w| wildcardable(part[w]));
        for (rank, w) in wildcardable.enumerate() {
            match rank % 2 {
                0 => first.push((p, w)),
                _ => second.push((p, w)),
            }
        }
    }
    if count >= 3 && all_wildcards(&words, &first) {
        if let Some(last) = first.pop() {
            second.push(last);
        }
    }
    if count >= 3 && all_wildcards(&words, &second) {
        second.clear();
    }

    let original = render(&words, &[]);
    let mut texts: Vec<String> = Vec::new();
    for chosen in [first, second] {
        let text = render(&words, &chosen);
        if text != original && !texts.contains(&text) {
            texts.push(text);
        }
    }
    texts
}

/// Whether `word` can be written as a wildcard term: it starts with a letter or digit, and
/// what is left after that character still holds one, so the term is never a bare `*`.
fn wildcardable(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(char::is_alphanumeric) && chars.any(char::is_alphanumeric)
}

/// Whether every word is a wildcard once the words at `chosen` are.
fn all_wildcards(words: &[Vec<&str>], chosen: &[(usize, usize)]) -> bool {
    words.iter().enumerate().all(|(p, part)| {
        (0..part.len()).all(|w| part[w].starts_with('*') || chosen.contains(&(p, w)))
    })
}

/// The words joined by single spaces, those at `chosen` with their first character replaced by
/// `*`.
fn render(words: &[Vec<&str>], chosen: &[(usize, usize)]) -> String {
    let mut out: Vec<String> = Vec::new();
    for (p, part) in words.iter().enumerate() {
        for (w, word) in part.iter().enumerate() {
            out.push(match chosen.contains(&(p, w)) {
                true => {
                    let rest = word.char_indices().nth(1).map_or("", |(at, _)| &word[at..]);
                    format!("*{rest}")
                }
                false => word.to_string(),
            });
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_retry_wildcards_the_first_word_of_the_artist_and_of_the_title() {
        assert_eq!(
            retry_texts(&["Ella Fitzgerald", "Winter Wonderland"]),
            [
                "*lla Fitzgerald *inter Wonderland",
                "Ella *itzgerald Winter *onderland"
            ]
        );
        assert_eq!(
            retry_texts(&["Kanye West", "Heartless"]),
            ["*anye West *eartless", "Kanye *est Heartless"]
        );
    }

    /// Two words may both be wildcards: there is no word left for the second text.
    #[test]
    fn a_one_word_artist_and_title_are_both_wildcarded_once() {
        assert_eq!(retry_texts(&["MGMT", "Kids"]), ["*GMT *ids"]);
    }

    #[test]
    fn a_word_of_one_character_is_never_a_bare_wildcard() {
        assert_eq!(
            retry_texts(&["Ariana Grande", "7 rings"]),
            ["*riana Grande 7 *ings", "Ariana *rande 7 rings"]
        );
        assert_eq!(
            retry_texts(&["Gorillaz", "E."]),
            ["*orillaz E."],
            "a word with no letter after its first character keeps it"
        );
        assert!(retry_texts(&["X", "7"]).is_empty());
    }

    #[test]
    fn free_text_is_one_part() {
        assert_eq!(
            retry_texts(&["Ella Swinging Christmas Winter Wonderland"]),
            [
                "*lla Swinging *hristmas Winter *onderland",
                "Ella *winging Christmas *inter Wonderland"
            ]
        );
        assert_eq!(
            retry_texts(&["Winter  Wonderland "]),
            ["*inter Wonderland", "Winter *onderland"]
        );
        assert_eq!(retry_texts(&["MGMT"]), ["*GMT"]);
    }

    #[test]
    fn three_words_or_more_keep_one_that_is_not_a_wildcard() {
        // The caller's own wildcards count: the first text would leave no plain word, so the
        // title's goes to the second.
        assert_eq!(
            retry_texts(&["*lla Fitzgerald", "Wonderland"]),
            ["*lla *itzgerald Wonderland", "*lla Fitzgerald *onderland"]
        );
        // Only Wonderland is left, and it stays.
        assert!(retry_texts(&["*lla *itzgerald Wonderland"]).is_empty());
        assert_eq!(
            retry_texts(&["*GMT Kids"]),
            ["*GMT *ids"],
            "two words may both be wildcards"
        );
    }

    #[test]
    fn the_first_character_is_replaced_whatever_its_width() {
        assert_eq!(
            retry_texts(&["Ólafur Arnalds", "Árbakkinn"]),
            ["*lafur Arnalds *rbakkinn", "Ólafur *rnalds Árbakkinn"]
        );
    }
}
