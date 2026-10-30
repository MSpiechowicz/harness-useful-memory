//! Definitions, references and imports of one source file, read with tree-sitter.
//!
//! Every language is a [`LangSpec`] in [`LANGS`]: its name (stored in `files.lang` and used by
//! [`crate::code::graph`] to pick the resolution rules), the file extensions it owns and the function
//! that fills an [`Extract`]. Adding a language is a new module here, one line in [`LANGS`], and the
//! matching rules in `graph/`.
//!
//! The extractors never panic: a file that does not parse yields what the tree has, and a file whose
//! parse takes too long yields nothing.

use std::borrow::Cow;
use std::cell::Cell;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, Node, ParseOptions, Parser, Tree};

mod go;
mod literals;
mod python;
mod rust;
mod sfc;
mod tags;
mod ts;

/// A parse that takes longer than this gives up (a hostile or generated file must not stall a refresh).
const PARSE_LIMIT: Duration = Duration::from_secs(5);

/// The time [`extract`] gives one file, parses and scans together, when the caller sets no deadline
/// ([`extract_within`]).
const FILE_LIMIT: Duration = Duration::from_secs(20);

thread_local! {
    /// When the file being read must be given up (see [`extract_within`]).
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// True once the time the file being read was given has run out: what is left of it is not read.
pub(super) fn out_of_time() -> bool {
    DEADLINE
        .with(Cell::get)
        .is_some_and(|deadline| Instant::now() >= deadline)
}

/// Puts the previous deadline back when the read ends, however it ends.
struct Restore(Option<Instant>);

impl Drop for Restore {
    fn drop(&mut self) {
        DEADLINE.with(|deadline| deadline.set(self.0));
    }
}

/// What one file may contribute. A generated or hostile file past these keeps its first entries and the
/// rest is dropped (and counted in [`Extract::dropped`]), so one file cannot make the graph quadratic.
pub const MAX_SYMS_PER_FILE: usize = 20_000;
pub const MAX_REFS_PER_FILE: usize = 100_000;
pub const MAX_IMPORTS_PER_FILE: usize = 20_000;
pub const MAX_BINDS_PER_FILE: usize = 50_000;

/// The word literals (see [`word_like`]) one file may hold. They are no code and only rank a text query, so a
/// file past this keeps its first ones and the rest is left out without a count.
pub const MAX_WORDS_PER_FILE: usize = 2_000;

/// Syntax trees deeper than this are not walked further (the walkers recurse).
pub(super) const MAX_DEPTH: usize = 400;

/// The longest string one item may hold, in bytes (names, owners, qualifiers, import sources, types). A
/// longer one is not a code name, and the same long text copied into every item of a file (a method's
/// owner, an import's source) would multiply a megabyte by the item count. An item with a longer string
/// is dropped and counted in [`Extract::dropped`].
pub const MAX_STR: usize = 256;

/// The longest written type that is looked at, in bytes, and how deep generic arguments are followed.
const MAX_TYPE_TEXT: usize = 2_048;
const MAX_TYPE_DEPTH: usize = 12;

/// The index [`push_def`] and [`Extract::add_sym`] answer for a definition that was not kept. Items that
/// belong to it (`from`, `scope`) are dropped too.
pub(super) const DROPPED: usize = usize::MAX;

/// True when `text` is short enough to be stored in an item.
pub(super) fn fits(text: &str) -> bool {
    text.len() <= MAX_STR
}

/// A definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sym {
    pub name: String,
    /// `fn`, `method`, `member` (a function-valued property of an interface or an object literal), `struct`,
    /// `enum`, `enum_member`, `union`, `trait`, `interface`, `class`, `type`, `const`, `static`, `macro` or
    /// `component`.
    pub kind: &'static str,
    /// The owning type or trait of a method or associated item.
    pub parent: Option<String>,
    /// First line, 1-based.
    pub line: usize,
    pub end_line: usize,
    /// The definition's first line, trimmed and shortened.
    pub sig: String,
    /// The written return type (filled by the receiver-type speed-up).
    pub ret: Option<String>,
    /// Inside `#[cfg(test)]` / `#[test]`.
    pub test: bool,
}

/// A reference to a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// Index into [`Extract::syms`] of the enclosing definition; `None` at module level.
    pub from: Option<usize>,
    pub name: String,
    /// `call`, `mcall`, `macro`, `type`, `new`, `component`, `str` (a string literal naming a symbol), `use`,
    /// `fnref` (a function passed as a value), `prop` (a function kept in a property of an object literal:
    /// `name` is the property, `qual` the value as written, `recv` the function the literal is passed to),
    /// `base` (a type derives from or implements another: `name` is the base, `qual` the derived type, see
    /// [`push_base`]) or `word` (a short plain-word string literal inside a definition, see [`word_like`]).
    pub kind: &'static str,
    /// `git` in `git::diff()`, `Env` in `Env::new()`, `Foo` in `Foo.bar()`.
    pub qual: Option<String>,
    /// The receiver variable of a method call (filled by the receiver-type speed-up).
    pub recv: Option<String>,
    pub line: usize,
    /// A mention of a constant (a `use`, or a Go name qualified by its package) written where something is
    /// handled: a `case` label, an operand of an equality test or the pattern of an `if let` (see [`handled`]).
    /// False for everything else.
    pub handler: bool,
}

/// A name brought into a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The name as used in this file.
    pub local: String,
    /// The name at the source (`*` for a namespace or package import).
    pub orig: String,
    /// A Rust path (`crate::git::diff`), a TS module specifier (`./x`, `$lib/y`) or a Go import path.
    pub source: String,
}

/// A variable, parameter or field with a written type (filled by the receiver-type speed-up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// Index into [`Extract::syms`] of the owning scope; `None` for a file-level binding.
    pub scope: Option<usize>,
    pub name: String,
    pub ty: String,
    /// `var`, `param`, `field` or `let`.
    pub kind: &'static str,
}

/// Everything read from one file.
#[derive(Debug, Default, Clone)]
pub struct Extract {
    pub syms: Vec<Sym>,
    pub refs: Vec<Ref>,
    pub imports: Vec<Import>,
    pub binds: Vec<Bind>,
    /// Entries left out because the file went past the per-file caps.
    pub dropped: usize,
    /// How many `word` references the file holds (they have a cap of their own).
    words: usize,
}

impl Extract {
    /// Keeps the definition `sym` and returns its index; [`DROPPED`] (and a count) when the file already
    /// holds [`MAX_SYMS_PER_FILE`] or a name is too long.
    pub(super) fn add_sym(&mut self, mut sym: Sym) -> usize {
        if self.syms.len() >= MAX_SYMS_PER_FILE || !fits(&sym.name) || !sym.parent.as_deref().is_none_or(fits) {
            self.dropped += 1;

            return DROPPED;
        }

        cut(&mut sym.sig);
        sym.ret = sym.ret.filter(|ret| fits(ret));
        self.syms.push(sym);

        self.syms.len() - 1
    }

