//! The names a single-file component imports, used inside its template expressions.
//!
//! The script (or frontmatter) says what the component imports, but a name such as `alternatePath` may be
//! used only in the template: `<a href={alternatePath(locale)}>` (Astro, Svelte) or `{{ format(x) }}` and
//! `:title="label(x)"` (Vue). Only the expression regions are read, and in them only the names the component
//! imports: HTML text, other identifiers and the contents of strings are not.
//!
//! The template is the blanked copy `sfc` makes, so byte offsets and line numbers are the file's own.

use std::collections::HashSet;

use super::sfc::SCAN_CHECK;
use super::{Extract, out_of_time, push_ref, push_ref_recv};

/// How deep expressions, template literals and markup may nest in one another (`` `${ `${x}` }` ``, an
/// element in an expression in an element): what is deeper is left unread.
const MAX_NESTING: usize = 32;

/// Elements without a closing tag.
const VOID_ELEMENTS: &[&[u8]] = &[
    b"area", b"base", b"br", b"col", b"embed", b"hr", b"img", b"input", b"link", b"meta", b"source", b"track", b"wbr",
];

/// Where an expression ends.
#[derive(Clone, Copy)]
enum Closer {
    /// The `}` of `{ ... }`.
    Brace,
    /// The `}}` of `{{ ... }}`.
    Mustache,
    /// The quote that ends an attribute value.
    Quote(u8),
}

impl Closer {
    fn len(self) -> usize {
        match self {
            Closer::Mustache => 2,
            Closer::Brace | Closer::Quote(_) => 1,
        }
    }
}

/// An imported name used at a byte of the template.
struct Hit {
    at: usize,
    name: String,
    /// The namespace of `ns.member`.
    qual: Option<String>,
    call: bool,
}

/// Records the uses and calls of the names `out` has imported in the expressions of `markup`; `vue` reads
/// `{{ }}` and the `:`, `@` and `v-` attributes, the others read `{ }`.
pub(super) fn extract(markup: &[u8], vue: bool, owner: Option<usize>, out: &mut Extract) {
    let (namespaces, values): (Vec<_>, Vec<_>) = out
        .imports
        .iter()
        // a re-export (`export:name`) brings nothing into the file
        .filter(|import| !import.local.contains(':'))
        .partition(|import| import.orig == "*");
    let values: HashSet<String> = values.into_iter().map(|import| import.local.clone()).collect();
    let namespaces: HashSet<String> = namespaces.into_iter().map(|import| import.local.clone()).collect();

    if values.is_empty() && namespaces.is_empty() {
        return;
    }

    let mut scan = Scan {
        markup,
        vue,
        values,
        namespaces,
        next_check: 0,
        stopped: false,
        hits: Vec::new(),
    };

    scan.template();

    if scan.stopped {
        out.dropped += 1;
    }

    // a reference the script already recorded for a line is not recorded again, nor twice in the template
    let mut seen: HashSet<(String, &str, Option<String>, usize)> = out
        .refs
        .iter()
        .filter(|r| matches!(r.kind, "use" | "call" | "mcall"))
        .map(|r| (r.name.clone(), r.kind, r.qual.clone(), r.line))
        .collect();
    let mut cursor = (0_usize, 1_usize);

    for hit in scan.hits {
        // the hits come in order, so the lines before each one are counted once
        if let Some(between) = markup.get(cursor.0..hit.at) {
            cursor = (hit.at, cursor.1 + between.iter().filter(|b| **b == b'\n').count());
        }

        let line = cursor.1;
        let kind = match (hit.call, &hit.qual) {
            (false, _) => "use",
            (true, None) => "call",
            (true, Some(_)) => "mcall",
        };

        if !seen.insert((hit.name.clone(), kind, hit.qual.clone(), line)) {
            continue;
        }

        match kind {
            "call" => push_ref(out, owner, &hit.name, "call", None, line),
            "mcall" => {
                let recv = hit.qual.clone();

                push_ref_recv(out, owner, &hit.name, "mcall", hit.qual, recv, line);
            }
            _ => push_ref_recv(out, owner, &hit.name, "use", hit.qual, None, line),
        }
    }
}

struct Scan<'a> {
    markup: &'a [u8],
    vue: bool,
    /// Default and named imports, by their local names.
    values: HashSet<String>,
    /// Namespace imports (`import * as ns`).
    namespaces: HashSet<String>,
    /// The byte at which the clock is looked at next.
    next_check: usize,
    /// True once the time of the file ran out: the rest is not read.
    stopped: bool,
    hits: Vec<Hit>,
}

