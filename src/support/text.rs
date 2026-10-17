//! Text checks shared by the store and the project resolver.

/// True when `value` has no control characters, no invisible format characters (Unicode
/// categories Cf, Zl and Zp: bidirectional overrides, isolates and marks, zero-width characters,
/// the soft hyphen, the byte-order mark, line/paragraph separators, tag characters) and no other
/// character that renders as nothing (fillers, blanks, variation selectors), so it renders as what
/// it is.
pub fn display_safe(value: &str) -> bool {
    !value
        .chars()
        .any(|character| character.is_control() || is_format_character(character))
}

/// Unicode format characters (category Cf), the line and paragraph separators (Zl, Zp), and the
/// invisible characters outside them: the combining grapheme joiner (U+034F), the Hangul fillers
/// (U+115F, U+1160, U+3164, U+FFA0), the Khmer inherent vowels (U+17B4, U+17B5), the Mongolian
/// variation selectors (U+180B–U+180F), the blank Braille pattern (U+2800) and the variation
/// selectors (U+FE00–U+FE0F, U+E0100–U+E01EF).
const FORMAT_CHARACTERS: &[(char, char)] = &[
    ('\u{00AD}', '\u{00AD}'),
    ('\u{034F}', '\u{034F}'),
    ('\u{0600}', '\u{0605}'),
    ('\u{061C}', '\u{061C}'),
    ('\u{06DD}', '\u{06DD}'),
    ('\u{070F}', '\u{070F}'),
    ('\u{0890}', '\u{0891}'),
    ('\u{08E2}', '\u{08E2}'),
    ('\u{115F}', '\u{1160}'),
    ('\u{17B4}', '\u{17B5}'),
    ('\u{180B}', '\u{180F}'),
    ('\u{200B}', '\u{200F}'),
    ('\u{2028}', '\u{202E}'),
    ('\u{2060}', '\u{2064}'),
    ('\u{2066}', '\u{206F}'),
    ('\u{2800}', '\u{2800}'),
    ('\u{3164}', '\u{3164}'),
    ('\u{FE00}', '\u{FE0F}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFA0}', '\u{FFA0}'),
    ('\u{FFF9}', '\u{FFFB}'),
    ('\u{110BD}', '\u{110BD}'),
    ('\u{110CD}', '\u{110CD}'),
    ('\u{13430}', '\u{1343F}'),
    ('\u{1BCA0}', '\u{1BCA3}'),
    ('\u{1D173}', '\u{1D17A}'),
    ('\u{E0001}', '\u{E0001}'),
    ('\u{E0020}', '\u{E007F}'),
    ('\u{E0100}', '\u{E01EF}'),
];

/// True for a character of [`FORMAT_CHARACTERS`]: invisible, or changing how the text around it
/// is shown.
pub fn is_format_character(character: char) -> bool {
    FORMAT_CHARACTERS
        .iter()
        .any(|(first, last)| (*first..=*last).contains(&character))
}

/// Common English words left out of search queries (the similar-note lookup; Harness Desktop
/// leaves the same words out of its memory recall queries).
pub const STOPWORDS: &[&str] = &[
    "about", "after", "also", "and", "any", "are", "because", "before", "but", "can", "could", "did", "does", "done",
    "each", "for", "from", "get", "got", "has", "have", "here", "how", "into", "its", "just", "like", "more", "most",
    "new", "not", "now", "one", "only", "our", "out", "over", "per", "set", "should", "some", "such", "than", "that",
    "the", "them", "then", "there", "they", "this", "two", "use", "used", "uses", "using", "very", "via", "was",
    "were", "what", "when", "where", "which", "why", "will", "with", "within", "without", "would", "you", "your",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_invisible_characters_are_not_display_safe() {
        for character in [
            '\u{0007}',
            '\u{001B}',
            '\u{00AD}',
            '\u{034F}',
            '\u{115F}',
            '\u{1160}',
            '\u{17B4}',
            '\u{17B5}',
            '\u{180B}',
            '\u{180F}',
            '\u{200B}',
            '\u{202E}',
            '\u{2066}',
            '\u{2800}',
            '\u{3164}',
            '\u{FE00}',
            '\u{FE0F}',
            '\u{FEFF}',
            '\u{FFA0}',
            '\u{E0041}',
            '\u{E0100}',
            '\u{E01EF}',
        ] {
            let value = format!("ok{character}name");
            assert!(!display_safe(&value), "U+{:04X}", u32::from(character));
        }
    }

    #[test]
    fn visible_text_in_any_script_is_display_safe() {
        assert!(display_safe("한국어 server"));
        assert!(display_safe("emoji 👍 and 日本語"));
        assert!(display_safe("git@github.com:acme/widget.git"));
    }
}