    /// Keeps the reference `reference`; counts it instead when the file already holds
    /// [`MAX_REFS_PER_FILE`], a name is too long or the definition it is in was not kept.
    pub(super) fn add_ref(&mut self, mut reference: Ref) {
        // a word literal outside any definition says nothing about one, and past its cap it is only left out
        if reference.kind == "word" {
            if reference.from.is_none_or(|from| from == DROPPED) || self.words >= MAX_WORDS_PER_FILE {
                return;
            }

            // nor does one written in a definition named for a credential (`const password = ...`)
            if reference
                .from
                .and_then(|from| self.syms.get(from))
                .is_some_and(|sym| credential_named(&sym.name))
            {
                return;
            }

            self.words += 1;
        }

        if self.refs.len() >= MAX_REFS_PER_FILE
            || reference.from == Some(DROPPED)
            || reference.name.is_empty()
            || !fits(&reference.name)
            || !reference.qual.as_deref().is_none_or(fits)
        {
            self.dropped += 1;

            return;
        }

        // the receiver only speeds the resolution up: without it the reference is still a call
        reference.recv = reference.recv.filter(|recv| fits(recv));
        self.refs.push(reference);
    }

    /// Keeps the import `import`; counts it instead when the file already holds [`MAX_IMPORTS_PER_FILE`] or
    /// a string is too long.
    pub(super) fn add_import(&mut self, import: Import) {
        if self.imports.len() >= MAX_IMPORTS_PER_FILE || !import_fits(&import) {
            self.dropped += 1;

            return;
        }

        self.imports.push(import);
    }

    /// Keeps the binding `bind`; counts it instead when the file already holds [`MAX_BINDS_PER_FILE`], a
    /// string is too long or the scope it is in was not kept.
    pub(super) fn add_bind(&mut self, bind: Bind) {
        if self.binds.len() >= MAX_BINDS_PER_FILE || bind.scope == Some(DROPPED) || !fits(&bind.name) || !fits(&bind.ty)
        {
            self.dropped += 1;

            return;
        }

        self.binds.push(bind);
    }

    /// Cuts every list to its per-file cap and drops imports that bypassed [`Extract::add_import`] with a
    /// string that is too long, then counts what went.
    fn cap(&mut self) {
        let before = self.syms.len() + self.refs.len() + self.imports.len() + self.binds.len();

        self.imports.retain(import_fits);
        self.syms.truncate(MAX_SYMS_PER_FILE);
        self.refs.truncate(MAX_REFS_PER_FILE);
        self.imports.truncate(MAX_IMPORTS_PER_FILE);
        self.binds.truncate(MAX_BINDS_PER_FILE);

        self.dropped += before - (self.syms.len() + self.refs.len() + self.imports.len() + self.binds.len());
    }
}

fn import_fits(import: &Import) -> bool {
    fits(&import.local) && fits(&import.orig) && fits(&import.source)
}

/// Shortens `text` to [`MAX_STR`] bytes at a character boundary.
fn cut(text: &mut String) {
    if text.len() <= MAX_STR {
        return;
    }

    let mut end = MAX_STR;

    while !text.is_char_boundary(end) {
        end -= 1;
    }

    text.truncate(end);
}

/// One language the index reads.
pub struct LangSpec {
    /// The name stored in `files.lang`; the graph picks its resolution rules by it.
    pub name: &'static str,
    /// Lowercase file extensions without the dot.
    pub exts: &'static [&'static str],
    /// Reads `src` (the file `path`, relative) into the extract.
    pub run: fn(&mut Parser, &str, &str, &mut Extract),
}

/// The languages, in the order their extensions are matched.
pub static LANGS: &[LangSpec] = &[
    LangSpec {
        name: "rust",
        exts: &["rs"],
        run: rust::extract,
    },
    LangSpec {
        name: "ts",
        exts: &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        run: ts::extract,
    },
    LangSpec {
        name: "ts",
        exts: &["svelte", "vue", "astro"],
        run: sfc::extract,
    },
    LangSpec {
        name: "go",
        exts: &["go"],
        run: go::extract,
    },
    LangSpec {
        name: "python",
        exts: &["py"],
        run: python::extract,
    },
];

/// The language that owns the file `path`, by extension (compared in lowercase).
pub fn spec_for(path: &str) -> Option<&'static LangSpec> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();

    LANGS.iter().find(|spec| spec.exts.contains(&ext.as_str()))
}

/// Reads the source of the file `path` with the language that owns it (nothing for an unknown one).
pub fn extract(parser: &mut Parser, path: &str, src: &str) -> Extract {
    extract_within(parser, path, src, Instant::now() + FILE_LIMIT)
}

/// [`extract`], giving up on what is left of the file at `deadline` (the end of the call's time budget):
/// a parse waits at most [`PARSE_LIMIT`] or what remains, and the scans between parses stop at it.
pub fn extract_within(parser: &mut Parser, path: &str, src: &str, deadline: Instant) -> Extract {
    let _restore = Restore(DEADLINE.with(|slot| slot.replace(Some(deadline))));
    let mut out = Extract::default();

    if let Some(spec) = spec_for(path) {
        (spec.run)(parser, path, src, &mut out);
        out.cap();

        // a string written in a definition's first line (`const KEY = "..."`, a default, a one-line body) is
        // not kept
        for sym in &mut out.syms {
            if let Cow::Owned(elided) = literals::elide_strings(&sym.sig, spec.name) {
                sym.sig = elided;
            }
        }
    }

    out
}

/// `line`, a line of source in the language `lang` (a `files.lang` name), with the content of its string
/// literals replaced like the one of a signature. A line that begins inside a literal that started on an
/// earlier line is not told from code: the caller leaves out a line it cannot trust.
pub fn elide_line<'a>(line: &'a str, lang: &str) -> Cow<'a, str> {
    literals::elide_strings(line, lang)
}

/// Parses `src` as `language`; `None` when the language cannot be set or the parse takes too long (longer
/// than [`PARSE_LIMIT`], or than what is left of the file's time).
pub(super) fn parse(parser: &mut Parser, language: &Language, src: &str) -> Option<Tree> {
    if out_of_time() {
        return None;
    }

    parser.set_language(language).ok()?;

    let bytes = src.as_bytes();
    let started = Instant::now();
    let mut give_up = |_: &tree_sitter::ParseState| {
        if started.elapsed() > PARSE_LIMIT || out_of_time() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };

    let tree = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or(&[]),
        None,
        Some(ParseOptions::new().progress_callback(&mut give_up)),
    );

    if tree.is_none() {
        parser.reset();
    }

    tree
}

/// The source text of a node (empty when it is not valid UTF-8).
pub(super) fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

/// The first line of a node, trimmed, without a trailing `{`, shortened to 140 characters.
pub(super) fn first_line(node: Node, src: &[u8]) -> String {
    let line = text(node, src).lines().next().unwrap_or("").trim();
    let line = line.trim_end_matches('{').trim_end();

    // at most 140 characters, and at most MAX_STR bytes (a line of wide characters, or no line break in a
    // megabyte, must not be kept whole)
    let mut sig = String::new();

    for (count, c) in line.chars().enumerate() {
        if count == 140 || sig.len() + c.len_utf8() > MAX_STR - '…'.len_utf8() {
            sig.push('…');

            return sig;
        }

        sig.push(c);
    }

    sig
}

