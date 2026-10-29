//! String literals in a signature are not kept, except in a type position.
//!
//! A signature is the first line of a definition, and `const API_KEY = "..."`, `static TOKEN: &str = "..."`,
//! a one-line function that builds a request with a key, a default argument or a struct tag would put a
//! secret that sits in a file into every answer of the code tools. The index exists to say where code is and
//! who uses it, so the content of every string literal in every signature is replaced by `"…"` before it is
//! stored ([`elide_strings`]); the rest of the line is kept. The scan knows the strings of the languages it
//! reads: raw, byte, template (with the code in `${...}`), triple-quoted and formatted ones, and leaves a
//! character literal (`'"'`) and a lifetime (`'a`) of Rust and Go alone.
//!
//! One exception: in TypeScript, JavaScript and Python a short plain literal in a type position (`type Mode =
//! "fast" | "slow"`, `m: "x"`, `Foo<"x">`, `Literal["a"]`, `x: "Foo"`) says what a definition accepts and is
//! kept; values (`= "..."`, defaults, call arguments, object members, enum values, templates, f-strings) are
//! not. The scan for those two languages is in [`typed`].

mod typed;

use std::borrow::Cow;

/// What replaces a string literal.
const ELIDED: &str = "\"…\"";

/// Which quotes start a string in a language, by its `files.lang` name.
struct Quotes {
    /// `'` starts a string (not in Rust and Go, where it starts a lifetime or a character).
    single: bool,
    /// `` ` `` starts a string (JavaScript templates, Go raw strings).
    backtick: bool,
    /// A backtick string has no escapes (Go).
    backtick_raw: bool,
    /// A backtick string holds code in `${...}` (JavaScript).
    template: bool,
    /// Rust raw strings, `r#"..."#`.
    rust_raw: bool,
    /// Python triple quotes and formatted strings.
    python: bool,
}

fn quotes(lang: &str) -> Quotes {
    Quotes {
        single: !matches!(lang, "rust" | "go"),
        backtick: !matches!(lang, "rust" | "python"),
        backtick_raw: lang == "go",
        template: !matches!(lang, "rust" | "python" | "go"),
        rust_raw: lang == "rust",
        python: lang == "python",
    }
}

/// How a string treats `{`: as the start of code, or not at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Holes {
    None,
    /// `${code}` (a JavaScript template).
    Template,
    /// `{code}` (a Python f-string; `{{` is a brace).
    Format,
}

/// `sig` with every string literal replaced by `"…"`. A literal that is not closed on the line (the line was
/// cut, or the string goes on) is replaced up to the end. `lang` is the `files.lang` of the file.
pub fn elide_strings<'a>(sig: &'a str, lang: &str) -> Cow<'a, str> {
    if !sig.contains(['"', '\'', '`']) {
        return Cow::Borrowed(sig);
    }

    let rules = quotes(lang);

    match lang {
        "ts" => return Cow::Owned(typed::elide(sig, typed::Lang::Ts, &rules)),
        "python" => return Cow::Owned(typed::elide(sig, typed::Lang::Python, &rules)),
        _ => {}
    }

    let chars: Vec<char> = sig.chars().collect();
    let mut out = String::with_capacity(sig.len());
    let mut at = 0;

    while let Some(&c) = chars.get(at) {
        // a character (`'"'`) or a lifetime: not a string, and what is inside a character stays
        if c == '\'' && !rules.single {
            let end = char_literal_end(&chars, at).unwrap_or(at + 1);

            out.extend(&chars[at..end]);
            at = end;

            continue;
        }

        if c == '"' || (c == '\'' && rules.single) || (c == '`' && rules.backtick) {
            at = string_end(&chars, at, &rules);
            out.push_str(ELIDED);

            continue;
        }

        if rules.rust_raw
            && c == 'r'
            && let Some(end) = rust_raw_end(&chars, at)
        {
            at = end;
            out.push_str(ELIDED);

            continue;
        }

        out.push(c);
        at += 1;
    }

    Cow::Owned(out)
}

