//! What the end of a query names: a symbol, a file of the repository, or a string that names no symbol.
//!
//! `code_path`, `code_symbol` and `code_impact` take a name (`name`, `Type::name`, `module::name`,
//! `path/fragment::name`). An agent often knows a file, or a command or event name written as a string, and not
//! a symbol, so a text that names no symbol is read as the next of these:
//!
//! 1. a file of the repository (`ui/src/Panel.svelte`, also `./ui/src/Panel.svelte`, the absolute path, or a
//!    file name that only one file ends with): all its definitions, and for a start also the references it
//!    makes outside any definition;
//! 2. a string that is written in the code and names no definition (an event name): the definitions that write
//!    it;
//! 3. words (`/useful-skills doctor`, `the diff view asks for the full lines`): the definitions whose name or
//!    word literals hold them (see `words`).
//!
//! A name that is a symbol stays a symbol whatever else it names. The answer says which reading was used
//! whenever the end holds more than one definition.

use super::{Graph, shown};
use crate::code::extract;

/// Definitions of one file that an end holds, the first ones in the order of the file. A file with more is
/// cut.
const MAX_FILE_ENDS: usize = 400;

/// Files an end may hold when the text is a file name that several files end with.
const MAX_ENDS_FILES: usize = 4;

/// Definitions that write a string and are used as an end.
const MAX_MENTION_ENDS: usize = 64;

/// Definitions or places named in a note.
const NOTE_LISTED: usize = 4;

/// What an end of a query was taken for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Reading {
    Symbol,
    File,
    Text,
    Words,
}

/// The definitions an end of a query stands for.
pub(super) struct Endpoint {
    pub reading: Reading,
    pub syms: Vec<usize>,
    /// References made outside any definition in the files of the end (the code at the top of a file): where a
    /// chain from the end may also begin.
    pub module_refs: Vec<usize>,
    /// What the text was taken for, when more than one definition stands for it.
    pub note: Option<String>,
}

impl Endpoint {
    pub(super) fn none() -> Self {
        Self {
            reading: Reading::Symbol,
            syms: Vec::new(),
            module_refs: Vec::new(),
            note: None,
        }
    }

    /// True when nothing is named.
    pub(super) fn is_empty(&self) -> bool {
        self.syms.is_empty() && self.module_refs.is_empty()
    }
}

impl Graph {
    /// The end `query` names. A symbol keeps at most `symbols` of its definitions (the most used first).
    pub(super) fn endpoint(&self, query: &str, symbols: usize) -> Endpoint {
        let mut found = self.find(query);

        if !found.is_empty() {
            let total = found.len();

            found.truncate(symbols);

            let note = (found.len() > 1).then(|| {
                let places: Vec<String> = found.iter().take(NOTE_LISTED).map(|sym| self.place(*sym)).collect();

                format!(
                    "`{}` matches {total} definitions, used: {}{}",
                    shown(query),
                    places.join(", "),
                    more(found.len(), NOTE_LISTED)
                )
            });

            return Endpoint {
                reading: Reading::Symbol,
                syms: found,
                module_refs: Vec::new(),
                note,
            };
        }

        let files = self.files_named(query);

        if !files.is_empty() {
            return self.file_endpoint(query, &files);
        }

        let text = self.text_endpoint(query);

        if !text.is_empty() {
            return text;
        }

        self.words_endpoint(query)
    }