/// The most characters a string may have to be taken for a code name.
const IDENT_MAX: usize = 64;

/// The most digits a code name may hold: `http2_handler` and `sha256_of` do, a random key does not.
const IDENT_DIGITS: usize = 2;

/// Strings that look like a code name (`git_file_lines`, `fetchDiffs`), not prose or keys like `id`. A string
/// that holds a credential format (`sk_live_...`, `ghp_...`), is longer than a name, or has the digits of a
/// random key is not one: it is stored as a reference (`str`) in the index file, so a secret that happens to
/// be written like a name must not reach it.
pub fn ident_like(s: &str) -> bool {
    s.len() >= 5
        && s.len() <= IDENT_MAX
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && (s.contains('_') || s.chars().skip(1).any(|c| c.is_ascii_uppercase()))
        && s.chars().filter(char::is_ascii_digit).count() <= IDENT_DIGITS
        && !crate::support::redact::has_credentials(s)
}

/// The longest word literal, in bytes.
const WORD_MAX: usize = 32;

/// A phrase of this many parts joined by `-` that is longer than [`PHRASE_CHARS`] is a passphrase, not a command
/// (`correct-horse-battery`).
const PHRASE_PARTS: usize = 3;
const PHRASE_CHARS: usize = 20;

/// True for a string literal that is one plain lowercase word (`doctor`, `git-status`) or a short command
/// (`/useful-skills doctor`, `git status`): lowercase ASCII letters and `-`, an optional leading `/`, at most
/// two parts. A word literal holds no digit, capital letter or symbol, and not a long run of words joined by
/// `-` (a passphrase), so it can hardly be a key or a token. It is stored as a `word` reference of the
/// definition it is written in and ranks a text query; it is never shown in a signature.
pub fn word_like(s: &str) -> bool {
    if s.len() > WORD_MAX {
        return false;
    }

    if s.len() > PHRASE_CHARS && s.split('-').count() >= PHRASE_PARTS {
        return false;
    }

    let part = |part: &str, least: usize| {
        part.len() >= least
            && part.starts_with(|c: char| c.is_ascii_lowercase())
            && part.chars().all(|c| c.is_ascii_lowercase() || c == '-')
    };

    match s.strip_prefix('/').unwrap_or(s).split_once(' ') {
        None => part(s, 3) || (s.starts_with('/') && part(&s[1..], 2)),
        Some((first, second)) => part(first, 2) && part(second, 2),
    }
}

/// Parts of a name that say its value is a secret.
const CREDENTIAL_PARTS: [&str; 7] = ["password", "passwd", "secret", "token", "key", "credential", "auth"];

/// True when `name` is that of a credential (`dbPassword`, `API_KEY`, `auth_header`), without regard to case.
pub(super) fn credential_named(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();

    CREDENTIAL_PARTS.iter().any(|part| lower.contains(part))
}

/// The fields of a node that hold the name a value is bound to (`const NAME = v`, `NAME = v`, `{ NAME: v }`,
/// `let NAME = v`, `f(NAME=v)`).
const BINDING_FIELDS: [&str; 5] = ["name", "left", "key", "pattern", "field"];

/// The most ancestors looked at, and the longest name text read.
const BINDING_DEPTH: usize = 8;
const BINDING_NAME_BYTES: usize = 120;

/// True when the string `node` is bound to a name of a credential: it is the value of a declaration, an
/// assignment, a member of an object or an argument by keyword whose name holds `password`, `secret`, `token`,
/// `key`, `credential` or `auth`.
pub(super) fn in_credential_binding(node: Node, src: &[u8]) -> bool {
    let mut child = node;

    for _ in 0..BINDING_DEPTH {
        let Some(parent) = child.parent() else {
            return false;
        };

        for field in BINDING_FIELDS {
            let Some(name) = parent.child_by_field_name(field).filter(|name| name.id() != child.id()) else {
                continue;
            };

            let written = text(name, src);

            if written.len() <= BINDING_NAME_BYTES && credential_named(written) {
                return true;
            }
        }

        // a keyed element of Go (`Key: value`) has no field names
        if parent.kind() == "keyed_element"
            && parent
                .named_child(0)
                .filter(|key| key.id() != child.id())
                .is_some_and(|key| text(key, src).len() <= BINDING_NAME_BYTES && credential_named(text(key, src)))
        {
            return true;
        }

        child = parent;
    }

    false
}

/// Appends the string literal `value` (the `node` of the tree) as a `word` reference of the definition `from`
/// when it is [`word_like`] and is bound to no name of a credential (a definition with one is checked by
/// [`Extract::add_ref`]).
pub(super) fn push_word(out: &mut Extract, from: Option<usize>, value: &str, node: Node, src: &[u8]) {
    if !word_like(value) {
        return;
    }

    if in_credential_binding(node, src) {
        return;
    }

    push_ref(out, from, value, "word", None, node.start_position().row + 1);
}

/// True when `node` is the `name` field of its parent (the defining occurrence, not a use).
pub(super) fn is_own_name(node: Node) -> bool {
    node.parent()
        .and_then(|parent| parent.child_by_field_name("name"))
        .is_some_and(|name| name.id() == node.id())
}

/// The last `::` segment of a path.
pub(super) fn last_seg(path: &str) -> String {
    path.rsplit("::").next().unwrap_or(path).trim().to_string()
}

/// Where a definition is, relative to the file it came from (script blocks start below line 1).
pub(super) struct Placement<'a> {
    pub src: &'a [u8],
    pub line_off: usize,
    pub test: bool,
}

/// Appends the definition `node` named by `name` and returns its index ([`DROPPED`] when it was not kept).
pub(super) fn push_def(
    out: &mut Extract,
    node: Node,
    kind: &'static str,
    name: Node,
    parent: Option<&str>,
    place: &Placement,
) -> usize {
    // the lengths first: a long owner is not copied into every method of it
    if !fits(text(name, place.src)) || !parent.is_none_or(fits) {
        out.dropped += 1;

        return DROPPED;
    }

    out.add_sym(Sym {
        name: text(name, place.src).to_string(),
        kind,
        parent: parent.map(str::to_string),
        line: node.start_position().row + place.line_off + 1,
        end_line: node.end_position().row + place.line_off + 1,
        sig: first_line(node, place.src),
        ret: None,
        test: place.test,
    })
}

/// Appends a reference to a non-empty `name`.
pub(super) fn push_ref(
    out: &mut Extract,
    from: Option<usize>,
    name: &str,
    kind: &'static str,
    qual: Option<String>,
    line: usize,
) {
    push_ref_recv(out, from, name, kind, qual, None, line);
}

/// Appends the fact that the type `derived` extends, implements or derives from the type `base` (a Python
/// base class, a TS `extends` or `implements`, a Rust `impl Trait for Type`). It is no use of the base: the
/// graph only reads it to let a call on a base method also reach the overrides.
pub(super) fn push_base(out: &mut Extract, derived: &str, base: &str, line: usize) {
    if derived.is_empty() || derived == base {
        return;
    }

    push_ref(out, None, base, "base", Some(derived.to_string()), line);
}