/// The index after the string whose opening quote is at `open`.
fn string_end(chars: &[char], open: usize, rules: &Quotes) -> usize {
    let quote = chars[open];
    let escapes = !(quote == '`' && rules.backtick_raw);
    let triple = rules.python && chars.get(open + 1) == Some(&quote) && chars.get(open + 2) == Some(&quote);
    let holes = if quote == '`' && rules.template {
        Holes::Template
    } else if rules.python && formatted(chars, open) {
        Holes::Format
    } else {
        Holes::None
    };

    scan_string(
        chars,
        open + if triple { 3 } else { 1 },
        quote,
        triple,
        escapes,
        holes,
        rules,
    )
}

/// The index after the string that started before `from` and closes with `quote` (three of them for a triple
/// quoted string). Code in the holes of a template or an f-string is skipped with the strings in it.
fn scan_string(
    chars: &[char],
    from: usize,
    quote: char,
    triple: bool,
    escapes: bool,
    holes: Holes,
    rules: &Quotes,
) -> usize {
    let mut at = from;

    while let Some(&c) = chars.get(at) {
        if escapes && c == '\\' {
            at += 2;
        } else if c == quote && (!triple || (chars.get(at + 1) == Some(&quote) && chars.get(at + 2) == Some(&quote))) {
            return at + if triple { 3 } else { 1 };
        } else if holes == Holes::Template && c == '$' && chars.get(at + 1) == Some(&'{') {
            at = hole_end(chars, at + 2, rules);
        } else if holes == Holes::Format && c == '{' {
            at = if chars.get(at + 1) == Some(&'{') {
                at + 2
            } else {
                hole_end(chars, at + 1, rules)
            };
        } else {
            at += 1;
        }
    }

    chars.len()
}

/// The index after the `}` that closes the code starting at `from` (inside `${` or `{`), skipping the
/// strings in it.
fn hole_end(chars: &[char], from: usize, rules: &Quotes) -> usize {
    let mut at = from;
    let mut depth = 1_usize;

    while let Some(&c) = chars.get(at) {
        match c {
            '"' | '`' | '\'' if c != '\'' || rules.single => {
                at = string_end(chars, at, rules);
            }
            '{' => {
                depth += 1;
                at += 1;
            }
            '}' => {
                depth -= 1;
                at += 1;

                if depth == 0 {
                    return at;
                }
            }
            _ => at += 1,
        }
    }

    chars.len()
}

/// True when the quote at `open` follows a Python string prefix that holds an `f` (`f"..."`, `rf'...'`).
fn formatted(chars: &[char], open: usize) -> bool {
    let mut start = open;

    while start > 0
        && chars
            .get(start - 1)
            .is_some_and(|c| matches!(c, 'r' | 'R' | 'b' | 'B' | 'f' | 'F' | 'u' | 'U'))
    {
        start -= 1;
    }

    let word = |c: &char| c.is_alphanumeric() || *c == '_';

    start < open
        && open - start <= 2
        && !start.checked_sub(1).and_then(|at| chars.get(at)).is_some_and(word)
        && chars[start..open].iter().any(|c| matches!(c, 'f' | 'F'))
}

/// The index after a character literal (`'x'`, `'\n'`, `'\u{1F600}'`, `'"'`) that starts at `start`; `None` for
/// a lifetime or a loop label (`'a`).
fn char_literal_end(chars: &[char], start: usize) -> Option<usize> {
    match chars.get(start + 1)? {
        '\\' => {
            // the escape, then at most a few more characters (`\u{10FFFF}`) up to the closing quote
            (start + 3..=start + 11)
                .find(|at| chars.get(*at) == Some(&'\''))
                .map(|at| at + 1)
        }
        '\'' => None,
        _ if chars.get(start + 2) == Some(&'\'') => Some(start + 3),
        _ => None,
    }
}

