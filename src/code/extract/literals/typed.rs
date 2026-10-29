//! The scan of TypeScript/JavaScript and Python signatures: a string literal in a type position stays, one in a
//! value position is replaced.
//!
//! `type Mode = "fast" | "slow"`, `m: "x"` and `Literal["a", "b"]` say what a definition accepts, and are shown.
//! `= "secret"`, a default argument, a call argument, an object member value, an enum value, a template or an
//! f-string is a value, and is not. The scan is lexical (no parse): it follows the brackets of the line and, per
//! bracket level, whether the cursor is in a type segment (after the `:` of an annotation up to the `=`, `,` or
//! closing bracket, after `type X =`, after `as`/`satisfies`, inside the `<...>` of generic arguments) or in a
//! value. Whatever it cannot place is a value: a literal that is unclear is blanked.

use super::{ELIDED, Quotes, string_end};

/// The language of the file, as far as the scan differs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lang {
    /// TypeScript and JavaScript (also the script of a Svelte, Vue or Astro file).
    Ts,
    Python,
}

/// What the line starts like.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Head {
    /// `interface X { ... }`: all of it is a type.
    Interface,
    /// `type X = ...`: what follows the `=` is a type.
    Alias,
    Class,
    Function,
    /// A variable, a field with a modifier, an enum: a `:` is an annotation.
    Decl,
    /// A line that starts with a name (`key: ...`, `method(...)`), or an expression.
    Bare,
    /// A Python line that starts with a keyword (`def`, `class`, `if`): its last `:` starts a block.
    Block,
}

/// Where in a bracket level the cursor is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seg {
    Value,
    /// After the `:` of an annotation (or a Python `->`), up to a `=`, `,`, `;` or the closing bracket.
    Annotation,
    /// After `as` or `satisfies`.
    Assertion,
    /// After the `=` of `type X =`, up to a `;`.
    Alias,
}

/// One level of brackets.
struct Frame {
    /// `(`, `[`, `{`, `<`, or `\0` for the line itself.
    open: char,
    /// Everything inside is a type (a generic argument list, the brackets of a written type).
    ty: bool,
    seg: Seg,
    /// `?` of a conditional and `lambda`s that wait for their `:`.
    pending: u32,
    /// The annotation that is open follows a `)`: the return type of a function, which the first `=>` ends
    /// even when the type itself ends in a `)` (`(): (A | B) => "v"`).
    returns: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Comment {
    No,
    Line,
    Block,
}

/// The deepest bracket nesting that is followed; beyond it every literal is replaced.
const MAX_DEPTH: usize = 32;

/// The most characters between the quotes of a type literal that is kept.
const MAX_LITERAL: usize = 64;

/// Words after which a `/` opens a regular expression and does not divide.
const REGEX_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

/// How far past a `<` the closing `>` of generic arguments is looked for.
const LOOKAHEAD: usize = 256;

/// `sig` with the string literals in value positions replaced by `"…"` and those in type positions kept.
pub(super) fn elide(sig: &str, lang: Lang, rules: &Quotes) -> String {
    let chars: Vec<char> = sig.chars().collect();
    let head = head_of(&chars, lang);
    let root = Frame {
        open: '\0',
        ty: head == Head::Interface,
        seg: Seg::Value,
        pending: 0,
        returns: false,
    };

    Scan {
        complex: chars.contains(&'(') || chars.windows(2).any(|pair| pair == ['=', '>']),
        chars: &chars,
        lang,
        rules,
        head,
        frames: vec![root],
        head_open: true,
        prev: '\0',
        comment: Comment::No,
        poisoned: false,
        out: String::with_capacity(sig.len()),
        at: 0,
    }
    .run()
}

struct Scan<'a> {
    chars: &'a [char],
    lang: Lang,
    rules: &'a Quotes,
    head: Head,
    /// The line holds a `(` or an `=>`: a `key:` at its start may be a member that holds a function.
    complex: bool,
    frames: Vec<Frame>,
    /// No `=` or `(` of the line itself was seen yet (the generic parameters of a type, class or function).
    head_open: bool,
    /// The last character that was not a space.
    prev: char,
    comment: Comment,
    /// The nesting went too deep.
    poisoned: bool,
    out: String,
    at: usize,
}