/// Appends a reference with the expression its receiver was written as (a method call, or a qualified call).
pub(super) fn push_ref_recv(
    out: &mut Extract,
    from: Option<usize>,
    name: &str,
    kind: &'static str,
    qual: Option<String>,
    recv: Option<String>,
    line: usize,
) {
    if name.is_empty() {
        return;
    }

    // the length first: a long name is not copied
    if !fits(name) {
        out.dropped += 1;

        return;
    }

    out.add_ref(Ref {
        from,
        name: name.to_string(),
        kind,
        qual,
        recv,
        line,
        handler: false,
    });
}

/// Appends a mention of a constant, `handler` saying that it is written where something is handled.
pub(super) fn push_use(
    out: &mut Extract,
    from: Option<usize>,
    name: &str,
    qual: Option<String>,
    node: Node,
    src: &[u8],
) {
    if name.is_empty() {
        return;
    }

    if !fits(name) {
        out.dropped += 1;

        return;
    }

    out.add_ref(Ref {
        from,
        name: name.to_string(),
        kind: "use",
        qual,
        recv: None,
        line: node.start_position().row + 1,
        handler: handled(node, src),
    });
}

/// How far up from a mention [`handled`] looks.
const MAX_HANDLER_CLIMB: usize = 8;

/// Nodes that only wrap a name or a list of names on the way to a `case` label or a comparison.
const WRAPPERS: &[&str] = &[
    "parenthesized_expression",
    "expression_list",
    "member_expression",
    "selector_expression",
    "attribute",
    "dotted_name",
    "scoped_identifier",
    "tuple_struct_pattern",
    "tuple_pattern",
    "struct_pattern",
    "or_pattern",
    "reference_pattern",
    "field_pattern",
    "union_pattern",
    "class_pattern",
    "tuple",
    "list",
    "set",
    "array",
];

/// True when the mention `node` of a constant is written where something is handled, not where it is sent or
/// passed on: the label of a `case` (Go and TS `switch`, Go `select`, a Rust `match` arm, a Python `match`), an
/// operand of an equality test (`==`, `!=`, `===`, `!==`, and `in` and `is` in Python) or the pattern of a
/// Rust `if let`. An event is set in one definition and handled in another: only the handling one is where
/// the event leads to.
pub(super) fn handled(node: Node, src: &[u8]) -> bool {
    let mut child = node;

    for _ in 0..MAX_HANDLER_CLIMB {
        let Some(parent) = child.parent() else {
            return false;
        };
        let within = |field: &str| {
            parent
                .child_by_field_name(field)
                .is_some_and(|inner| inner.id() == child.id())
        };

        match parent.kind() {
            "expression_case" | "switch_case" => return within("value"),
            "communication_case" => return within("communication") && child.kind() != "send_statement",
            "match_pattern" => return !within("condition"),
            "let_condition" => return within("pattern"),
            "case_pattern" => return true,
            "binary_expression" => {
                return parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| matches!(text(op, src), "==" | "!=" | "===" | "!=="));
            }
            "comparison_operator" => {
                let mut cursor = parent.walk();

                return parent
                    .children(&mut cursor)
                    .any(|op| matches!(op.kind(), "==" | "!=" | "in" | "not in" | "is" | "is not"));
            }
            kind if WRAPPERS.contains(&kind) => {}
            _ => return false,
        }

        child = parent;
    }

    false
}

/// Appends a variable, parameter or field whose type is written (`ty`) or follows from `=expr`.
pub(super) fn push_bind(out: &mut Extract, scope: Option<usize>, name: &str, ty: &str, kind: &'static str) {
    if name.is_empty() || ty.is_empty() || ty == "=" {
        return;
    }

    if !fits(name) || !fits(ty) {
        out.dropped += 1;

        return;
    }

    out.add_bind(Bind {
        scope,
        name: name.to_string(),
        ty: ty.to_string(),
        kind,
    });
}

/// Appends a method call with the expression its receiver was written as.
pub(super) fn push_mcall(
    out: &mut Extract,
    from: Option<usize>,
    name: &str,
    qual: Option<String>,
    recv: Option<String>,
    line: usize,
) {
    push_ref_recv(out, from, name, "mcall", qual, recv, line);
}

/// Type names that say nothing: the receiver may be anything.
const OPEN_TYPES: &[&str] = &[
    "any",
    "unknown",
    "object",
    "never",
    "void",
    "Any",
    "_",
    "interface{}",
    "T",
];

/// Generic types whose first argument is what iterating them yields.
const ITERABLES: &[&str] = &[
    "Vec",
    "VecDeque",
    "Option",
    "HashSet",
    "BTreeSet",
    "Array",
    "ReadonlyArray",
    "list",
    "List",
    "Set",
    "set",
    "Iterable",
    "Sequence",
    "Iterator",
    "frozenset",
    "Deque",
    "Tuple",
    "tuple",
];

/// Wrappers whose methods are reached through the type inside (smart pointers, promises, optional fields).
const TRANSPARENT: &[&str] = &[
    "Box",
    "Rc",
    "Arc",
    "Cow",
    "Pin",
    "Promise",
    "PromiseLike",
    "Awaited",
    "Readonly",
    "Optional",
    "Final",
    "ClassVar",
    "Required",
    "NonNullable",
];

/// The generic arguments of `rest` (`<A, B>` or `[A, B]`), split at the top level.
fn generic_args(rest: &str) -> Vec<&str> {
    let rest = rest.trim();
    let Some(open) = rest.chars().next().filter(|c| matches!(c, '<' | '[')) else {
        return Vec::new();
    };
    let close = if open == '<' { '>' } else { ']' };
    let mut depth = 0_usize;
    let mut start = 1;
    let mut args = Vec::new();

    for (i, c) in rest.char_indices() {
        match c {
            '<' | '[' | '(' | '{' => depth += 1,
            '>' | ']' | ')' | '}' => {
                depth = depth.saturating_sub(1);

                if depth == 0 && c == close {
                    args.push(rest.get(start..i).unwrap_or(""));

                    return args;
                }
            }
            ',' if depth == 1 => {
                args.push(rest.get(start..i).unwrap_or(""));
                start = i + 1;
            }
            _ => {}
        }
    }

    args
}

/// A written type reduced to the name of the type whose methods a value has: pointers, references and
/// smart pointers are looked through, generics and paths dropped (`*store.Store` -> `Store`,
/// `Promise<Subject>` -> `Subject`, `Option<Foo>` -> `Option<Foo>`, `[]Foo` -> `slice<Foo>`). What iterating
/// the type yields stays in angle brackets after its name. `None` when the text says nothing (`any`,
/// `unknown`, a union of several types, a function type).
pub(super) fn written_type(raw: &str) -> Option<String> {
    written_type_at(raw, 0).filter(|ty| fits(ty))
}