/// The index after a Rust raw string (`r"..."`, `r#"..."#`, `br##"..."##`, `cr"..."`) that starts with the `r`
/// at `start`; `None` when that `r` does not start one.
fn rust_raw_end(chars: &[char], start: usize) -> Option<usize> {
    let before = |back: usize| start.checked_sub(back).and_then(|at| chars.get(at)).copied();
    let word = |c: char| c.is_alphanumeric() || c == '_';

    // `r` ends a word (`for"x"` is not valid anyway) unless it is the `r` of a `br` or `cr` prefix
    let prefixed = matches!(before(1), Some('b' | 'c')) && !before(2).is_some_and(word);

    if before(1).is_some_and(word) && !prefixed {
        return None;
    }

    let mut at = start + 1;
    let mut hashes = 0;

    while chars.get(at) == Some(&'#') {
        hashes += 1;
        at += 1;
    }

    if chars.get(at) != Some(&'"') {
        return None;
    }

    at += 1;

    while at < chars.len() {
        if chars.get(at) == Some(&'"') && (1..=hashes).all(|n| chars.get(at + n) == Some(&'#')) {
            return Some(at + 1 + hashes);
        }

        at += 1;
    }

    Some(chars.len())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn elided(sig: &str, lang: &str) -> String {
        elide_strings(sig, lang).into_owned()
    }

    #[test]
    fn a_literal_value_is_replaced_in_every_language() {
        assert_eq!(elided("API_KEY = \"hunter2\"", "python"), "API_KEY = \"…\"");
        assert_eq!(elided("TOKEN = 'hunter2'  # note", "python"), "TOKEN = \"…\"  # note");
        assert_eq!(
            elided("export const token = \"hunter2\";", "ts"),
            "export const token = \"…\";"
        );
        assert_eq!(elided("const k = 'hunter2'", "ts"), "const k = \"…\"");
        assert_eq!(elided("const k = `hunter ${2}`", "ts"), "const k = \"…\"");
        assert_eq!(elided("const K: &str = \"hunter2\";", "rust"), "const K: &str = \"…\";");
        assert_eq!(
            elided("static S: &str = r#\"hun\"ter2\"#;", "rust"),
            "static S: &str = \"…\";"
        );
        assert_eq!(
            elided("const B: &[u8] = br\"hunter2\";", "rust"),
            "const B: &[u8] = b\"…\";"
        );
        assert_eq!(elided("const Key = \"hunter2\"", "go"), "const Key = \"…\"");
        assert_eq!(elided("var Key = `hun\\ter2`", "go"), "var Key = \"…\"");
        assert_eq!(
            elided("SECRET = \"\"\"multi \"line\" hunter2\"\"\"", "python"),
            "SECRET = \"…\""
        );
    }

    #[test]
    fn escapes_and_unclosed_literals_do_not_leak_the_rest() {
        assert_eq!(elided("x = \"a\\\"b hunter2\" + y", "ts"), "x = \"…\" + y");
        assert_eq!(elided("x = \"never closed hunter2", "python"), "x = \"…\"");
        assert_eq!(elided("x = \"cut…", "ts"), "x = \"…\"");
        assert_eq!(elided("x = \"…\"", "ts"), "x = \"…\"");
    }

    #[test]
    fn lifetimes_characters_and_plain_signatures_are_left_alone() {
        assert_eq!(
            elided("fn pick<'a, 'b>(x: &'a str) -> &'b str", "rust"),
            "fn pick<'a, 'b>(x: &'a str) -> &'b str"
        );
        assert_eq!(elided("const NL: char = '\\n';", "rust"), "const NL: char = '\\n';");
        assert_eq!(elided("const R: rune = 'x'", "go"), "const R: rune = 'x'");
        assert_eq!(
            elided("pub fn for_each(items: &[i32])", "rust"),
            "pub fn for_each(items: &[i32])"
        );
        assert_eq!(elided("let var r = 1", "rust"), "let var r = 1");
        assert!(matches!(elide_strings("fn plain()", "rust"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_default_argument_is_elided_too() {
        assert_eq!(
            elided("def connect(password=\"hunter2\"):", "python"),
            "def connect(password=\"…\"):"
        );
    }

    #[test]
    fn templates_and_formatted_strings_are_blanked_with_the_code_in_their_holes() {
        assert_eq!(
            elided("const t = `a ${f(`hunter`, \"hunter\")} b hunter`;", "ts"),
            "const t = \"…\";"
        );
        assert_eq!(
            elided("const t = `a ${ {x: 'hunter'}.x } hunter` + 'hunter'", "ts"),
            "const t = \"…\" + \"…\""
        );
        assert_eq!(
            elided("f\"hunter {d[\"hunter\"]} {x!r:>{w}} {{hunter}}\" + y", "python"),
            "f\"…\" + y"
        );
        assert_eq!(elided("x = rf'hunter {a['hunter']}' + y", "python"), "x = rf\"…\" + y");
        // a brace in a plain string is only a brace
        assert_eq!(elided("x = \"{hunter\" + y", "python"), "x = \"…\" + y");
    }

    #[test]
    fn rust_and_go_keep_their_characters_and_lifetimes_and_blank_the_strings_around_them() {
        assert_eq!(elided("const Q: char = '\"';", "rust"), "const Q: char = '\"';");
        assert_eq!(elided("const Q: char = '\\'';", "rust"), "const Q: char = '\\'';");
        assert_eq!(
            elided("const U: char = '\\u{1F600}';", "rust"),
            "const U: char = '\\u{1F600}';"
        );
        assert_eq!(elided("const B: u8 = b'\"';", "rust"), "const B: u8 = b'\"';");
        assert_eq!(
            elided("fn f<'a>(x: &'a str) -> &'static str { '\"'; \"hunter\" }", "rust"),
            "fn f<'a>(x: &'a str) -> &'static str { '\"'; \"…\" }"
        );
        assert_eq!(
            elided("const Q = '\"'; var K = \"hunter\"", "go"),
            "const Q = '\"'; var K = \"…\""
        );
        assert_eq!(elided("let s = cr#\"hunter\"x\"#;", "rust"), "let s = c\"…\";");
    }

    fn assert_elided(cases: &[(&str, &str)], lang: &str) {
        for (sig, want) in cases {
            assert_eq!(elided(sig, lang), *want, "{sig}");
        }
    }

    #[test]
    fn typescript_keeps_the_literals_of_type_positions() {
        assert_elided(
            &[
                ("type HarnessId = \"a\" | \"b\"", "type HarnessId = \"a\" | \"b\""),
                (
                    "export type Mode = 'fast' | 'slow';",
                    "export type Mode = 'fast' | 'slow';",
                ),
                (
                    "function f(a: \"x\"): \"x\" | \"y\"",
                    "function f(a: \"x\"): \"x\" | \"y\"",
                ),
                ("const x: Foo<\"x\"> = null", "const x: Foo<\"x\"> = null"),
                (
                    "type Pick<Key> = Key extends \"x\" ? Base : never",
                    "type Pick<Key> = Key extends \"x\" ? Base : never",
                ),
                (
                    "type Own<T> = keyof T extends \"a\" ? T[\"b\"] : never",
                    "type Own<T> = keyof T extends \"a\" ? T[\"b\"] : never",
                ),
                (
                    "type Icon = Component<LucideProps, {}, \"Heart\">",
                    "type Icon = Component<LucideProps, {}, \"Heart\">",
                ),
                (
                    "type Props = { kind: \"a\" | \"b\"; on: (m: \"x\") => \"y\" }",
                    "type Props = { kind: \"a\" | \"b\"; on: (m: \"x\") => \"y\" }",
                ),
                (
                    "interface P extends Q<\"a\"> { kind: \"a\" | \"b\" }",
                    "interface P extends Q<\"a\"> { kind: \"a\" | \"b\" }",
                ),
                ("kind: \"a\" | \"b\";", "kind: \"a\" | \"b\";"),
                ("readonly kind: \"a\" = \"b\";", "readonly kind: \"a\" = \"…\";"),
                (
                    "class A<T extends \"a\"> extends B<\"x\"> implements C<\"y\">",
                    "class A<T extends \"a\"> extends B<\"x\"> implements C<\"y\">",
                ),
                (
                    "function f<T extends \"a\" = \"a\">(x: T): T",
                    "function f<T extends \"a\" = \"a\">(x: T): T",
                ),
                (
                    "const v = x as \"a\" | \"b\", w = \"secret\"",
                    "const v = x as \"a\" | \"b\", w = \"…\"",
                ),
                (
                    "const v = { a: 1 } satisfies Record<string, \"k\">;",
                    "const v = { a: 1 } satisfies Record<string, \"k\">;",
                ),
                (
                    "constructor(private mode: \"a\" | \"b\", n = \"secret\")",
                    "constructor(private mode: \"a\" | \"b\", n = \"…\")",
                ),
                (
                    "export const tag = <T extends \"a\">(x: T) => \"v\"",
                    "export const tag = <T extends \"a\">(x: T) => \"…\"",
                ),
            ],
            "ts",
        );
    }

    #[test]
    fn typescript_blanks_the_literals_of_value_positions() {
        assert_elided(
            &[
                ("const x: \"a\" | \"b\" = \"secret\"", "const x: \"a\" | \"b\" = \"…\""),
                ("function f(m: \"x\" = \"y\")", "function f(m: \"x\" = \"…\")"),
                ("foo<\"T\">(\"v\")", "foo<\"T\">(\"…\")"),
                ("const f = (a: \"x\") => \"v\"", "const f = (a: \"x\") => \"…\""),
                (
                    "const f = (a: \"x\"): \"r\" => \"v\"",
                    "const f = (a: \"x\"): \"r\" => \"…\"",
                ),
                ("export const K = \"secret\";", "export const K = \"…\";"),
                (
                    "let k = 'secret', j: \"t\" = 'secret2'",
                    "let k = \"…\", j: \"t\" = \"…\"",
                ),
                (
                    "function f(): 'c' { return 'secret' }",
                    "function f(): 'c' { return \"…\" }",
                ),
                (
                    "const f = (a: 'x'): { k: 'y' } => { return 'secret' }",
                    "const f = (a: 'x'): { k: 'y' } => { return \"…\" }",
                ),
                ("class A { token = \"secret\"", "class A { token = \"…\""),
                ("static token = 'secret'", "static token = \"…\""),
                ("handler: (a = \"secret\") => \"v\",", "handler: (a = \"…\") => \"…\","),
                (
                    "const o = { key: \"secret\", n: \"s2\" }",
                    "const o = { key: \"…\", n: \"…\" }",
                ),
                ("foo(\"secret\", { a: \"s\" })", "foo(\"…\", { a: \"…\" })"),
                ("const c = x ? \"secret\" : \"other\"", "const c = x ? \"…\" : \"…\""),
                ("f(x ? \"a\" : \"b\", y)", "f(x ? \"…\" : \"…\", y)"),
                ("enum E { A = \"secret\", B = 'x' }", "enum E { A = \"…\", B = \"…\" }"),
                ("const t = `a ${b} secret`", "const t = \"…\""),
                ("type T = `on${string}`", "type T = \"…\""),
                ("const r = a < \"secret\"", "const r = a < \"…\""),
                (
                    "const r = (<div title=\"secret\">(x)</div>)",
                    "const r = (<div title=\"…\">(x)</div>)",
                ),
                ("const v = a ?? \"secret\"", "const v = a ?? \"…\""),
                ("const v = x as T || \"secret\"", "const v = x as T || \"…\""),
                ("const v = x as T + \"secret\"", "const v = x as T + \"…\""),
                ("export * as ns from \"secret\"", "export * as ns from \"…\""),
                ("const o = obj[\"secret\"]", "const o = obj[\"…\"]"),
                ("let a: \"x\" = 1; // \"secret\"", "let a: \"x\" = 1; // \"…\""),
            ],
            "ts",
        );
    }

    #[test]
    fn a_value_after_a_parenthesised_return_type_or_a_comment_is_blanked() {
        assert_elided(
            &[
                (
                    "const f = (): (string | undefined) => \"SECRET\"",
                    "const f = (): (string | undefined) => \"…\"",
                ),
                (
                    "const g = (): (() => void) => { return \"SECRET\" }",
                    "const g = (): (() => void) => { return \"…\" }",
                ),
                (
                    "handler = (a: \"x\"): (\"y\") => \"SECRET\"",
                    "handler = (a: \"x\"): (\"y\") => \"…\"",
                ),
                // the return type of a function is still shown
                (
                    "function f(a: \"x\"): (\"y\" | \"z\") { return \"SECRET\" }",
                    "function f(a: \"x\"): (\"y\" | \"z\") { return \"…\" }",
                ),
                ("const K = /* prod: */ \"SECRET\"", "const K = /* prod: */ \"…\""),
                (
                    "const K /* a: */ = /* b = */ \"SECRET\" // c: \"x\"",
                    "const K /* a: */ = /* b = */ \"…\" // c: \"…\"",
                ),
                (
                    "const K = 1 // note: \nlet j = \"SECRET\"",
                    "const K = 1 // note: \nlet j = \"…\"",
                ),
                (
                    "const K: /* t */ \"a\" = /* it's */ \"SECRET\"",
                    "const K: /* t */ \"a\" = /* it\"…\"",
                ),
            ],
            "ts",
        );
    }

    #[test]
    fn a_slash_that_may_open_a_regular_expression_blanks_the_rest_of_the_line() {
        assert_elided(
            &[
                ("/\"/.test(v) ? \"SECRET\" : \"\"", "/…"),
                ("const k = v.split(/'/) && \"SECRET\"", "const k = v.split(/…"),
                ("const K = /a:b/ + \"SECRET\"", "const K = /…"),
                ("return /[\"]/.test(v) ? \"SECRET\" : \"\"", "return /…"),
                // a division, or a slash with nothing quoted after it, is left alone
                ("const h = total / 2 + \"SECRET\"", "const h = total / 2 + \"…\""),
                ("const h = a[0] / b + \"SECRET\"", "const h = a[0] / b + \"…\""),
                ("const r = /ab+c/.test(v)", "const r = /ab+c/.test(v)"),
                (
                    "const r = (<div title=\"secret\">(x)</div>)",
                    "const r = (<div title=\"…\">(x)</div>)",
                ),
            ],
            "ts",
        );
    }

    #[test]
    fn a_python_lambda_keeps_its_colon_across_the_commas_of_its_parameters() {
        assert_elided(
            &[
                ("f = lambda a, b: \"SECRET\"", "f = lambda a, b: \"…\""),
                ("cb=lambda req, resp: \"SECRET\"", "cb=lambda req, resp: \"…\""),
                (
                    "h = g(lambda a, *rest, k=1: \"SECRET\", n=\"s\")",
                    "h = g(lambda a, *rest, k=1: \"…\", n=\"…\")",
                ),
                (
                    "f = lambda a, b: lambda c, d: \"SECRET\"",
                    "f = lambda a, b: lambda c, d: \"…\"",
                ),
            ],
            "python",
        );
    }

    #[test]
    fn a_member_that_may_hold_a_function_keeps_no_value_after_its_key() {
        // an interface member and an object-literal member look alike: the part after `=>` is blanked
        assert_elided(
            &[
                ("on: (m: \"x\") => \"y\"", "on: (m: \"x\") => \"…\""),
                ("run: async (\n a = \"secret\"", "run: async (\n a = \"…\""),
                ("run: function (a = \"secret\") {", "run: function (a = \"…\") {"),
            ],
            "ts",
        );
    }

    #[test]
    fn python_keeps_the_literals_of_annotations() {
        assert_elided(
            &[
                ("def f(m: Literal[\"a\"] = \"b\")", "def f(m: Literal[\"a\"] = \"…\")"),
                (
                    "def f(m: Literal[\"a\", 'b'], n: \"Foo\" = None) -> Literal[\"x\"]:",
                    "def f(m: Literal[\"a\", 'b'], n: \"Foo\" = None) -> Literal[\"x\"]:",
                ),
                ("x: \"Foo\" = None", "x: \"Foo\" = None"),
                ("x: list[\"Foo\"] = [\"secret\"]", "x: list[\"Foo\"] = [\"…\"]"),
                ("Mode = Literal[\"a\", \"b\"]", "Mode = Literal[\"a\", \"b\"]"),
                ("type Mode = Literal[\"a\"]", "type Mode = Literal[\"a\"]"),
                (
                    "self.m: typing.Literal[\"a\"] = \"secret\"",
                    "self.m: typing.Literal[\"a\"] = \"…\"",
                ),
                (
                    "async def f(self, *, mode: Literal[\"fast\"] = \"fast\", key=\"secret\") -> \"Foo\":",
                    "async def f(self, *, mode: Literal[\"fast\"] = \"…\", key=\"…\") -> \"Foo\":",
                ),
            ],
            "python",
        );
    }

    #[test]
    fn python_blanks_the_literals_of_values() {
        assert_elided(
            &[
                ("API_KEY = \"secret\"", "API_KEY = \"…\""),
                ("KEY: str = 'secret'", "KEY: str = \"…\""),
                (
                    "def f(mode=\"secret\"): return \"v\"",
                    "def f(mode=\"…\"): return \"…\"",
                ),
                ("def f(): return {\"a\": \"secret\"}", "def f(): return {\"…\": \"…\"}"),
                ("x = d[\"secret\"]", "x = d[\"…\"]"),
                ("x: int = Query(\"secret\")", "x: int = Query(\"…\")"),
                (
                    "x: Annotated[str, Field(default=\"secret\")]",
                    "x: Annotated[str, Field(default=\"…\")]",
                ),
                ("x: Foo(\"secret\") = 1", "x: Foo(\"…\") = 1"),
                ("f = lambda a: \"secret\"", "f = lambda a: \"…\""),
                (
                    "h = g(lambda a: \"secret\", b=\"s\")",
                    "h = g(lambda a: \"…\", b=\"…\")",
                ),
                ("f = f\"{a} secret\"", "f = f\"…\""),
                ("x: \"\"\"Foo\"\"\" = 1", "x: \"…\" = 1"),
                ("if (y := \"secret\"):", "if (y := \"…\"):"),
                ("x = 1  # \"secret\"", "x = 1  # \"…\""),
            ],
            "python",
        );
    }

    #[test]
    fn a_long_or_unclosed_type_literal_is_blanked() {
        let long = "a".repeat(65);

        assert_eq!(elided(&format!("type T = \"{long}\""), "ts"), "type T = \"…\"");
        assert_eq!(elided("type T = \"a\" | \"cut", "ts"), "type T = \"a\" | \"…\"");
        assert_eq!(elided("type T = \"a\\\"", "ts"), "type T = \"…\"");
    }

    #[test]
    fn rust_and_go_are_unchanged_by_type_positions() {
        assert_eq!(elided("const K: &str = \"s\";", "rust"), "const K: &str = \"…\";");
        assert_eq!(
            elided("fn f(a: &str) -> Foo<\"x\">", "rust"),
            "fn f(a: &str) -> Foo<\"…\">"
        );
        assert_eq!(elided("Key string `json:\"key\"`", "go"), "Key string \"…\"");
    }

    #[test]
    fn deep_nesting_is_blanked_and_bounded() {
        let sig = format!("type T = {}\"a\"", "[".repeat(200));

        assert_eq!(elided(&sig, "ts"), format!("type T = {}\"…\"", "[".repeat(200)));
    }

    #[test]
    fn arbitrary_lines_are_scanned_without_a_panic() {
        let tokens = [
            "\"s\"",
            "'s'",
            "`s${",
            "}",
            "(",
            ")",
            "[",
            "]",
            "{",
            "<",
            ">",
            ":",
            "=",
            "=>",
            "?",
            ",",
            ";",
            "as",
            "type",
            "Literal",
            "lambda",
            "->",
            "x",
            " ",
            "#",
            "//",
            "\\",
            "\"",
            "'",
            "interface",
            "const",
            "def",
        ];
        let mut seed = 0x2545_f491_u64;

        for lang in ["ts", "python"] {
            for _ in 0..4_000 {
                let mut sig = String::new();

                for _ in 0..24 {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    sig.push_str(tokens[(seed >> 33) as usize % tokens.len()]);
                }

                let out = elided(&sig, lang);

                assert!(out.chars().count() <= sig.chars().count() * 3, "{lang}: {sig} -> {out}");
            }
        }
    }
}