impl Scan<'_> {
    /// True once the time ran out; the clock is looked at every [`SCAN_CHECK`] bytes.
    fn out_of_time(&mut self, at: usize) -> bool {
        if !self.stopped && at >= self.next_check {
            self.next_check = at + SCAN_CHECK;
            self.stopped = out_of_time();
        }

        self.stopped
    }

    /// Finds the expressions of the template and reads each.
    fn template(&mut self) {
        let mut at = 0;

        while let Some(&byte) = self.markup.get(at) {
            if self.out_of_time(at) {
                return;
            }

            at = match byte {
                b'<' if self.markup.get(at..).is_some_and(|rest| rest.starts_with(b"<!--")) => self.after(at, b"-->"),
                b'<' if self.vue && self.markup.get(at + 1).is_some_and(u8::is_ascii_alphabetic) => self.tag(at),
                b'{' if self.vue => {
                    if self.markup.get(at + 1) == Some(&b'{') {
                        self.read(at + 2, Closer::Mustache)
                    } else {
                        at + 1
                    }
                }
                b'{' => self.read(at + 1, Closer::Brace),
                _ => at + 1,
            };
        }
    }

    /// The offset past the next `end` at or after `from`, or the end of the template.
    fn after(&self, from: usize, end: &[u8]) -> usize {
        self.markup
            .get(from..)
            .and_then(|rest| rest.windows(end.len()).position(|window| window == end))
            .map_or(self.markup.len(), |offset| from + offset + end.len())
    }

    /// Reads an expression and returns the offset past it. One that never closes is no expression: the rest
    /// of the template is left unread (looking for the next `{` would read to the end again for each).
    fn read(&mut self, from: usize, closer: Closer) -> usize {
        let kept = self.hits.len();

        match self.expression(from, closer, 0) {
            Some(end) => end,
            None => {
                self.hits.truncate(kept);

                self.markup.len()
            }
        }
    }

    /// A Vue tag: the values of its `:`, `@` and `v-` attributes are expressions, the others are text.
    fn tag(&mut self, from: usize) -> usize {
        let markup = self.markup;
        let blank = |at: usize| markup.get(at).is_some_and(u8::is_ascii_whitespace);
        let mut at = from + 1;

        while markup.get(at).is_some_and(|b| !b.is_ascii_whitespace() && *b != b'>') {
            at += 1;
        }

        loop {
            while blank(at) {
                at += 1;
            }

            if self.out_of_time(at) {
                return markup.len();
            }

            match markup.get(at) {
                None => return markup.len(),
                Some(b'>') => return at + 1,
                // `a<b then {{ x }}`: the `<b` was text, and a mustache is no attribute
                Some(b'{') if markup.get(at + 1) == Some(&b'{') => return at,
                _ => {}
            }

            let start = at;

            while markup
                .get(at)
                .is_some_and(|b| !b.is_ascii_whitespace() && !matches!(b, b'=' | b'>'))
            {
                at += 1;
            }

            if at == start {
                at += 1;

                continue;
            }

            let name = markup.get(start..at).unwrap_or(&[]);
            let dynamic = name.starts_with(b":") || name.starts_with(b"@") || name.starts_with(b"v-");
            let mut value = at;

            while blank(value) {
                value += 1;
            }

            if markup.get(value) != Some(&b'=') {
                continue;
            }

            value += 1;

            while blank(value) {
                value += 1;
            }

            match markup.get(value) {
                None => return markup.len(),
                Some(&quote) if matches!(quote, b'"' | b'\'') => {
                    at = if dynamic {
                        self.read(value + 1, Closer::Quote(quote))
                    } else {
                        self.after(value + 1, &[quote])
                    };
                }
                Some(_) => {
                    at = value;

                    while markup.get(at).is_some_and(|b| !b.is_ascii_whitespace() && *b != b'>') {
                        at += 1;
                    }
                }
            }
        }
    }

    /// Reads code from `from` to the `closer` outside any brace, string or comment; returns the offset past
    /// the closer, or `None` when the code never closes or the time ran out.
    fn expression(&mut self, from: usize, closer: Closer, nesting: usize) -> Option<usize> {
        let markup = self.markup;
        let mut at = from;
        let mut depth = 0_usize;

        while let Some(&byte) = markup.get(at) {
            if self.out_of_time(at) {
                return None;
            }

            let next = markup.get(at + 1).copied();

            match byte {
                _ if depth == 0 && closes(closer, markup, at) => return Some(at + closer.len()),
                b'{' => depth += 1,
                b'}' => depth = depth.saturating_sub(1),
                b'\'' | b'"' => {
                    at = string_end(markup, at, byte);

                    continue;
                }
                b'`' => {
                    at = self.template_literal(at, nesting)?;

                    continue;
                }
                // markup in the expression (`items.map((i) => <li>{i}</li>)`) is no code: Vue has none
                b'<' if !self.vue && opens_element(markup, at) => {
                    at = self.element(at, nesting)?;

                    continue;
                }
                b'/' if next == Some(b'/') => {
                    at = markup
                        .get(at..)
                        .and_then(|rest| rest.iter().position(|b| *b == b'\n'))
                        .map_or(markup.len(), |offset| at + offset);

                    continue;
                }
                b'/' if next == Some(b'*') => {
                    at = self.after(at + 2, b"*/");

                    continue;
                }
                // a number (`1e5`, `0xff`) holds letters that are no names
                b'0'..=b'9' => {
                    while markup.get(at).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_') {
                        at += 1;
                    }

                    continue;
                }
                _ if ident_byte(byte) => {
                    at = self.name(at);

                    continue;
                }
                _ => {}
            }

            at += 1;
        }

        None
    }

    /// The offset past the element that opens at `from` (a `<`). Its tag and attribute names and its text are
    /// no code: only the `{ }` in them are read, as expressions.
    fn element(&mut self, from: usize, nesting: usize) -> Option<usize> {
        if nesting >= MAX_NESTING {
            return None;
        }

        let markup = self.markup;
        let blank = |at: usize| markup.get(at).is_some_and(u8::is_ascii_whitespace);
        let mut at = from + 1;
        let name_start = at;

        while markup
            .get(at)
            .is_some_and(|b| !b.is_ascii_whitespace() && !matches!(b, b'>' | b'/'))
        {
            at += 1;
        }

        let void = markup
            .get(name_start..at)
            .is_some_and(|name| VOID_ELEMENTS.contains(&name.to_ascii_lowercase().as_slice()));

        // the attributes
        loop {
            while blank(at) {
                at += 1;
            }

            if self.out_of_time(at) {
                return None;
            }

            match markup.get(at)? {
                b'>' => break,
                b'/' if markup.get(at + 1) == Some(&b'>') => return Some(at + 2),
                // `{...props}`
                b'{' => {
                    at = self.expression(at + 1, Closer::Brace, nesting + 1)?;

                    continue;
                }
                _ => {}
            }

            let start = at;

            while markup.get(at).is_some_and(|b| {
                !b.is_ascii_whitespace()
                    && !matches!(b, b'=' | b'>')
                    && !(*b == b'/' && markup.get(at + 1) == Some(&b'>'))
            }) {
                at += 1;
            }

            if at == start {
                at += 1;

                continue;
            }

            let mut value = at;

            while blank(value) {
                value += 1;
            }

            if markup.get(value) != Some(&b'=') {
                continue;
            }

            value += 1;

            while blank(value) {
                value += 1;
            }

            match markup.get(value)? {
                &quote @ (b'"' | b'\'') => at = self.after(value + 1, &[quote]),
                b'{' => at = self.expression(value + 1, Closer::Brace, nesting + 1)?,
                // `href=http://x`: up to the next blank
                _ => {
                    at = value;

                    while markup.get(at).is_some_and(|b| !b.is_ascii_whitespace() && *b != b'>') {
                        at += 1;
                    }
                }
            }
        }

        at += 1;

        // a closing tag (`</x>`) has no children; neither has a void element
        if void || markup.get(from + 1) == Some(&b'/') {
            return Some(at);
        }

        // the children: text, expressions and elements, up to the closing tag
        loop {
            if self.out_of_time(at) {
                return None;
            }

            match markup.get(at)? {
                b'{' => at = self.expression(at + 1, Closer::Brace, nesting + 1)?,
                b'<' if markup.get(at..).is_some_and(|rest| rest.starts_with(b"<!--")) => at = self.after(at, b"-->"),
                b'<' if markup.get(at + 1) == Some(&b'/') => {
                    return Some(self.after(at, b">"));
                }
                b'<' if markup
                    .get(at + 1)
                    .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'>') =>
                {
                    at = self.element(at, nesting + 1)?;
                }
                _ => at += 1,
            }
        }
    }

    /// The offset past the template literal that opens at `from`; the `${ }` in it are code.
    fn template_literal(&mut self, from: usize, nesting: usize) -> Option<usize> {
        let markup = self.markup;
        let mut at = from + 1;

        while let Some(&byte) = markup.get(at) {
            if self.out_of_time(at) {
                return None;
            }

            match byte {
                b'\\' => at += 2,
                b'`' => return Some(at + 1),
                b'$' if markup.get(at + 1) == Some(&b'{') => {
                    if nesting >= MAX_NESTING {
                        return None;
                    }

                    at = self.expression(at + 2, Closer::Brace, nesting + 1)?;
                }
                _ => at += 1,
            }
        }

        None
    }

    /// Reads the name at `from` and records it when the component imports it; returns the offset past what
    /// was read.
    fn name(&mut self, from: usize) -> usize {
        let markup = self.markup;
        let end = ident_end(markup, from);
        let Some(word) = markup.get(from..end).and_then(|word| std::str::from_utf8(word).ok()) else {
            return end;
        };

        if !self.names_a_value(from) {
            return end;
        }

        if self.values.contains(word) {
            let call = calls(markup, end);

            self.hits.push(Hit {
                at: from,
                name: word.to_string(),
                qual: None,
                call,
            });

            return end;
        }

        // `ns.member` of `import * as ns` names an export of that module
        if self.namespaces.contains(word) && markup.get(end) == Some(&b'.') {
            let member_end = ident_end(markup, end + 1);
            let member = markup
                .get(end + 1..member_end)
                .and_then(|m| std::str::from_utf8(m).ok());

            if let Some(member) = member.filter(|member| !member.is_empty()) {
                let call = calls(markup, member_end);

                self.hits.push(Hit {
                    at: from,
                    name: member.to_string(),
                    qual: Some(word.to_string()),
                    call,
                });

                return member_end;
            }
        }

        end
    }

    /// False for a name that is a property (`obj.name`). A tag (`<Name>`) is never read here: `<Foo>` is
    /// already recorded as a component, and the markup of an expression is skipped by [`Scan::element`].
    fn names_a_value(&self, at: usize) -> bool {
        let before = |back: usize| at.checked_sub(back).and_then(|i| self.markup.get(i)).copied();

        match before(1) {
            Some(b'.') => before(3) == Some(b'.') && before(2) == Some(b'.'),
            _ => true,
        }
    }
}