/// [`written_type`] at generic depth `depth`. Text over [`MAX_TYPE_TEXT`] bytes, or nested deeper than
/// [`MAX_TYPE_DEPTH`], says nothing: the scans below are quadratic in the text and recurse with it.
fn written_type_at(raw: &str, depth: usize) -> Option<String> {
    if depth > MAX_TYPE_DEPTH || raw.len() > MAX_TYPE_TEXT {
        return None;
    }

    let mut text = raw.trim().trim_start_matches("->").trim_start_matches(':').trim();

    text = text.trim_matches(['"', '\'']);

    // `Foo | null`, `Foo | None`: the one type that is not nothing
    let parts = top_level_split(text, '|');

    if parts.len() > 1 {
        let kept: Vec<&str> = parts
            .into_iter()
            .map(str::trim)
            .filter(|part| !matches!(*part, "null" | "undefined" | "None" | "nil"))
            .collect();

        return match kept.as_slice() {
            [one] => written_type_at(one, depth + 1),
            _ => None,
        };
    }

    loop {
        let before = text;

        for prefix in [
            "&",
            "*",
            "mut ",
            "dyn ",
            "impl ",
            "readonly ",
            "const ",
            "...",
            "?",
            "<-",
            "await ",
        ] {
            if let Some(rest) = text.strip_prefix(prefix) {
                text = rest.trim_start();
            }
        }

        if text.starts_with('\'')
            && let Some((_, rest)) = text.split_once(' ')
        {
            text = rest.trim_start(); // a lifetime: &'a Foo
        }

        if text == before {
            break;
        }
    }

    if let Some(rest) = text.strip_prefix('[') {
        // Rust `[T]` / `[T; N]`, Go `[]T` / `[N]T`
        let element = match rest.split_once(']') {
            Some(("", after)) => after,
            Some((inside, after)) if inside.chars().all(|c| c.is_ascii_digit()) => after,
            _ => rest.split([';', ']']).next().unwrap_or(""),
        };

        return Some(with_element("slice", written_type_at(element, depth + 1)));
    }

    if let Some(element) = text.strip_suffix("[]")
        && !text.contains('<')
    {
        return Some(with_element("slice", written_type_at(element, depth + 1)));
    }

    if text.starts_with("map[") {
        return Some("map".to_string());
    }

    if text.starts_with("chan ") || text.starts_with("chan<-") {
        return Some("chan".to_string());
    }

    if text.starts_with("interface{") || text.starts_with("interface {") || text.starts_with("struct{") {
        return None;
    }

    if text.starts_with("func(") || text.starts_with("fn(") || text.starts_with('(') || text.starts_with('{') {
        return None;
    }

    let end = text
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '$' | ':' | '.')))
        .unwrap_or(text.len());
    let (path, rest) = text.split_at(end);
    let name = path.rsplit([':', '.']).next().unwrap_or(path);

    if name.is_empty() || OPEN_TYPES.contains(&name) {
        return None;
    }

    if TRANSPARENT.contains(&name)
        && let Some(inner) = generic_args(rest).first()
    {
        return written_type_at(inner, depth + 1);
    }

    if rest.trim_start().starts_with("[]") {
        return Some(with_element("slice", written_type_at(name, depth + 1)));
    }

    if ITERABLES.contains(&name) {
        let element = generic_args(rest)
            .first()
            .and_then(|inner| written_type_at(inner, depth + 1));

        return Some(with_element(name, element));
    }

    Some(name.to_string())
}

/// `name<element>`, or `name` when the element is not known.
fn with_element(name: &str, element: Option<String>) -> String {
    match element {
        Some(element) => format!("{name}<{element}>"),
        None => name.to_string(),
    }
}

