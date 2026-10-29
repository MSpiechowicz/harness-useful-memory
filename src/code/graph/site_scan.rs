//! What a line of source is made of, for the line an answer shows ([`super::site`]): code, strings and comments.
//!
//! The line is read out of its file, so it may begin inside something that started above it. The scan carries
//! its state over a few lines before the line (a string that goes on, a block comment), writes the line again
//! with each comment replaced (`// …`, `/* … */`, `# …`: a comment holds notes, and a note holds what it holds)
//! and says where the strings are, so the caller can leave out a line it cannot read with certainty: one that
//! ends inside a string, begins inside one, or whose first quote closes a string more than it opens one.

/// The most lines above the line that the scan reads for the state it starts in.
pub(super) const CONTEXT_LINES: usize = 20;

/// What replaces the content of a comment.
const BLANK: &str = "…";

/// How a language writes comments and strings.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Rules {
    /// `#` starts a comment (Python), not an attribute (Rust).
    hash_comment: bool,
    /// `//` starts a comment and `/* */` holds one.
    slash_comment: bool,
    /// `'` starts a string (not in Rust and Go, where it is a lifetime or a character).
    single: bool,
    /// `` ` `` starts a string.
    backtick: bool,
    /// A backtick string has no escapes (Go).
    backtick_raw: bool,
    /// A plain string goes on to the next line (Rust).
    multiline: bool,
    /// Triple quotes (Python).
    triple: bool,
    /// Raw strings `r#"…"#` (Rust).
    raw: bool,
    /// A character literal `'x'` (Rust and Go).
    chars: bool,
}

fn rules(lang: &str) -> Rules {
    Rules {
        hash_comment: lang == "python",
        slash_comment: lang != "python",
        single: !matches!(lang, "rust" | "go"),
        backtick: !matches!(lang, "rust" | "python"),
        backtick_raw: lang == "go",
        multiline: lang == "rust",
        triple: lang == "python",
        raw: lang == "rust",
        chars: matches!(lang, "rust" | "go"),
    }
}

/// Where the scan is at the end of a line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum State {
    Code,
    /// In a block comment.
    Block,
    /// In a string closed by `quote` (three of them when `triple`).
    Quote {
        quote: char,
        triple: bool,
    },
    /// In a raw string closed by `"` and `hashes` `#`.
    Raw {
        hashes: usize,
    },
}

/// A line, written again.
pub(super) struct Scanned {
    /// The line with each comment replaced by its mark.
    pub text: String,
    /// The byte ranges of `text` that are strings, quotes included.
    pub strings: Vec<(usize, usize)>,
    /// Where the line ends.
    pub end: State,
    /// A plain string was left open at the end of the line that does not go on (a quote in a regular
    /// expression, or a line cut short).
    pub broken: bool,
}

/// Scans the `lines` in order and gives the state at the end of the last one. The scan starts in code, unless
/// a line shows the end of a string (see [`first_quote_closes`]): the lines are a window cut out of a file, which
/// may begin inside a string.
pub(super) fn state_after(lang: &str, lines: &[String]) -> State {
    let rules = rules(lang);
    let mut state = State::Code;

    for (number, line) in lines.iter().enumerate() {
        // the window may begin inside a string: a line that shows the end of one, with a quote that is not
        // the start of any, closes it
        if state == State::Code {
            let cold = scan(&rules, line, State::Code);
            let closes = if number == 0 {
                first_quote_closes(&cold)
            } else {
                first_quote_glued(&cold)
            };

            if closes
                && let Some(quote) = cold
                    .strings
                    .first()
                    .and_then(|(at, _)| cold.text.get(*at..)?.chars().next())
            {
                state = State::Quote { quote, triple: false };
            }
        }

        state = scan(&rules, line, state).end;
    }

    state
}

/// Scans `line`, which begins in `state`, in the language `lang`.
pub(super) fn scan_line(lang: &str, line: &str, state: State) -> Scanned {
    scan(&rules(lang), line, state)
}

fn scan(rules: &Rules, line: &str, start: State) -> Scanned {
    let chars: Vec<char> = line.chars().collect();
    let mut walker = Scan {
        rules,
        chars: &chars,
        out: String::with_capacity(line.len()),
        strings: Vec::new(),
        opened: 0,
        broken: false,
    };
    let mut state = start;
    let mut at = 0;

    while at < chars.len() {
        (state, at) = match state {
            State::Code => walker.code(at),
            State::Block => walker.block(at),
            State::Quote { quote, triple } => walker.quoted(at, quote, triple),
            State::Raw { hashes } => walker.raw(at, hashes),
        };
    }

    // a string that began on this line and is left open where nothing continues it is a broken line
    if let State::Quote { quote, triple } = state
        && !triple
        && !walker.rules.multiline
        && !(quote == '`' && walker.rules.backtick)
        && !continued(&chars)
    {
        walker.broken = true;
        state = State::Code;
    }

    walker.close_string();

    Scanned {
        text: walker.out,
        strings: walker.strings,
        end: state,
        broken: walker.broken,
    }
}

