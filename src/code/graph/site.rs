//! The place of a reference in the source, and the line of source that makes it: what an agent would open the
//! file for.
//!
//! The line is read from the file when the answer is written, not stored: only while the file on disk is
//! what the index holds (same size and modification time), so a line number never points into an edited
//! file, and only through the safe read of [`walk::read_lines`] (a link, a large file or a file outside the
//! root is not read). The comments of the line are replaced (`// …`, `/* … */`, `# …`: a note holds what it
//! holds), the string literals are replaced like those of a signature, and the text is written out like any
//! text of the repository. A line that cannot be told with certainty from the inside of a string or of a
//! comment (a template, a triple-quoted string, a string that goes on from the line above or below, a brace of
//! an f-string, a quote that closes more than it opens) is left out: the place is still given.

use std::path::Path;

use super::{Graph, shown, site_scan};
use crate::code::{extract, walk};

/// The most characters of a source line in an answer.
const SITE_CHARS: usize = 120;

impl Graph {
    /// `path:line` of the reference `ri`.
    pub(super) fn site(&self, ri: usize) -> String {
        match self.refs.get(ri) {
            Some(r) => format!(
                "{}:{}",
                shown(self.files.get(r.file).map_or("", |f| f.path.as_str())),
                r.line
            ),
            None => String::new(),
        }
    }

    /// `path:line | source line` of the reference `ri`, or only the place when the line cannot be shown.
    pub(super) fn site_with_text(&self, ri: usize) -> String {
        match self.site_text(ri) {
            Some(text) => format!("{} | {text}", self.site(ri)),
            None => self.site(ri),
        }
    }

    /// The line of source of the reference `ri`, trimmed to [`SITE_CHARS`] characters.
    pub(super) fn site_text(&self, ri: usize) -> Option<String> {
        let r = self.refs.get(ri)?;
        let file = self.files.get(r.file)?;
        let mut lines = walk::read_lines(
            Path::new(&self.root),
            &file.path,
            file.mtime_ns,
            file.size,
            r.line,
            site_scan::CONTEXT_LINES,
        )?;
        let raw = lines.pop().filter(|raw| !raw.is_empty())?;
        let masked = readable(file.lang, &lines, &raw, &r.name, &r.kind)?;
        let elided = extract::elide_line(&masked, file.lang);
        let squeezed = elided.split_whitespace().collect::<Vec<_>>().join(" ");
        let escaped = shown(&squeezed);

        if escaped.chars().count() <= SITE_CHARS {
            return Some(escaped.into_owned());
        }

        let mut cut: String = escaped.chars().take(SITE_CHARS - 1).collect();

        cut.push('…');

        Some(cut)
    }
}

/// The line `raw`, `above` being the lines before it, with its comments replaced; `None` when it cannot be told
/// with certainty to start in code and to hold the reference `name` in code: it is then not shown, only its
/// place. The line of a reference does start in code, unless it sits in the middle of a literal that began above
/// it (a string that goes on, a template, a triple-quoted string, a hole of an f-string), or in a comment.
fn readable(lang: &str, above: &[String], raw: &str, name: &str, kind: &str) -> Option<String> {
    if raw.contains('`') {
        return None;
    }

    if lang == "python" && (raw.contains("\"\"\"") || raw.contains("'''")) {
        return None;
    }

    // a line that begins inside a string would show the end of the string as if it were code
    let start = match site_scan::state_after(lang, above) {
        site_scan::State::Code => site_scan::State::Code,
        site_scan::State::Block => site_scan::State::Block,
        site_scan::State::Quote { .. } | site_scan::State::Raw { .. } => return None,
    };
    let scanned = site_scan::scan_line(lang, raw, start);

    // a string left open on the line, or an odd quote: the strings would not be told from the code
    if scanned.broken
        || matches!(
            scanned.end,
            site_scan::State::Quote { .. } | site_scan::State::Raw { .. }
        )
    {
        return None;
    }

    if start == site_scan::State::Code && site_scan::first_quote_closes(&scanned) {
        return None;
    }

    // the name is code (a string that names a definition is the reference itself)
    if kind != "str" && !name.is_empty() {
        let mut found = scanned.text.match_indices(name).peekable();
        let in_string = |at: usize| scanned.strings.iter().any(|(from, to)| *from <= at && at < *to);

        if found.peek().is_some() && found.all(|(at, _)| in_string(at)) {
            return None;
        }
    }

    // inside braces before the name: a hole of an f-string, or a dict (blanked all the same)
    if lang == "python" {
        let before = scanned
            .text
            .find(name)
            .map_or(scanned.text.as_str(), |at| scanned.text.get(..at).unwrap_or(""));
        let depth = before.matches('{').count() as i64 - before.matches('}').count() as i64;

        if depth > 0 {
            return None;
        }
    }

    Some(scanned.text)
}
