//! Note or script ids a person lists for a command: one argument (`12,40`) or standard input
//! (`-`), for `push --overwrite`, `scripts push --overwrite` and `notes` and `scripts
//! discard-local`. Standard input is read up to
//! [`STDIN_IDS_BYTES_MAX`] bytes, and a list holds at most [`IDS_MAX`] ids, so a runaway pipe
//! cannot fill the memory.

use std::io::Read;

use crate::limits::{IDS_MAX, STDIN_IDS_BYTES_MAX};

/// Why a list of note ids was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdsError {
    /// A piece is not a positive whole number.
    #[error("ids are positive whole numbers, separated by commas or blanks")]
    NotIds,
    /// More than [`IDS_MAX`] ids.
    #[error("at most {IDS_MAX} ids can be given")]
    TooMany,
}

/// Note ids separated by commas or blanks, in the order given; an error when one is not a
/// positive whole number or there are more than [`IDS_MAX`].
pub fn note_ids(raw: &str) -> Result<Vec<i64>, IdsError> {
    let mut ids: Vec<i64> = Vec::new();

    for piece in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        if piece.is_empty() {
            continue;
        }

        let Some(id) = piece.parse::<i64>().ok().filter(|id| *id > 0) else {
            return Err(IdsError::NotIds);
        };

        if ids.len() == IDS_MAX {
            return Err(IdsError::TooMany);
        }

        ids.push(id);
    }

    Ok(ids)
}

/// Standard input as text; the reason when it cannot be read, is not text, or is longer than
/// [`STDIN_IDS_BYTES_MAX`] bytes.
pub fn read_stdin() -> Result<String, String> {
    read_capped(std::io::stdin())
}

fn read_capped(input: impl Read) -> Result<String, String> {
    let cap = u64::try_from(STDIN_IDS_BYTES_MAX).unwrap_or(u64::MAX);
    let mut text = String::new();

    input
        .take(cap.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|_| "the ids could not be read from stdin".to_owned())?;

    if text.len() > STDIN_IDS_BYTES_MAX {
        return Err(format!(
            "stdin holds more than {} MiB of ids",
            STDIN_IDS_BYTES_MAX / (1024 * 1024)
        ));
    }

    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_positive_numbers_up_to_the_cap() {
        assert_eq!(note_ids(" 3, 7\n9,,"), Ok(vec![3, 7, 9]));
        assert_eq!(note_ids(""), Ok(Vec::new()));
        assert_eq!(note_ids("3,x"), Err(IdsError::NotIds));
        assert_eq!(note_ids("0"), Err(IdsError::NotIds));

        let at_cap = vec!["1"; IDS_MAX].join(",");
        assert_eq!(note_ids(&at_cap).map(|ids| ids.len()), Ok(IDS_MAX));

        let past = format!("{at_cap},2");
        assert_eq!(note_ids(&past), Err(IdsError::TooMany));
    }

    #[test]
    fn stdin_is_read_up_to_the_cap() {
        let at_cap = "1".repeat(STDIN_IDS_BYTES_MAX);
        assert_eq!(
            read_capped(at_cap.as_bytes()).map(|text| text.len()),
            Ok(STDIN_IDS_BYTES_MAX)
        );

        let past = format!("{at_cap},");
        assert!(read_capped(past.as_bytes()).is_err());
    }
}