/// True when the line ends with a backslash that is not itself escaped: the string goes on.
fn continued(chars: &[char]) -> bool {
    chars.iter().rev().take_while(|c| **c == '\\').count() % 2 == 1
}

struct Scan<'a> {
    rules: &'a Rules,
    chars: &'a [char],
    out: String,
    strings: Vec<(usize, usize)>,
    /// The byte of `out` where the string being written began, plus one (zero when none).
    opened: usize,
    broken: bool,
}

impl Scan<'_> {
    fn peek(&self, at: usize) -> Option<char> {
        self.chars.get(at).copied()
    }

    fn open_string(&mut self) {
        self.opened = self.out.len() + 1;
    }

    fn close_string(&mut self) {
        if self.opened > 0 {
            self.strings.push((self.opened - 1, self.out.len()));
            self.opened = 0;
        }
    }

    /// One step in code at `at`: the next state and index.
    fn code(&mut self, at: usize) -> (State, usize) {
        let c = self.chars[at];
        let next = self.peek(at + 1);

        // the end of a comment that began above this line: what is before it is the comment
        if self.rules.slash_comment && c == '*' && next == Some('/') {
            self.out.clear();
            self.strings.clear();
            self.opened = 0;
            self.out.push_str("/* ");
            self.out.push_str(BLANK);
            self.out.push_str(" */");

            return (State::Code, at + 2);
        }

        if self.rules.slash_comment && c == '/' && next == Some('/') {
            self.out.push_str("// ");
            self.out.push_str(BLANK);

            return (State::Code, self.chars.len());
        }

        if self.rules.hash_comment && c == '#' {
            self.out.push_str("# ");
            self.out.push_str(BLANK);

            return (State::Code, self.chars.len());
        }

        if self.rules.slash_comment && c == '/' && next == Some('*') {
            return self.block_start(at);
        }

        if self.rules.chars && c == '\'' {
            let end = char_literal_end(self.chars, at).unwrap_or(at + 1);

            self.out.extend(&self.chars[at..end]);

            return (State::Code, end);
        }

        if self.rules.raw
            && c == 'r'
            && let Some((hashes, body)) = raw_start(self.chars, at)
        {
            self.open_string();
            self.out.extend(&self.chars[at..body]);

            return (State::Raw { hashes }, body);
        }

        if c == '"' || (c == '\'' && self.rules.single) || (c == '`' && self.rules.backtick) {
            return self.string_start(at, c);
        }

        self.out.push(c);

        (State::Code, at + 1)
    }

    /// A block comment opens at `at`: it ends on this line (and is replaced whole) or goes on.
    fn block_start(&mut self, at: usize) -> (State, usize) {
        self.out.push_str("/* ");
        self.out.push_str(BLANK);

        match (at + 2..self.chars.len().saturating_sub(1)).find(|i| self.chars[*i] == '*' && self.chars[*i + 1] == '/')
        {
            Some(end) => {
                self.out.push_str(" */");

                (State::Code, end + 2)
            }
            None => (State::Block, self.chars.len()),
        }
    }

    /// In a block comment that began above this line.
    fn block(&mut self, at: usize) -> (State, usize) {
        self.out.push_str("/* ");
        self.out.push_str(BLANK);

        match (at..self.chars.len().saturating_sub(1)).find(|i| self.chars[*i] == '*' && self.chars[*i + 1] == '/') {
            Some(end) => {
                self.out.push_str(" */");

                (State::Code, end + 2)
            }
            None => (State::Block, self.chars.len()),
        }
    }

    fn string_start(&mut self, at: usize, quote: char) -> (State, usize) {
        let triple =
            self.rules.triple && quote != '`' && self.peek(at + 1) == Some(quote) && self.peek(at + 2) == Some(quote);
        let width = if triple { 3 } else { 1 };

        self.open_string();
        self.out.extend(&self.chars[at..at + width]);

        (State::Quote { quote, triple }, at + width)
    }

    /// In a string closed by `quote`.
    fn quoted(&mut self, mut at: usize, quote: char, triple: bool) -> (State, usize) {
        let escapes = !(quote == '`' && self.rules.backtick_raw);

        while let Some(c) = self.peek(at) {
            if escapes && c == '\\' {
                let end = (at + 2).min(self.chars.len());

                self.out.extend(&self.chars[at..end]);
                at = end;

                continue;
            }

            let closes =
                c == quote && (!triple || (self.peek(at + 1) == Some(quote) && self.peek(at + 2) == Some(quote)));

            if closes {
                let width = if triple { 3 } else { 1 };

                self.out.extend(&self.chars[at..at + width]);
                self.close_string();

                return (State::Code, at + width);
            }

            self.out.push(c);
            at += 1;
        }

        (State::Quote { quote, triple }, at)
    }

    /// In a raw string closed by `"` and `hashes` `#`.
    fn raw(&mut self, mut at: usize, hashes: usize) -> (State, usize) {
        while let Some(c) = self.peek(at) {
            if c == '"' && (1..=hashes).all(|n| self.peek(at + n) == Some('#')) {
                self.out.extend(&self.chars[at..at + 1 + hashes]);
                self.close_string();

                return (State::Code, at + 1 + hashes);
            }

            self.out.push(c);
            at += 1;
        }

        (State::Raw { hashes }, at)
    }
}