/// True when the `<` at `at` opens an element, not a comparison (`a < b`, `a<b`) or the generics of a call
/// (`useState<string>()`): a tag follows no operand.
fn opens_element(markup: &[u8], at: usize) -> bool {
    if !markup
        .get(at + 1)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'>')
    {
        return false;
    }

    let Some(before) = markup
        .get(..at)
        .and_then(|head| head.iter().rposition(|b| !b.is_ascii_whitespace()))
    else {
        return true;
    };

    match markup.get(before) {
        Some(b')' | b']' | b'"' | b'\'' | b'`') => false,
        // `return <li />` follows a word, `count <li` does not
        Some(b) if ident_byte(*b) => markup.get(..=before).is_some_and(|head| head.ends_with(b"return")),
        _ => true,
    }
}

/// True when `closer` ends the code at `at`.
fn closes(closer: Closer, markup: &[u8], at: usize) -> bool {
    match closer {
        Closer::Brace => markup.get(at) == Some(&b'}'),
        Closer::Mustache => markup.get(at..at + 2) == Some(b"}}"),
        Closer::Quote(quote) => markup.get(at) == Some(&quote),
    }
}

/// A byte of a name: a non-ASCII byte belongs to one, so a name is cut on a character boundary.
fn ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
}

fn ident_end(markup: &[u8], from: usize) -> usize {
    let mut at = from;

    while markup.get(at).is_some_and(|b| ident_byte(*b)) {
        at += 1;
    }

    at
}

/// The offset past the string that opens at `from`. A string ends at its line: a quote in the text of a
/// markup element inside the expression (`It's`) opens no string that spans lines.
fn string_end(markup: &[u8], from: usize, quote: u8) -> usize {
    let mut at = from + 1;

    while let Some(&byte) = markup.get(at) {
        match byte {
            b'\\' => at += 2,
            b'\n' => return at,
            _ if byte == quote => return at + 1,
            _ => at += 1,
        }
    }

    markup.len()
}

/// True when a `(` follows the offset, after optional whitespace.
fn calls(markup: &[u8], from: usize) -> bool {
    markup
        .get(from..)
        .and_then(|rest| rest.iter().find(|b| !b.is_ascii_whitespace()))
        == Some(&b'(')
}
