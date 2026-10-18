//! The saved scripts a `note_search` reply lists after its notes, so an agent finds a script
//! without another tool call.
//!
//! The search text is split into words the way note search splits it (alphanumeric runs,
//! lowercased, accents dropped; common English words left out, the last word matching as a
//! prefix), and a script matches when its name or description holds one of them. Scripts are
//! ranked by the words they match, then by runs, then by the latest write. They come from the
//! store scripts are routed to (the local file or the team database), and a script whose name or
//! description holds a [`hidden_character`] is never listed. A failing script store leaves the
//! block out and never fails the search.

use std::cmp::Reverse;
use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::Server;
use super::script_blocks::SCRIPTS_FRAME;
use crate::limits::QUERY_TOKENS_MAX;
use crate::store::{Scope, ScriptSummary, StoreError, View, clean, hidden_character};
use crate::support::text::STOPWORDS;

/// Scripts listed after the notes.
const LISTED_MAX: usize = 5;

/// The longest description shown, in characters.
const DESCRIPTION_MAX: usize = 120;

/// The block of saved scripts visible in `view` that match `query`: [`SCRIPTS_FRAME`] and a
/// `<saved-scripts>` element with one line per script. `None` when none matches or the script
/// store failed (noted on stderr).
pub(super) fn block(server: &mut Server, view: &View, query: &str) -> Option<String> {
    let words = query_words(query);

    if words.is_empty() {
        return None;
    }

    let summaries = match server.scripts.list_scripts(view, None) {
        Ok(summaries) => summaries,
        Err(error) => {
            eprintln!(
                "harness-memory: note_search left out saved scripts: {}",
                failure(&error)
            );
            return None;
        }
    };

    let mut ranked: Vec<(usize, &ScriptSummary)> = summaries
        .iter()
        .filter(|summary| shown_safely(summary))
        .map(|summary| (matched(&words, summary), summary))
        .filter(|(count, _)| *count > 0)
        .collect();

    if ranked.is_empty() {
        return None;
    }

    ranked.sort_by_key(|(count, summary)| {
        (
            Reverse(*count),
            Reverse(summary.run_count),
            Reverse(summary.updated_at),
            summary.id,
        )
    });

    let lines: Vec<String> = ranked
        .iter()
        .take(LISTED_MAX)
        .map(|(_, summary)| line(summary, &summaries))
        .collect();

    Some(format!(
        "{SCRIPTS_FRAME}\n<saved-scripts>\n{}\n</saved-scripts>",
        lines.join("\n")
    ))
}

/// What stderr may say about a store failure: the database's own message (which carries no
/// stored text), never text a client sent.
fn failure(error: &StoreError) -> String {
    match error {
        StoreError::Backend(_) | StoreError::NewerSchema => error.to_string(),
        _ => "the script store refused the read".to_owned(),
    }
}

/// The search words: [`words`] of `query` without common English words and repeats, at most
/// [`QUERY_TOKENS_MAX`], the last one (the word being typed) matching as a prefix.
fn query_words(query: &str) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();

    for word in words(query).into_iter().take(QUERY_TOKENS_MAX) {
        if !STOPWORDS.contains(&word.as_str()) && !kept.contains(&word) {
            kept.push(word);
        }
    }

    kept
}

/// The alphanumeric words of `text`, lowercased and without accents.
fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(fold)
        .filter(|word| !word.is_empty())
        .collect()
}

/// `word` lowercased, with combining marks (accents) dropped after canonical decomposition.
fn fold(word: &str) -> String {
    word.nfd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

/// How many of the search `words` the name or description of `summary` holds (the last search
/// word also as the start of a word).
fn matched(words: &[String], summary: &ScriptSummary) -> usize {
    let own: HashSet<String> = words_of(summary).into_iter().collect();
    let last = words.len() - 1;

    words
        .iter()
        .enumerate()
        .filter(|(index, word)| {
            own.contains(*word) || (*index == last && own.iter().any(|known| known.starts_with(word.as_str())))
        })
        .count()
}

fn words_of(summary: &ScriptSummary) -> Vec<String> {
    words(&format!("{} {}", summary.name, summary.description))
}

/// True when nothing the line shows hides a character.
fn shown_safely(summary: &ScriptSummary) -> bool {
    hidden_character(&summary.name).is_none() && hidden_character(&summary.description).is_none()
}

/// `- <name>: <description> (<language>) → script_run name=<name>`; a global script that a
/// repository script of the same name hides is run by `id=<id>`.
fn line(summary: &ScriptSummary, visible: &[ScriptSummary]) -> String {
    let name = clean::line(&summary.name);
    let description = description(&summary.description);
    let description = if description.is_empty() {
        String::new()
    } else {
        format!(": {description}")
    };

    let hidden_by_repo = summary.scope == Scope::Global
        && visible
            .iter()
            .any(|other| other.scope == Scope::Repo && other.name == summary.name);
    let call = if hidden_by_repo {
        format!("id={}", summary.id)
    } else {
        format!("name={name}")
    };

    format!(
        "- {name}{description} ({}) → script_run {call}",
        summary.language.as_str()
    )
}

/// The description on one line, cut to [`DESCRIPTION_MAX`] characters (ending with `…`).
fn description(text: &str) -> String {
    let line = clean::line(text);

    if line.chars().count() <= DESCRIPTION_MAX {
        return line;
    }

    let cut: String = line.chars().take(DESCRIPTION_MAX - 1).collect();
    format!("{}…", cut.trim_end())
}