/// The index after a character literal (`'x'`, `'\n'`, `'\u{1F600}'`, `'"'`) that starts at `start`; `None` for
/// a lifetime or a loop label (`'a`).
fn char_literal_end(chars: &[char], start: usize) -> Option<usize> {
    match chars.get(start + 1)? {
        '\\' => (start + 3..=start + 11)
            .find(|at| chars.get(*at) == Some(&'\''))
            .map(|at| at + 1),
        '\'' => None,
        _ if chars.get(start + 2) == Some(&'\'') => Some(start + 3),
        _ => None,
    }
}

/// For a raw string whose `r` is at `start`: the number of `#` and the index after its opening quote.
fn raw_start(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let before = |back: usize| start.checked_sub(back).and_then(|at| chars.get(at)).copied();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let prefixed = matches!(before(1), Some('b' | 'c')) && !before(2).is_some_and(word);

    if before(1).is_some_and(word) && !prefixed {
        return None;
    }

    let mut at = start + 1;

    while chars.get(at) == Some(&'#') {
        at += 1;
    }

    (chars.get(at) == Some(&'"')).then_some((at - start - 1, at + 1))
}

/// True when the first string of `scanned` looks like the end of a string that began above the line more
/// than the start of one: a quote glued to the word before it (`tail text", call("x")`), or a quote at the
/// start of the line followed by what ends an argument (`", call(k)`, `");`).
pub(super) fn first_quote_closes(scanned: &Scanned) -> bool {
    let Some(&(start, _)) = scanned.strings.first() else {
        return false;
    };
    let text = &scanned.text;
    let before = text.get(..start).unwrap_or("");
    let quote_len = text
        .get(start..)
        .and_then(|s| s.chars().next())
        .map_or(1, char::len_utf8);

    if before.trim().is_empty() {
        let after = text.get(start + quote_len..).and_then(|s| s.chars().next());

        return after.is_none_or(|c| matches!(c, ',' | ';' | ')' | ']' | '}' | '+' | '.' | ':' | '?'));
    }

    if !before.chars().next_back().is_some_and(is_word) {
        return before.ends_with([')', ']']);
    }

    glued_to_word(before)
}