    /// The files a text may mean: the one with that path, else the (few) files whose path ends with it.
    fn files_named(&self, query: &str) -> Vec<usize> {
        let readings = self.readings(query);

        for reading in &readings {
            if let Some(&file) = self.path_idx.get(reading) {
                return vec![file];
            }
        }

        // a file name, or the end of a path: only for a text that looks like a source file
        let Some(written) = readings.last().map(|reading| reading.trim_start_matches('/')) else {
            return Vec::new();
        };

        if written.is_empty() || extract::spec_for(written).is_none() {
            return Vec::new();
        }

        let mut files: Vec<usize> = (0..self.files.len())
            .filter(|file| {
                self.files.get(*file).is_some_and(|f| {
                    f.path == written || f.path.strip_suffix(written).is_some_and(|before| before.ends_with('/'))
                })
            })
            .collect();

        files.sort_by_key(|file| self.files.get(*file).map(|f| (f.test, f.path.len(), f.path.as_str())));
        files.truncate(MAX_ENDS_FILES);

        files
    }

    fn file_endpoint(&self, query: &str, files: &[usize]) -> Endpoint {
        let mut syms: Vec<usize> = Vec::new();
        let mut total = 0_usize;

        for &file in files {
            let list = self.file_syms.get(file).map(Vec::as_slice).unwrap_or(&[]);

            total += list.len();
            syms.extend(list.iter().copied());
        }

        // the tests of a file are its definitions too, but production ones lead
        syms.sort_by_key(|sym| (self.syms.get(*sym).is_some_and(|s| s.test), *sym));
        syms.truncate(MAX_FILE_ENDS);

        let module_refs: Vec<usize> = self
            .refs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.from.is_none() && files.contains(&r.file) && r.kind != "type")
            .map(|(ri, _)| ri)
            .collect();
        let paths: Vec<String> = files
            .iter()
            .take(NOTE_LISTED)
            .filter_map(|file| self.files.get(*file))
            .map(|file| shown(&file.path).into_owned())
            .collect();
        let cut = if total > syms.len() {
            format!(", the first {} used", syms.len())
        } else {
            String::new()
        };
        let note = if files.len() > 1 {
            format!(
                "`{}` matches {} files, used: {}{}: {}{cut}",
                shown(query),
                files.len(),
                paths.join(", "),
                more(files.len(), NOTE_LISTED),
                definitions(total)
            )
        } else {
            format!("file {}: {}{cut}", paths.join(", "), definitions(total))
        };

        Endpoint {
            reading: Reading::File,
            syms,
            module_refs,
            note: Some(note),
        }
    }

    /// A string written in the code that names no definition: the definitions that write it.
    fn text_endpoint(&self, query: &str) -> Endpoint {
        let Some(places) = self.loose.get(query) else {
            return Endpoint::none();
        };
        let mut syms: Vec<usize> = places.iter().filter_map(|place| place.from).collect();

        syms.sort_by_key(|sym| (self.syms.get(*sym).is_some_and(|s| s.test), *sym));
        syms.dedup();
        syms.truncate(MAX_MENTION_ENDS);

        let listed: Vec<String> = places
            .iter()
            .take(NOTE_LISTED)
            .filter_map(|place| {
                let file = self.files.get(place.file)?;

                Some(format!("{}:{}", shown(&file.path), place.line))
            })
            .collect();
        let note = format!(
            "`{}` is a string, not a symbol: written in {} ({}{}), {} used",
            shown(query),
            if places.len() == 1 {
                "1 place".to_owned()
            } else {
                format!("{} places", places.len())
            },
            listed.join(", "),
            more(places.len(), NOTE_LISTED),
            definitions(syms.len())
        );

        Endpoint {
            reading: Reading::Text,
            syms,
            module_refs: Vec::new(),
            note: Some(note),
        }
    }

    /// `label path:line` of a definition.
    pub(super) fn place(&self, sym: usize) -> String {
        format!("{} {}", self.label(sym), self.loc(sym))
    }
}

/// `1 definition` or `N definitions`.
fn definitions(count: usize) -> String {
    if count == 1 {
        "1 definition".to_owned()
    } else {
        format!("{count} definitions")
    }
}

/// `, … N more` when `total` is past the `listed` names of a note.
fn more(total: usize, listed: usize) -> String {
    if total > listed {
        format!(", … {} more", total - listed)
    } else {
        String::new()
    }
}