/// `text` split at `separator` outside brackets.
fn top_level_split(text: &str, separator: char) -> Vec<&str> {
    let mut depth = 0_usize;
    let mut start = 0;
    let mut parts = Vec::new();

    for (i, c) in text.char_indices() {
        match c {
            '<' | '[' | '(' | '{' => depth += 1,
            '>' | ']' | ')' | '}' => depth = depth.saturating_sub(1),
            c if c == separator && depth == 0 => {
                parts.push(text.get(start..i).unwrap_or(""));
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(text.get(start..).unwrap_or(""));

    parts
}

/// A constant-looking name: `EvtSearchNew`, `RATE_PERCENT`, `Color` (what it names decides at resolution).
pub(super) fn constant_like(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(
            name,
            "Some" | "None" | "Ok" | "Err" | "Self" | "String" | "Vec" | "Box" | "Option" | "Result"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str, src: &str) -> Extract {
        extract(&mut Parser::new(), path, src)
    }

    /// Whether each mention of `name` in `src` is where something is handled, in order.
    fn handlers(path: &str, src: &str, name: &str) -> Vec<bool> {
        run(path, src)
            .refs
            .into_iter()
            .filter(|r| r.name == name && r.kind != "base")
            .map(|r| r.handler)
            .collect()
    }

    #[test]
    fn a_constant_is_handled_in_a_case_label_or_an_equality_test_and_only_passed_on_elsewhere() {
        let go = "package p\nimport \"example.com/events\"\nfunc f(k string) {\n\tswitch k {\n\tcase events.Ping, Pong:\n\t}\n\tselect {\n\tcase ch <- Pong:\n\t}\n\tif k != Pong {\n\t}\n\tlog(Pong)\n\tx := events.Ping\n}\n";

        // Pong: case label, a send in select, `!=`, an argument. events.Ping: case label, an assignment
        assert_eq!(handlers("a.go", go, "Pong"), [true, false, true, false]);
        assert_eq!(handlers("a.go", go, "Ping"), [true, false]);

        let ts = "function f(k) {\n  switch (k) {\n    case Evt.New:\n    case PING:\n  }\n  if (k === PING || k !== Evt.New) {}\n  log(PING, Evt.New);\n}\n";

        assert_eq!(handlers("a.ts", ts, "PING"), [true, true, false]);
        assert_eq!(handlers("a.ts", ts, "New"), [true, true, false]);

        let py = "def f(k):\n    match k:\n        case Color.RED | Color.BLUE:\n            pass\n    if k == PING or k in (PING, PONG) or k is not PONG:\n        pass\n    log(PING)\n";

        assert_eq!(handlers("a.py", py, "PING"), [true, true, false]);
        assert_eq!(handlers("a.py", py, "RED"), [true]);
        assert_eq!(handlers("a.py", py, "PONG"), [true, true]);

        let rs = "fn f(k: u8) {\n    match k {\n        PING => {}\n        Evt::New(_) if g(PING) => {}\n        _ => {}\n    }\n    if let Evt::Done = k {}\n    if k == PING {}\n    run(PING, Evt::New);\n}\n";

        assert_eq!(handlers("a.rs", rs, "PING"), [true, false, true, false]);
        assert_eq!(handlers("a.rs", rs, "New"), [true, false]);
        assert_eq!(handlers("a.rs", rs, "Done"), [true]);
    }

    /// The signature stored for the definition named `name` in `src`.
    fn sig_of(path: &str, src: &str, name: &str) -> String {
        run(path, src)
            .syms
            .into_iter()
            .find(|sym| sym.name == name)
            .unwrap_or_else(|| panic!("no {name} in {path}"))
            .sig
    }

    #[test]
    fn the_value_of_a_string_constant_is_not_kept_in_its_signature() {
        let hunter = ["hunter", "2"].concat();
        let cases = [
            (
                "a.rs",
                format!("const K: &str = \"{hunter}\";\n"),
                "K",
                "const K: &str = \"…\";",
            ),
            (
                "a.rs",
                format!("pub static TOKEN: &str = r#\"{hunter}\"#;\n"),
                "TOKEN",
                "pub static TOKEN: &str = \"…\";",
            ),
            (
                "a.ts",
                format!("export const token = \"{hunter}\";\n"),
                "token",
                "token = \"…\"",
            ),
            (
                "a.ts",
                format!("const secret = '{hunter}'\n"),
                "secret",
                "secret = \"…\"",
            ),
            (
                "a.py",
                format!("API_KEY = \"{hunter}\"\n"),
                "API_KEY",
                "API_KEY = \"…\"",
            ),
            (
                "a.go",
                format!("package a\n\nconst Key = \"{hunter}\"\n"),
                "Key",
                "Key = \"…\"",
            ),
            (
                "a.go",
                format!("package a\n\nconst Raw = `{hunter}`\n"),
                "Raw",
                "Raw = \"…\"",
            ),
        ];

        for (path, src, name, expected) in cases {
            let sig = sig_of(path, &src, name);

            assert!(!sig.contains(&hunter), "{path}: {sig}");
            assert_eq!(sig, expected, "{path}");
        }
    }

    #[test]
    fn a_literal_of_a_type_stays_in_the_signature_and_one_of_a_value_does_not() {
        assert_eq!(
            sig_of("a.ts", "export type Tone = 'neutral' | 'danger';\n", "Tone"),
            "type Tone = 'neutral' | 'danger';"
        );
        assert_eq!(
            sig_of(
                "a.ts",
                "export function tone(m: 'a' = 'b'): 'c' { return 'd' }\n",
                "tone"
            ),
            "function tone(m: 'a' = \"…\"): 'c' { return \"…\" }"
        );
        assert_eq!(
            sig_of("a.py", "def connect(mode=\"fast\"):\n    pass\n", "connect"),
            "def connect(mode=\"…\"):"
        );
    }

    /// Every signature of `src` (in the language of `path`) free of `hunter`.
    fn assert_no_literal_in_any_signature(path: &str, src: &str, hunter: &str) {
        let src = src.replace("HUNTER", hunter);
        let out = run(path, &src);

        assert!(!out.syms.is_empty(), "{path}: nothing was found in {src}");

        for sym in &out.syms {
            assert!(!sym.sig.contains(hunter), "{path}: {} -> {}", sym.name, sym.sig);
        }
    }

    #[test]
    fn no_signature_keeps_the_text_of_a_string_whatever_the_shape_of_the_definition() {
        let hunter = ["hunter", "2"].concat();
        let cases: &[(&str, &str)] = &[
            // one-line functions and methods
            ("a.rs", "pub fn key() -> &'static str { \"HUNTER\" }\n"),
            (
                "a.rs",
                "struct A;\nimpl A { pub fn token(&self) -> String { \"HUNTER\".into() } }\n",
            ),
            ("a.rs", "pub fn raw() -> &'static str { r#\"HUNTER\"x\"# }\n"),
            ("a.rs", "pub fn bytes() -> &'static [u8] { br\"HUNTER\" }\n"),
            ("a.rs", "macro_rules! make { () => { \"HUNTER\" }; }\n"),
            ("a.go", "package a\n\nfunc Key() string { return \"HUNTER\" }\n"),
            (
                "a.go",
                "package a\n\ntype S struct{}\n\nfunc (s S) Token() string { return `HUNTER` }\n",
            ),
            ("a.go", "package a\n\ntype S struct { Name string `json:\"HUNTER\"` }\n"),
            ("a.ts", "export function key() { return 'HUNTER'; }\n"),
            ("a.ts", "export const key = () => 'HUNTER';\n"),
            ("a.ts", "export const key = (x: string) => `HUNTER ${x}`;\n"),
            ("a.ts", "export const key = () => `a ${f(`HUNTER`)} b`;\n"),
            (
                "a.ts",
                "export const config = { token: 'HUNTER', other() { return \"HUNTER\"; } };\n",
            ),
            (
                "a.ts",
                "export class Client { static token = 'HUNTER'; send() { return \"HUNTER\"; } }\n",
            ),
            ("a.ts", "export enum Key { Live = 'HUNTER' }\n"),
            ("a.py", "def connect(password=\"HUNTER\", user='HUNTER'): pass\n"),
            ("a.py", "def greet(name): return f\"HUNTER {name}\"\n"),
            (
                "a.py",
                "def greet(name): return f\"{name} {data[\"HUNTER\"]} {name!r:>{width}}\"\n",
            ),
            ("a.py", "class Client: token = \"\"\"HUNTER\"\"\"\n"),
            ("a.py", "def key(): return rb'HUNTER'\n"),
        ];

        for (path, src) in cases {
            assert_no_literal_in_any_signature(path, src, &hunter);
        }
    }

    #[test]
    fn a_character_literal_is_not_taken_for_the_start_of_a_string() {
        assert_eq!(sig_of("a.rs", "const Q: char = '\"';\n", "Q"), "const Q: char = '\"';");
        assert_eq!(
            sig_of("a.rs", "pub fn q<'a>(x: &'a str) -> char { '\"' }\n", "q"),
            "pub fn q<'a>(x: &'a str) -> char { '\"' }"
        );
        assert_eq!(sig_of("a.go", "package a\n\nconst Q = '\"'\n", "Q"), "Q = '\"'");
    }

    #[test]
    fn extensions_pick_the_language_in_lowercase() {
        assert_eq!(spec_for("a/b/lib.rs").map(|s| s.name), Some("rust"));
        assert_eq!(spec_for("a/App.TSX").map(|s| s.name), Some("ts"));
        assert_eq!(spec_for("ui/Card.svelte").map(|s| s.name), Some("ts"));
        assert_eq!(spec_for("main.go").map(|s| s.name), Some("go"));
        assert_eq!(spec_for("pkg/mod.PY").map(|s| s.name), Some("python"));
        assert!(spec_for("README.md").is_none());
        assert!(spec_for("Makefile").is_none());
    }

    #[test]
    fn written_types_reduce_to_the_name_of_the_type() {
        let cases = [
            ("*store.Store", Some("Store")),
            ("&mut Foo", Some("Foo")),
            ("&'a Foo<T>", Some("Foo")),
            ("Promise<Subject[]>", Some("slice<Subject>")),
            ("Promise<Subject>", Some("Subject")),
            ("Arc<Mutex<Foo>>", Some("Mutex")),
            ("Option<Foo>", Some("Option<Foo>")),
            ("&[String]", Some("slice<String>")),
            ("Vec<Vec<u8>>", Some("Vec<Vec<u8>>")),
            ("Foo | null", Some("Foo")),
            ("Foo | Bar", None),
            ("Optional[Foo]", Some("Foo")),
            ("\"Foo\"", Some("Foo")),
            ("[]string", Some("slice<string>")),
            ("map[string]Foo", Some("map")),
            ("chan events.Kind", Some("chan")),
            ("any", None),
            ("interface{}", None),
            ("(a: number) => void", None),
            (": ApplicationApiClient", Some("ApplicationApiClient")),
            ("m.Request", Some("Request")),
            ("crate::git::Diff", Some("Diff")),
        ];

        for (raw, wanted) in cases {
            assert_eq!(written_type(raw).as_deref(), wanted, "{raw}");
        }
    }

    #[test]
    fn constant_like_names_skip_the_prelude() {
        assert!(constant_like("RATE_PERCENT"));
        assert!(constant_like("EvtSearchNew"));
        assert!(!constant_like("x"));
        assert!(!constant_like("rate"));
        assert!(!constant_like("None"));
        assert!(!constant_like("A"));
    }

    #[test]
    fn identifier_like_strings_are_code_names() {
        assert!(ident_like("git_file_lines"));
        assert!(ident_like("fetchDiffs"));
        assert!(!ident_like("id"));
        assert!(!ident_like("hello world"));
        assert!(!ident_like("plainword"));
        assert!(ident_like("http2_handler"));
        // a key written like a name, a random mixed-case word, a string longer than a name
        assert!(!ident_like(&["sk_", "live_", "4eC39HqLyjWDarjtT1zdp7dc"].concat()));
        assert!(!ident_like(&["gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4"].concat()));
        assert!(!ident_like("xQ9fLm3vTz7bKd2R_aWp"));
        assert!(!ident_like(&format!("a_{}", "b".repeat(IDENT_MAX))));
    }

    #[test]
    fn garbage_sources_do_not_panic() {
        let junk = [
            "",
            "\0\0\0",
            "fn (((((",
            "}}}}{{{{",
            "<<<<>>>>",
            "\u{feff}\u{1F600}\u{1F600}",
        ];
        let deep = format!("{}1{}", "(".repeat(20_000), ")".repeat(20_000));

        for path in [
            "a.rs", "a.ts", "a.tsx", "a.js", "a.go", "a.py", "a.svelte", "a.vue", "a.astro",
        ] {
            for source in junk.iter().copied().chain([deep.as_str()]) {
                let _ = run(path, source);
            }
        }
    }

    /// Every string an extract holds, in bytes.
    fn stored_bytes(out: &Extract) -> usize {
        let opt = |text: &Option<String>| text.as_ref().map_or(0, String::len);
        let syms: usize = out
            .syms
            .iter()
            .map(|s| s.name.len() + s.sig.len() + opt(&s.parent) + opt(&s.ret))
            .sum();
        let refs: usize = out
            .refs
            .iter()
            .map(|r| r.name.len() + opt(&r.qual) + opt(&r.recv))
            .sum();
        let imports: usize = out
            .imports
            .iter()
            .map(|i| i.local.len() + i.orig.len() + i.source.len())
            .sum();
        let binds: usize = out.binds.iter().map(|b| b.name.len() + b.ty.len()).sum();

        syms + refs + imports + binds
    }

    /// The longest string any item of `out` holds.
    fn longest(out: &Extract) -> usize {
        let opt = |text: &Option<String>| text.as_ref().map_or(0, String::len);

        out.syms
            .iter()
            .flat_map(|s| [s.name.len(), s.sig.len(), opt(&s.parent), opt(&s.ret)])
            .chain(out.refs.iter().flat_map(|r| [r.name.len(), opt(&r.qual), opt(&r.recv)]))
            .chain(
                out.imports
                    .iter()
                    .flat_map(|i| [i.local.len(), i.orig.len(), i.source.len()]),
            )
            .chain(out.binds.iter().flat_map(|b| [b.name.len(), b.ty.len()]))
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn a_long_name_is_dropped_once_not_copied_into_every_item_that_uses_it() {
        let long = "A".repeat(20_000);
        let shapes = [
            // a `use` prefix, an `impl` owner, an import source, a class owner
            ("a.rs", format!("use {long}::{{{}}};\nfn f(){{}}\n", "x,".repeat(5_000))),
            ("a.rs", format!("impl {long} {{{}}}\n", "fn a(){}".repeat(5_000))),
            ("a.rs", format!("trait {long} {{{}}}\n", "fn a(){}".repeat(5_000))),
            ("a.ts", format!("import {{{}}} from \"{long}\";\n", "x,".repeat(5_000))),
            ("a.py", format!("class {long}:\n{}", " def a(s): s.b=1\n".repeat(5_000))),
            ("a.py", format!("from {long} import {}\n", "x, ".repeat(5_000))),
            (
                "a.go",
                format!("package p\ntype {long} struct{{\n{}}}\n", "a int\n".repeat(5_000)),
            ),
        ];

        for (path, source) in shapes {
            let out = run(path, &source);

            assert!(
                longest(&out) <= MAX_STR,
                "{path}: {} bytes in one string",
                longest(&out)
            );
            assert!(stored_bytes(&out) < 1_000_000, "{path}: {} bytes", stored_bytes(&out));
            assert!(out.dropped > 0, "{path}: nothing was counted as dropped");
        }
    }

    #[test]
    fn short_names_beside_a_long_one_are_kept() {
        let long = "A".repeat(20_000);
        let out = run(
            "a.rs",
            &format!("use {long}::x;\nuse crate::b::kept;\nstruct {long};\nfn f() {{ g(); }}\n"),
        );

        assert!(out.imports.iter().any(|i| i.local == "kept"));
        assert!(out.syms.iter().any(|s| s.name == "f"));
        assert!(out.refs.iter().any(|r| r.name == "g"));
        assert!(out.syms.iter().all(|s| s.name != long));
    }

    #[test]
    fn the_per_file_caps_hold_while_items_are_pushed() {
        let out = run("a.rs", &"fn x(){x()}\n".repeat(MAX_SYMS_PER_FILE + 1_000));

        assert_eq!(out.syms.len(), MAX_SYMS_PER_FILE);
        assert!(out.refs.len() <= MAX_REFS_PER_FILE);
        assert!(out.dropped >= 1_000);
        // a call inside a definition that was not kept has nowhere to belong: it goes with it
        assert!(out.refs.iter().all(|r| r.from.is_none_or(|from| from < out.syms.len())));
    }

    #[test]
    fn deeply_nested_and_endlessly_repeated_types_are_read_at_once() {
        let deep_ts = format!("{}number{}", "Promise<".repeat(110_000), ">".repeat(110_000));
        let deep_py = format!("{}int{}", "Optional[".repeat(90_000), "]".repeat(90_000));
        let arrays = format!("Foo{}", "[]".repeat(480_000));
        let started = Instant::now();

        for text in [&deep_ts, &deep_py, &arrays] {
            assert!(written_type(text).is_none(), "{} bytes", text.len());
        }

        // and a file that holds them is read whole
        for (path, source) in [
            ("a.ts", format!("let x: {deep_ts};\n")),
            ("a.py", format!("x: {deep_py} = 1\n")),
            ("a.ts", format!("let x: {arrays};\n")),
        ] {
            let _ = run(path, &source);
        }

        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[test]
    fn written_types_within_the_limits_still_reduce() {
        let nested = format!("{}Foo{}", "Promise<".repeat(8), ">".repeat(8));

        assert_eq!(written_type(&nested).as_deref(), Some("Foo"));
        assert_eq!(written_type("Foo[][][]").as_deref(), Some("slice<slice<slice<Foo>>>"));
    }

    #[test]
    fn word_literals_are_plain_lowercase_words_and_short_commands() {
        for word in [
            "doctor",
            "git-status",
            "/doctor",
            "/useful-skills doctor",
            "git status",
            "harness-memory",
        ] {
            assert!(word_like(word), "{word}");
        }

        // a digit, a capital letter, a symbol, more than two parts or more than 32 bytes: never a word literal
        for other in [
            "id",
            "Doctor",
            "doctor2",
            "a1b2c3d4",
            "ab_cd",
            "x y z",
            "/a/b",
            "a  b",
            "",
            "-doctor",
            "a.b",
            &"a".repeat(33),
            &["gh", "p", "ABCD1234abcd5678"].concat(),
        ] {
            assert!(!word_like(other), "{other}");
        }

        assert!(word_like(&"a".repeat(32)));

        // whatever is accepted holds letters, dashes, one slash at the start and one space between two parts
        let alphabet: Vec<char> = "abz-/ 09AZ_.".chars().collect();
        let mut seen = 0_usize;

        for length in 1..=5_u32 {
            for n in 0..alphabet.len().pow(length) {
                let text: String = (0..length)
                    .map(|i| alphabet[n / alphabet.len().pow(i) % alphabet.len()])
                    .collect();

                if word_like(&text) {
                    seen += 1;
                    assert!(
                        text.chars()
                            .all(|c| c.is_ascii_lowercase() || matches!(c, '-' | '/' | ' ')),
                        "{text}"
                    );
                }
            }
        }

        assert!(seen > 100, "{seen}");
    }

    #[test]
    fn word_literals_are_kept_for_the_definition_that_writes_them_in_every_language() {
        let cases = [
            (
                "a.ts",
                "export function run(a) {\n  return a === \"doctor\" ? \"/skills doctor\" : \"Doctor9\";\n}\n",
            ),
            (
                "a.py",
                "def run(a):\n    return \"doctor\" if a else \"/skills doctor\" + \"Doctor9\"\n",
            ),
            (
                "a.go",
                "package a\n\nfunc run(a int) string {\n\tx := \"doctor\"\n\ty := \"/skills doctor\"\n\ty += \"Doctor9\"\n\treturn x + y\n}\n",
            ),
            (
                "a.rs",
                "pub fn run(a: i32) -> &'static str {\n    let y = \"/skills doctor\";\n    let z = \"Doctor9\";\n    if a > 0 { \"doctor\" } else { y }\n}\n",
            ),
            (
                "A.svelte",
                "<script lang=\"ts\">\n  export function run(a) {\n    return a === \"doctor\" ? \"/skills doctor\" : \"Doctor9\";\n  }\n</script>\n",
            ),
        ];

        for (path, source) in cases {
            let out = run(path, source);
            let owner = out.syms.iter().position(|sym| sym.name == "run").expect("run");
            let mut words: Vec<&str> = out
                .refs
                .iter()
                .filter(|r| r.kind == "word")
                .inspect(|r| assert_eq!(r.from, Some(owner), "{path}"))
                .map(|r| r.name.as_str())
                .collect();

            words.sort_unstable();

            assert_eq!(words, ["/skills doctor", "doctor"], "{path}");
        }
    }

    #[test]
    fn word_literals_outside_a_definition_or_in_markup_attributes_are_left_out() {
        let top = run("a.ts", "register(\"doctor\");\nimport x from \"react-dom\";\n");

        assert!(top.refs.iter().all(|r| r.kind != "word"), "{:?}", top.refs);

        let jsx = run(
            "a.tsx",
            "export function A() {\n  return <div className=\"flex-col\" data-x=\"doctor\" />;\n}\n",
        );

        assert!(jsx.refs.iter().all(|r| r.kind != "word"), "{:?}", jsx.refs);
    }

    #[test]
    fn a_passphrase_or_a_word_bound_to_a_credential_is_no_word_literal() {
        assert!(!word_like("correct-horse-battery"));
        assert!(!word_like("correct-horse-battery-staple"));
        assert!(word_like("harness-memory"));
        assert!(word_like("git-file-lines"));

        let cases = [
            (
                "a.ts",
                "export function run(o) {\n  const dbPassword = \"hunter\";\n  o.apiKey = \"hunter\";\n  f({ authHeader: \"hunter\" });\n  return o === \"doctor\";\n}\n",
            ),
            (
                "a.py",
                "def run(o):\n    db_password = \"hunter\"\n    connect(secret=\"hunter\")\n    return o == \"doctor\"\n",
            ),
            (
                "a.rs",
                "pub fn run(o: &str) -> bool {\n    let token = \"hunter\";\n    let c = Cfg { api_key: \"hunter\" };\n    o == \"doctor\"\n}\n",
            ),
            (
                "a.go",
                "package a\n\nfunc run(o string) bool {\n\tpassword := \"hunter\"\n\tc := Cfg{Secret: \"hunter\"}\n\treturn o == \"doctor\"\n}\n",
            ),
        ];

        for (path, source) in cases {
            let out = run(path, source);
            let words: Vec<&str> = out
                .refs
                .iter()
                .filter(|r| r.kind == "word")
                .map(|r| r.name.as_str())
                .collect();

            assert_eq!(words, ["doctor"], "{path}");
        }

        // a definition named for a credential holds none either
        let owner = run("a.ts", "export function setPassword() {\n  return \"doctor\";\n}\n");

        assert!(owner.refs.iter().all(|r| r.kind != "word"), "{:?}", owner.refs);
    }

    #[test]
    fn word_literals_are_never_shown_in_a_signature() {
        let sig = sig_of(
            "a.ts",
            "export const mode = pick(\"doctor\", \"/skills doctor\");\n",
            "mode",
        );

        assert!(!sig.contains("doctor"), "{sig}");
        assert_eq!(
            sig_of("a.rs", "const MODE: &str = \"doctor\";\n", "MODE"),
            "const MODE: &str = \"…\";"
        );
    }

    #[test]
    fn a_file_keeps_at_most_the_cap_of_word_literals_and_counts_none_as_dropped() {
        let calls: String = (0..MAX_WORDS_PER_FILE + 50).map(|_| "  f(\"doctor\");\n").collect();
        let out = run("a.ts", &format!("export function run() {{\n{calls}}}\n"));

        assert_eq!(out.refs.iter().filter(|r| r.kind == "word").count(), MAX_WORDS_PER_FILE);
        assert_eq!(out.dropped, 0);
    }

    #[test]
    fn a_file_read_after_its_deadline_gives_nothing_and_the_deadline_is_put_back() {
        let source = "fn f() {}\n";
        let spent = Instant::now();
        let none = extract_within(&mut Parser::new(), "a.rs", source, spent);

        assert!(none.syms.is_empty());
        assert!(!out_of_time(), "the deadline of the call leaked into the next read");
        assert_eq!(run("a.rs", source).syms.len(), 1);
    }

    #[test]
    fn a_component_with_thousands_of_script_blocks_stops_at_the_deadline() {
        let source = "<script>const a = 1;</script>\n".repeat(20_000);
        let started = Instant::now();
        let out = extract_within(&mut Parser::new(), "A.svelte", &source, Instant::now());

        assert!(out.dropped > 0 || out.syms.len() <= 1);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }
}