/// True when `scanned` holds a string whose first quote is glued to the word before it
/// (`tail text", call("x")`): the end of a string that began above. A window cut out of a file uses this for
/// any of its lines.
fn first_quote_glued(scanned: &Scanned) -> bool {
    let Some(&(start, _)) = scanned.strings.first() else {
        return false;
    };
    let before = scanned.text.get(..start).unwrap_or("");

    before.chars().next_back().is_some_and(is_word) && glued_to_word(before)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `before`, which ends in a word character, ends in a word and not in the prefix of a string
/// (`r"…"`, `b"…"`, `f'…'`, `br#"…"#`).
fn glued_to_word(before: &str) -> bool {
    let letters: String = before.chars().rev().take_while(|c| c.is_alphabetic()).collect();
    let prefix = letters.len() <= 2
        && letters
            .chars()
            .all(|c| matches!(c.to_ascii_lowercase(), 'r' | 'b' | 'f' | 'u' | 'c'))
        && !before.chars().rev().nth(letters.chars().count()).is_some_and(is_word);

    !prefix
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn text(lang: &str, line: &str) -> String {
        scan_line(lang, line, State::Code).text
    }

    #[test]
    fn comments_are_replaced_and_the_code_around_them_stays() {
        assert_eq!(
            text("rust", "target_fn(\"a\") // prod password is HUNTER"),
            "target_fn(\"a\") // …"
        );
        assert_eq!(
            text("rust", "/* api key HUNTER */ target_fn(\"a\")"),
            "/* … */ target_fn(\"a\")"
        );
        assert_eq!(
            text("rust", "f(/* HUNTER */ 1, /* two */ 2)"),
            "f(/* … */ 1, /* … */ 2)"
        );
        assert_eq!(
            text("ts", "return tsTarget(\"a\") // DB_PASSWORD=HUNTER"),
            "return tsTarget(\"a\") // …"
        );
        assert_eq!(text("go", "target() // HUNTER"), "target() // …");
        assert_eq!(text("python", "target(\"a\")  # HUNTER"), "target(\"a\")  # …");
        assert_eq!(text("ts", "f(1) /* opens HUNTER"), "f(1) /* …");
    }

    #[test]
    fn markers_inside_strings_and_attributes_are_not_comments() {
        assert_eq!(text("rust", "f(\"http://x // y\")"), "f(\"http://x // y\")");
        assert_eq!(text("rust", "#[derive(Debug)] f()"), "#[derive(Debug)] f()");
        assert_eq!(text("python", "f(\"a # b\", '# c')"), "f(\"a # b\", '# c')");
        assert_eq!(text("ts", "f('/* not */')"), "f('/* not */')");
        assert_eq!(text("rust", "f('\"', '/'); // HUNTER"), "f('\"', '/'); // …");
        assert_eq!(text("rust", "f(r#\"a // b\"#) // HUNTER"), "f(r#\"a // b\"#) // …");
    }

    #[test]
    fn a_comment_that_ended_on_this_line_is_replaced_from_its_start() {
        let line = scan_line("rust", "HUNTER words */ target_fn()", State::Code);

        assert_eq!(line.text, "/* … */ target_fn()");
        assert_eq!(scan_line("rust", "HUNTER words", State::Block).text, "/* …");
        assert_eq!(
            scan_line("rust", "HUNTER */ target_fn()", State::Block).text,
            "/* … */ target_fn()"
        );
    }

    #[test]
    fn the_state_goes_across_lines() {
        let lines = |text: &[&str]| text.iter().map(|l| (*l).to_string()).collect::<Vec<_>>();

        assert_eq!(state_after("rust", &lines(&["let s = \"a", "b\";"])), State::Code);
        assert_eq!(
            state_after("rust", &lines(&["let s = \"a", "b"])),
            State::Quote {
                quote: '"',
                triple: false
            }
        );
        assert_eq!(state_after("rust", &lines(&["let s = r#\"a", "b\"#;"])), State::Code);
        assert_eq!(
            state_after("rust", &lines(&["let s = r#\"a"])),
            State::Raw { hashes: 1 }
        );
        // a window that begins with the end of a string: that string is not opened again
        assert_eq!(state_after("rust", &lines(&["ORDER BY r.id\",", ")?;"])), State::Code);
        assert_eq!(
            state_after("rust", &lines(&["AND n.id != $6", "LIMIT $5\",", ")?;"])),
            State::Code
        );
        assert_eq!(
            state_after("rust", &lines(&["ORDER BY r.id\",", "let s = \"a"])),
            State::Quote {
                quote: '"',
                triple: false
            }
        );
        assert_eq!(state_after("rust", &lines(&["/* a \" b", "c */"])), State::Code);
        assert_eq!(state_after("rust", &lines(&["/* a \" b"])), State::Block);
        assert_eq!(state_after("ts", &lines(&["const s = \"a \\", "b\";"])), State::Code);
        assert_eq!(
            state_after("ts", &lines(&["const s = \"a \\"])),
            State::Quote {
                quote: '"',
                triple: false
            }
        );
        // a quote that nothing continues ends with its line
        assert_eq!(state_after("ts", &lines(&["const s = /'/;"])), State::Code);
        assert_eq!(
            state_after("python", &lines(&["x = \"\"\"a", "b"])),
            State::Quote {
                quote: '"',
                triple: true
            }
        );
        assert_eq!(state_after("python", &lines(&["x = \"\"\"a", "b\"\"\""])), State::Code);
        assert_eq!(
            state_after("go", &lines(&["x := `a", "b"])),
            State::Quote {
                quote: '`',
                triple: false
            }
        );
    }

    #[test]
    fn a_first_quote_that_closes_is_told_from_one_that_opens() {
        let closes = |lang: &str, line: &str| first_quote_closes(&scan_line(lang, line, State::Code));

        assert!(closes("rust", "tail of the text\", target_fn(\"x\")"));
        assert!(closes("rust", "\", target_fn(k)"));
        assert!(closes("rust", "\");"));
        assert!(closes("rust", "tail\"#; target_fn(k)"));
        assert!(!closes("rust", "let k = \"x\"; target_fn(k)"));
        assert!(!closes("rust", "target_fn(\"x\")"));
        assert!(!closes("rust", "let b = b\"x\"; let r = r#\"y\"#;"));
        assert!(!closes("python", "x = f\"{a}\" + rb'x'"));
        assert!(!closes("ts", "return call(\"x\")"));
        assert!(!closes("ts", "\"key\": call(1),"));
    }
}
