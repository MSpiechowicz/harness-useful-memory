//! The language of a note: English or not, for the English notes setting and the rewrite hints.
//!
//! Detection is conservative. [`Lang::Other`] needs a reliable verdict of a language that is not
//! English over at least [`OTHER_MIN_LETTERS`] letters of prose, so a short note, or one that is
//! mostly code, paths and identifiers, is never flagged.

use whatlang::Lang as Detected;

/// The fewest letters of prose (code-like words left out) a note needs to be called
/// [`Lang::Other`].
pub const OTHER_MIN_LETTERS: usize = 40;

/// The stored value of a note whose language is not certain ([`stored`]).
pub const UNKNOWN: &str = "unknown";

/// The language of a note, as far as it is certain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Reliably English.
    En,
    /// Reliably another language, in a text long enough to tell.
    Other,
}

impl Lang {
    /// The stored name: `en` or `other`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Other => "other",
        }
    }

    /// The language a stored value names; `None` for `unknown`, a missing value or anything else.
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            "en" => Some(Self::En),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// The value a store keeps in a note's `lang` column or field for [`detect`]'s answer: `en`,
/// `other`, or [`UNKNOWN`] when it was not certain. A missing value means the note was not looked
/// at yet (a client that does not know the column wrote it).
pub fn stored(lang: Option<Lang>) -> &'static str {
    lang.map_or(UNKNOWN, Lang::as_str)
}

/// The language of a note with this `title` and `body`: [`Lang::Other`] when whatlang is reliable,
/// it is not English and the prose has at least [`OTHER_MIN_LETTERS`] letters, [`Lang::En`] when
/// it is reliably English, else `None`.
pub fn detect(title: &str, body: &str) -> Option<Lang> {
    let prose = prose(&format!("{title}\n{body}"));
    let info = whatlang::detect(&prose)?;

    if !info.is_reliable() {
        return None;
    }

    if info.lang() == Detected::Eng {
        return Some(Lang::En);
    }

    let letters = prose.chars().filter(|character| character.is_alphabetic()).count();

    (letters >= OTHER_MIN_LETTERS).then_some(Lang::Other)
}

/// Characters that make a word an identifier, a path, a URL or an expression instead of prose when
/// they stand inside it.
const CODE_INSIDE: &[char] = &[
    '/', '\\', '_', ':', '.', '=', '<', '>', '{', '}', '[', ']', '(', ')', '$', '#', '@', '|', ';', '*', '&', '~',
];

/// `text` without its code: backtick spans and fenced blocks, and the words that look like paths,
/// URLs, identifiers or expressions.
fn prose(text: &str) -> String {
    // Splitting at backticks leaves prose in the even parts and code in the odd ones; a fence of
    // three backticks toggles three times, which ends in the same state.
    let outside = text.split('`').step_by(2);
    let mut kept: Vec<&str> = Vec::new();

    for part in outside {
        for word in part.split_whitespace() {
            let word = word.trim_matches(|character: char| !character.is_alphanumeric());

            if !word.is_empty() && !word.contains(CODE_INSIDE) {
                kept.push(word);
            }
        }
    }

    kept.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGLISH: &str = "The sync job reads every changed note from the team database, then writes the new \
                           revision to the local cache so the next search finds it without another round trip.";

    #[test]
    fn english_prose_is_english() {
        assert_eq!(detect("Cache the notes locally", ENGLISH), Some(Lang::En));
    }

    #[test]
    fn polish_and_german_prose_is_other() {
        let polish = "Zadanie synchronizacji czyta każdą zmienioną notatkę z bazy zespołu, a potem zapisuje nową \
                      wersję w lokalnej pamięci podręcznej, żeby następne wyszukiwanie znalazło ją od razu.";
        let german = "Der Synchronisierungsauftrag liest jede geänderte Notiz aus der Teamdatenbank und schreibt \
                      die neue Fassung in den lokalen Zwischenspeicher, damit die nächste Suche sie sofort findet.";

        assert_eq!(detect("Synchronizacja notatek", polish), Some(Lang::Other));
        assert_eq!(detect("Notizen abgleichen", german), Some(Lang::Other));
    }

    #[test]
    fn a_short_text_is_never_other() {
        for (title, body) in [
            ("Wersja", "Zapisz plik"),
            ("Datenbank", "Neu starten"),
            ("", ""),
            ("ok", ""),
        ] {
            assert_ne!(detect(title, body), Some(Lang::Other), "{title} {body}");
        }
    }

    #[test]
    fn code_and_paths_do_not_make_english_notes_other() {
        let body = "Run `cargo test --locked` then check src/store/sqlite/schema.rs and notes_fts_en::rebuild, \
                    see https://example.com/docs/fts5.html for the option list.\n\
                    ```rust\nfn ensure_extensions(db: &mut dyn Sql) -> Result<(), StoreError> { Ok(()) }\n```";

        assert_ne!(detect("Where the schema lives", body), Some(Lang::Other));
    }

    #[test]
    fn mostly_code_is_not_other_even_with_a_foreign_remark() {
        let body = "`SELECT id, title FROM notes WHERE lang IS NULL ORDER BY id LIMIT 500` src/store/sqlite/notes.rs \
                    notes_fts_en::rebuild the_fts_table.sql a.b.c \
                    ```\nfor (id, title) in rows { backfill(id, title)?; }\n``` tak";

        assert_ne!(detect("", body), Some(Lang::Other));
    }

    #[test]
    fn stored_names_cover_every_answer() {
        assert_eq!(stored(Some(Lang::En)), "en");
        assert_eq!(stored(Some(Lang::Other)), "other");
        assert_eq!(stored(None), "unknown");
    }
}