impl Scan<'_> {
    fn run(mut self) -> String {
        while let Some(&c) = self.chars.get(self.at) {
            // a comment changes nothing about where the scan is: only its strings are blanked
            if self.comment != Comment::No {
                self.in_comment(c);

                continue;
            }

            match c {
                '"' | '\'' => self.string(),
                '`' if self.rules.backtick => self.string(),
                _ if is_word(c) => self.word(),
                '(' | '[' | '{' | '<' => self.open(c),
                ')' | ']' | '}' => self.close(c),
                '>' => self.greater(),
                ',' | ';' => self.separator(c),
                ':' => self.colon(),
                '=' => self.equals(),
                '?' if self.lang == Lang::Ts => self.question(),
                '-' if self.lang == Lang::Python && self.next(1) == Some('>') => self.return_arrow(),
                _ => self.plain(c),
            }
        }

        self.out
    }

    fn next(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn top(&self) -> &Frame {
        &self.frames[self.frames.len() - 1]
    }

    fn top_mut(&mut self) -> &mut Frame {
        let last = self.frames.len() - 1;

        &mut self.frames[last]
    }

    /// True when the cursor is in a type segment or a type bracket.
    fn in_type(&self) -> bool {
        let top = self.top();

        top.ty || top.seg != Seg::Value
    }

    fn emit(&mut self, c: char) {
        self.out.push(c);
        self.at += 1;

        if !c.is_whitespace() {
            self.prev = c;
        }
    }

    fn plain(&mut self, c: char) {
        let next = self.next(1);

        match self.lang {
            Lang::Ts if c == '/' && next == Some('/') => return self.start_comment(Comment::Line, 2),
            Lang::Ts if c == '/' && next == Some('*') => return self.start_comment(Comment::Block, 2),
            Lang::Python if c == '#' => return self.start_comment(Comment::Line, 1),
            Lang::Ts if c == '/' && self.may_start_regex() => return self.regex(),
            _ => {}
        }

        if self.assertion_ends(c) {
            self.top_mut().seg = Seg::Value;
        }

        self.emit(c);
    }

    // --- comments and regular expressions ---

    /// Copies the `marker` characters that open a comment, without touching the state of the scan.
    fn start_comment(&mut self, kind: Comment, marker: usize) {
        self.out.extend(&self.chars[self.at..self.at + marker]);
        self.at += marker;
        self.comment = kind;
    }

    /// One character inside a comment: copied as it is, unless it starts a string (blanked: a comment may
    /// quote what it says) or ends the comment.
    fn in_comment(&mut self, c: char) {
        let quoted = matches!(c, '"' | '\'') || (c == '`' && self.rules.backtick);

        if quoted {
            self.at = string_end(self.chars, self.at, self.rules);
            self.out.push_str(ELIDED);

            return;
        }

        match self.comment {
            Comment::Block if c == '*' && self.next(1) == Some('/') => {
                self.out.push_str("*/");
                self.at += 2;
                self.comment = Comment::No;
            }
            Comment::Line if c == '\n' => {
                self.out.push(c);
                self.at += 1;
                self.comment = Comment::No;
            }
            _ => {
                self.out.push(c);
                self.at += 1;
            }
        }
    }

    /// True when the `/` at the cursor may open a regular expression (TypeScript): after anything but a word
    /// (a name or a number, not a keyword like `return`), a `]` or a closing quote, which a division follows.
    fn may_start_regex(&self) -> bool {
        match self.prev {
            '\0' => true,
            ']' | '"' => false,
            prev if is_word(prev) => {
                let mut end = self.at;

                while end > 0 && self.chars[end - 1].is_whitespace() {
                    end -= 1;
                }

                let mut start = end;

                while start > 0 && is_word(self.chars[start - 1]) {
                    start -= 1;
                }

                let word: String = self.chars[start..end].iter().collect();

                REGEX_KEYWORDS.contains(&word.as_str())
            }
            _ => true,
        }
    }

    /// A `/` that may open a regular expression, whose body may hold quotes and brackets that this scan would
    /// take for strings and types: when a quote follows anywhere, the rest of the line is blanked.
    fn regex(&mut self) {
        let quote_follows = self.chars[self.at + 1..]
            .iter()
            .any(|c| matches!(c, '"' | '\'') || (*c == '`' && self.rules.backtick));

        if !quote_follows {
            self.emit('/');

            return;
        }

        self.out.push('/');
        self.out.push('…');
        self.at = self.chars.len();
    }

    /// An operator that cannot be part of a type ends what `as` started.
    fn assertion_ends(&self, c: char) -> bool {
        let top = self.top();

        top.seg == Seg::Assertion
            && !top.ty
            && match c {
                '+' | '*' | '%' => true,
                '|' | '&' => self.next(1) == Some(c),
                _ => false,
            }
    }

    // --- strings ---

    fn string(&mut self) {
        let open = self.at;
        let end = string_end(self.chars, open, self.rules);

        if self.keeps(open, end) {
            self.out.extend(&self.chars[open..end]);
        } else {
            self.out.push_str(ELIDED);
        }

        self.prev = '"';
        self.at = end;
    }

    /// True when the literal at `open..end` is a plain, short, closed one in a type position.
    fn keeps(&self, open: usize, end: usize) -> bool {
        let quote = self.chars[open];
        let length = end - open;
        let prefixed = open > 0 && is_word(self.chars[open - 1]);
        let triple = self.chars.get(open + 1) == Some(&quote) && self.chars.get(open + 2) == Some(&quote);

        self.in_type()
            && !self.poisoned
            && self.comment == Comment::No
            && quote != '`'
            && !prefixed
            && !(triple && length >= 6)
            && (2..=MAX_LITERAL + 2).contains(&length)
            && self.chars[end - 1] == quote
            && self.chars[open + 1..end - 1]
                .iter()
                .rev()
                .take_while(|c| **c == '\\')
                .count()
                % 2
                == 0
    }

    // --- words ---

    fn word(&mut self) {
        let start = self.at;

        while self.chars.get(self.at).is_some_and(|c| is_word(*c)) {
            self.at += 1;
        }

        let word: String = self.chars[start..self.at].iter().collect();
        let after_dot = start > 0 && self.chars[start - 1] == '.';
        let top = self.top();
        let (typed, seg) = (top.ty, top.seg);

        match (self.lang, word.as_str()) {
            (Lang::Ts, "as" | "satisfies") if !typed && seg == Seg::Value && !after_dot => {
                self.top_mut().seg = Seg::Assertion;
            }
            (Lang::Ts, "from") if !typed && matches!(seg, Seg::Annotation | Seg::Assertion) => {
                self.top_mut().seg = Seg::Value;
            }
            (Lang::Python, "lambda") if !typed && seg == Seg::Value => self.top_mut().pending += 1,
            _ => {}
        }

        self.out.push_str(&word);
        self.prev = self.chars[self.at - 1];
    }

    // --- brackets ---

    fn open(&mut self, c: char) {
        let ty = match (self.lang, c) {
            (_, '<') if self.lang == Lang::Python => return self.plain(c),
            (_, '<') => {
                if !self.generic_opens() {
                    return self.plain(c);
                }

                true
            }
            (Lang::Python, '[') => {
                let before = self.word_before();

                before == "Literal" || (self.in_type() && before != "Annotated")
            }
            (Lang::Python, _) => false,
            (Lang::Ts, '{') if self.body_follows() => {
                self.top_mut().seg = Seg::Value;

                false
            }
            (Lang::Ts, _) => self.in_type(),
        };

        if c == '(' && self.frames.len() == 1 {
            self.head_open = false;
        }

        if self.frames.len() >= MAX_DEPTH {
            self.poisoned = true;
        } else {
            self.frames.push(Frame {
                open: c,
                ty,
                seg: Seg::Value,
                pending: 0,
                returns: false,
            });
        }

        self.emit(c);
    }

    /// True when the `{` at the cursor follows a complete type (`): "x" {`), so it opens a body and not an object
    /// type (`m: {`, `A | {`, `=> {` of a function type).
    fn body_follows(&self) -> bool {
        let top = self.top();

        if top.ty || !matches!(top.seg, Seg::Annotation | Seg::Assertion) {
            return false;
        }

        let Some(last) = self.chars[..self.at].iter().rposition(|c| !c.is_whitespace()) else {
            return false;
        };

        match self.chars[last] {
            ':' | '|' | '&' | '<' | ',' | '(' | '?' => false,
            '>' => !(last > 0 && self.chars[last - 1] == '='),
            _ => true,
        }
    }

    fn close(&mut self, c: char) {
        let want = match c {
            ')' => '(',
            ']' => '[',
            _ => '{',
        };

        if let Some(level) = self.frames.iter().rposition(|frame| frame.open == want) {
            self.frames.truncate(level);
        }

        self.emit(c);
    }

    fn greater(&mut self) {
        if self.top().open == '<' && self.frames.len() > 1 {
            self.frames.pop();
        }

        self.emit('>');
    }

    /// The identifier that ends right before the cursor.
    fn word_before(&self) -> String {
        let mut start = self.at;

        while start > 0 && is_word(self.chars[start - 1]) {
            start -= 1;
        }

        self.chars[start..self.at].iter().collect()
    }

    /// True when the `<` at the cursor opens a list of generic arguments (TypeScript).
    fn generic_opens(&self) -> bool {
        if self.in_type() {
            return true;
        }

        let adjacent = self.at > 0 && is_word(self.chars[self.at - 1]);
        let leading = matches!(self.prev, '=' | '(' | ',' | '\0');

        if !adjacent && !leading {
            return false;
        }

        let Some((close, assigns)) = self.matching_angle() else {
            return false;
        };

        let next = self.chars[close + 1..].iter().find(|c| !c.is_whitespace());
        let parameters =
            self.frames.len() == 1 && self.head_open && matches!(self.head, Head::Alias | Head::Class | Head::Function);

        // `foo<"T">("v")` calls with type arguments. An `=` inside (`<a b="v">(`) is a JSX attribute.
        parameters || (next == Some(&'(') && !assigns)
    }

    /// The `>` that closes the `<` at the cursor within the look-ahead, and whether an `=` stands between.
    fn matching_angle(&self) -> Option<(usize, bool)> {
        let limit = (self.at + LOOKAHEAD).min(self.chars.len());
        let mut at = self.at + 1;
        let mut depth = 1_usize;
        let mut assigns = false;

        while at < limit {
            match self.chars[at] {
                '"' | '\'' | '`' => {
                    at = string_end(self.chars, at, self.rules);

                    continue;
                }
                '<' => depth += 1,
                '=' if self.chars.get(at + 1) == Some(&'>') => at += 1,
                '=' => assigns = true,
                '>' => {
                    depth -= 1;

                    if depth == 0 {
                        return Some((at, assigns));
                    }
                }
                _ => {}
            }

            at += 1;
        }

        None
    }

    // --- segments ---

    fn separator(&mut self, c: char) {
        let lang = self.lang;
        let top = self.top_mut();

        if !top.ty {
            if c == ';' || top.seg != Seg::Alias {
                top.seg = Seg::Value;
            }

            // the `,` between the parameters of a Python `lambda` does not end it: its `:` is still to come
            if c == ';' || lang == Lang::Ts {
                top.pending = 0;
            }
        }

        self.plain(c);
    }

    fn colon(&mut self) {
        // `:=` of Python
        if self.lang == Lang::Python && self.next(1) == Some('=') {
            return self.plain(':');
        }

        let head = self.head;
        let complex = self.complex;
        let prev = self.prev;
        let lang = self.lang;
        let top = self.top_mut();

        if top.ty {
            // nothing changes inside a type
        } else if top.pending > 0 {
            // the `:` of a conditional or of a lambda
            top.pending -= 1;

            if top.seg != Seg::Alias {
                top.seg = Seg::Value;
            }
        } else if top.open == '(' {
            top.seg = Seg::Annotation;
        } else if top.open == '\0' {
            let annotates = match (lang, head) {
                (Lang::Python, Head::Block) => {
                    top.seg = Seg::Value;

                    false
                }
                (Lang::Python, Head::Decl) => true,
                (Lang::Ts, Head::Class | Head::Function | Head::Decl) => true,
                // `key: ...` may be an object member that holds a function: a value
                (Lang::Ts, Head::Bare) => prev == ')' || !complex,
                _ => false,
            };

            if annotates {
                top.seg = Seg::Annotation;
                top.returns = prev == ')';
            }
        }

        self.plain(':');
    }

    fn return_arrow(&mut self) {
        let top = self.top_mut();

        if top.open == '\0' && !top.ty && top.seg == Seg::Value {
            top.seg = Seg::Annotation;
        }

        self.out.push('-');
        self.at += 1;
        self.emit('>');
    }

    fn equals(&mut self) {
        let mut run = 0;

        while self.next(run) == Some('=') {
            run += 1;
        }

        let arrow = run == 1 && self.next(1) == Some('>');
        let assign = run == 1 && !arrow;
        let prev = self.prev;
        let alias = self.head == Head::Alias;
        let prev_char = if self.at > 0 { self.chars[self.at - 1] } else { '\0' };
        let top = self.top_mut();

        if !top.ty {
            let typed = matches!(top.seg, Seg::Annotation | Seg::Assertion);

            if arrow {
                // `(a): "x" => ...` ends the annotation, `(a) => "x"` of a function type does not, but the
                // arrow after the return type of a function does, whatever the type ends in
                if typed && (prev != ')' || (top.seg == Seg::Annotation && top.returns)) {
                    top.seg = Seg::Value;
                }
            } else if assign && top.open == '\0' && top.seg == Seg::Value && alias && !"<>!".contains(prev_char) {
                top.seg = Seg::Alias;
            } else if typed {
                top.seg = Seg::Value;
            }
        }

        if assign && self.frames.len() == 1 {
            self.head_open = false;
        }

        for _ in 0..run {
            self.emit('=');
        }

        if arrow {
            self.emit('>');
        }
    }

    fn question(&mut self) {
        let next = self.next(1);
        let digit_after = self.next(2).is_some_and(|c| c.is_ascii_digit());
        let top = self.top_mut();

        if top.ty || matches!(top.seg, Seg::Annotation | Seg::Alias) {
            return self.plain('?');
        }

        if top.seg == Seg::Assertion {
            top.seg = Seg::Value;
        }

        match next {
            Some('?') => {
                self.emit('?');
                self.emit('?');
            }
            Some('.') if !digit_after => self.emit('?'),
            // the mark of an optional parameter or member
            Some(':') => self.emit('?'),
            _ => {
                top.pending += 1;
                self.emit('?');
            }
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// What the line starts like, from its first words.
fn head_of(chars: &[char], lang: Lang) -> Head {
    let mut at = 0;
    let mut modified = false;

    loop {
        while chars.get(at).is_some_and(|c| c.is_whitespace()) {
            at += 1;
        }

        let start = at;

        while chars.get(at).is_some_and(|c| is_word(*c)) {
            at += 1;
        }

        let word: String = chars[start..at].iter().collect();
        let named = chars[at..]
            .iter()
            .find(|c| !c.is_whitespace())
            .is_some_and(|c| c.is_alphabetic() || *c == '_' || *c == '$');

        match (lang, word.as_str()) {
            (_, "type") if named => return Head::Alias,
            (Lang::Ts, "interface") => return Head::Interface,
            (Lang::Ts, "class") => return Head::Class,
            (Lang::Ts, "function") => return Head::Function,
            (Lang::Ts, "const" | "let" | "var" | "enum" | "namespace" | "module") => return Head::Decl,
            (Lang::Ts, "export" | "default" | "async") => {}
            (Lang::Ts, "declare" | "abstract" | "public" | "private" | "protected" | "static" | "readonly") => {
                modified = true;
            }
            (Lang::Ts, "override" | "accessor") => modified = true,
            (Lang::Python, "async") => {}
            (Lang::Python, "") => return Head::Block,
            (Lang::Python, w) if PYTHON_KEYWORDS.contains(&w) => return Head::Block,
            (Lang::Python, _) => return Head::Decl,
            _ => return if modified { Head::Decl } else { Head::Bare },
        }
    }
}

/// Python words that start a statement which is not an assignment.
const PYTHON_KEYWORDS: &[&str] = &[
    "def", "class", "if", "elif", "else", "for", "while", "with", "try", "except", "finally", "lambda", "match",
    "case", "return", "yield", "raise", "assert", "del", "import", "from", "global", "nonlocal", "pass", "await",
];
