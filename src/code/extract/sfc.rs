//! Single-file components (Svelte, Vue, Astro, ...): one generic handler, no framework knowledge.
//!
//! The file is a `component` symbol named after the file. Its `<script>` blocks (and an Astro-style `---`
//! frontmatter) are parsed as TypeScript. The rest of the file (the template) is scanned for component tags
//! (`<FooBar>`, `<Foo.Bar>`, `<foo-bar>`) and for quoted code names (`invoke('git_file_lines')`). The names the
//! script imports are also looked for in the template's expressions (see `sfc_template`).
//!
//! The script, style and frontmatter regions are blanked in the template copy byte for byte (one ASCII space
//! per byte, newlines kept), so offsets and line numbers of the template never shift, whatever the text holds.

use std::ops::Range;
use std::path::Path;

use tree_sitter::Parser;

use super::{Extract, Sym, ident_like, out_of_time, push_ref};

/// The template scan looks at the clock this often, in bytes.
pub(super) const SCAN_CHECK: usize = 1 << 16;

/// A quoted string longer than this is not a code name.
const MAX_QUOTED: usize = 80;

/// Reads a single-file component.
pub(super) fn extract(parser: &mut Parser, path: &str, src: &str, out: &mut Extract) {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("component")
        .to_string();
    let component = out.add_sym(Sym {
        name: stem.clone(),
        kind: "component",
        parent: None,
        line: 1,
        end_line: src.lines().count().max(1),
        sig: format!("<{stem}>"),
        ret: None,
        test: false,
    });

    let owner = Some(component);
    let language = super::ts::typescript();
    let mut markup = src.as_bytes().to_vec();

    if let Some((code, region)) = frontmatter(src) {
        super::ts::extract_block(parser, &language, code, 1, owner, false, out);
        blank(&mut markup, region);
    }

    // the blocks come in order, so the lines before each one are counted once, not from the top every time
    let mut counted = (0_usize, 0_usize);

    for block in blocks(src, "script") {
        // every block is a parse of its own: the file's time is checked between them
        if out_of_time() {
            out.dropped += 1;

            break;
        }

        let code = src.get(block.body.clone()).unwrap_or("");
        let between = src
            .as_bytes()
            .get(counted.0..block.body.start)
            .map_or(0, count_newlines);
        let line_off = counted.1 + between;

        counted = (block.body.start, line_off);

        super::ts::extract_block(parser, &language, code, line_off, owner, false, out);
        blank(&mut markup, block.whole);
    }

    for block in blocks(src, "style") {
        blank(&mut markup, block.whole);
    }

    scan_template(&markup, owner, out);

    let vue = Path::new(path).extension().is_some_and(|ext| ext == "vue");

    super::sfc_template::extract(&markup, vue, owner, out);
    super::ts::keep_function_values(out);
}

fn count_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| **b == b'\n').count()
}

/// Replaces every byte of `range` but newlines by an ASCII space: a multi-byte character becomes as many
/// spaces as it has bytes.
fn blank(markup: &mut [u8], range: Range<usize>) {
    if let Some(region) = markup.get_mut(range) {
        for byte in region {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
}

/// The code of a leading `---` ... `---` block and the byte range to blank.
fn frontmatter(src: &str) -> Option<(&str, Range<usize>)> {
    let opening = ["---\n", "---\r\n"]
        .into_iter()
        .find(|opening| src.starts_with(opening))?;
    let rest = src.get(opening.len()..)?;
    let end = rest.find("\n---")?;

    Some((rest.get(..end)?, 0..opening.len() + end + "\n---".len()))
}

/// A `<tag ...> ... </tag>` block.
struct Block {
    /// The text between the tags.
    body: Range<usize>,
    /// From `<tag` through the closing `>`.
    whole: Range<usize>,
}

/// The `<tag>` blocks of `src`, in order. Quoted attribute values may hold a `>`; a self-closing tag has
/// no block; a tag that never closes ends the search.
fn blocks(src: &str, tag: &str) -> Vec<Block> {
    let bytes = src.as_bytes();
    let opening = format!("<{tag}");
    let closing = format!("</{tag}");
    let mut found = Vec::new();
    let mut at = 0;

    while let Some(offset) = src.get(at..).and_then(|rest| rest.find(&opening)) {
        let open = at + offset;
        let after = open + opening.len();

        // `<scripts>` or `<style-guide>` are other tags
        if !bytes
            .get(after)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            at = after;
            continue;
        }

        let Some((tag_end, self_closing)) = tag_end(bytes, after) else {
            break;
        };

        if self_closing {
            at = tag_end;
            continue;
        }

        let Some(close_offset) = src.get(tag_end..).and_then(|rest| rest.find(&closing)) else {
            break;
        };
        let close = tag_end + close_offset;
        let whole_end = src
            .get(close..)
            .and_then(|rest| rest.find('>'))
            .map_or(src.len(), |gt| close + gt + 1);

        found.push(Block {
            body: tag_end..close,
            whole: open..whole_end,
        });
        at = whole_end;
    }

    found
}

/// The offset just past the `>` that ends the opening tag starting before `from`, and whether it was `/>`.
fn tag_end(bytes: &[u8], from: usize) -> Option<(usize, bool)> {
    let mut quote: Option<u8> = None;

    for (i, byte) in bytes.get(from..)?.iter().enumerate() {
        match (quote, *byte) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(*byte),
            (None, b'>') => {
                let at = from + i;
                let self_closing = at > from && bytes.get(at - 1) == Some(&b'/');

                return Some((at + 1, self_closing));
            }
            _ => {}
        }
    }

    None
}

/// Component tags and quoted code names of the template.
fn scan_template(markup: &[u8], owner: Option<usize>, out: &mut Extract) {
    let mut line = 1;
    let mut i = 0;

    while let Some(&byte) = markup.get(i) {
        if i.is_multiple_of(SCAN_CHECK) && out_of_time() {
            out.dropped += 1;

            return;
        }

        match byte {
            b'\n' => line += 1,
            b'<' if markup.get(i + 1).is_some_and(u8::is_ascii_alphabetic) => {
                let tag: Vec<u8> = markup
                    .get(i + 1..)
                    .unwrap_or(&[])
                    .iter()
                    .copied()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
                    .collect();

                if let Some(name) = component_name(&String::from_utf8_lossy(&tag)) {
                    push_ref(out, owner, &name, "component", None, line);
                }
            }
            b'\'' | b'"' => {
                // a quoted code name inside a template expression: invoke('x'), emit("y"), ...
                let window = markup.get(i + 1..).unwrap_or(&[]);

                if let Some(len) = window.iter().take(MAX_QUOTED).position(|c| *c == byte)
                    && let Some(value) = window.get(..len).and_then(|v| std::str::from_utf8(v).ok())
                    && ident_like(value)
                {
                    push_ref(out, owner, value, "str", None, line);
                }
            }
            _ => {}
        }

        i += 1;
    }
}

/// The component a tag names: `<FooBar>`, `<Foo.Bar>` (Bar) or `<foo-bar>` (FooBar); plain HTML tags have
/// neither a capital nor a dash.
fn component_name(tag: &str) -> Option<String> {
    let tag = tag.rsplit('.').next().unwrap_or(tag);

    if tag.contains('-') {
        let name: String = tag
            .split('-')
            .map(|part| {
                let mut chars = part.chars();

                chars
                    .next()
                    .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect();

        return (!name.is_empty()).then_some(name);
    }

    tag.starts_with(|c: char| c.is_ascii_uppercase())
        .then(|| tag.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str, src: &str) -> Extract {
        let mut out = Extract::default();
        extract(&mut Parser::new(), path, src, &mut out);
        out
    }

    #[test]
    fn script_functions_template_components_and_strings() {
        let out = run(
            "ui/DiffPanel.svelte",
            "<script lang=\"ts\">\n  import Card from './Card.svelte';\n  function load() { return invoke('git_file_lines'); }\n</script>\n\n<Card on:click={load} />\n<my-widget></my-widget>\n<div>{fmt('some_helper')}</div>\n",
        );

        assert_eq!(out.syms[0].kind, "component");
        assert_eq!(out.syms[0].name, "DiffPanel");

        let load = out.syms.iter().find(|s| s.name == "load").map(|s| s.line);
        assert_eq!(load, Some(3));

        let components: Vec<(&str, usize)> = out
            .refs
            .iter()
            .filter(|r| r.kind == "component")
            .map(|r| (r.name.as_str(), r.line))
            .collect();
        assert_eq!(components, vec![("Card", 6), ("MyWidget", 7)]);

        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "str" && r.name == "git_file_lines" && r.line == 3)
        );
        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "str" && r.name == "some_helper" && r.line == 8)
        );
    }

    #[test]
    fn functions_of_later_script_blocks_get_their_own_lines() {
        let out = run(
            "ui/Two.svelte",
            "<script context=\"module\">\nexport function first() {}\n</script>\n\n<p>text</p>\n<script>\n\nfunction second() {}\n</script>\n",
        );
        let line = |name: &str| out.syms.iter().find(|s| s.name == name).map(|s| s.line);

        assert_eq!(line("first"), Some(2));
        assert_eq!(line("second"), Some(8));
    }

    #[test]
    fn a_file_of_many_script_blocks_is_read_in_linear_time() {
        let blocks = 15_000;
        let src = "<script>function a(){}</script>\n".repeat(blocks);
        let started = std::time::Instant::now();
        let out = run("ui/Many.svelte", &src);

        // the lines before a block were once counted from the top of the file for every block
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out.syms.iter().filter(|s| s.name == "a").count(), blocks);
        assert_eq!(
            out.syms.iter().filter(|s| s.name == "a").map(|s| s.line).max(),
            Some(blocks)
        );
    }

    #[test]
    fn multibyte_text_before_the_script_end_keeps_offsets_and_does_not_panic() {
        let src = "<script>\n// zażółć gęślą jaźń \u{1F980} 日本語\nfunction łaskawy() { return 1 }\n</script>\n<Wide />\n<p>żółć_x</p>\n";
        let out = run("multi.svelte", src);

        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "component" && r.name == "Wide" && r.line == 5)
        );
        // nothing of the blanked script leaks into the template scan
        assert!(out.refs.iter().all(|r| r.name != "script"));
    }

    #[test]
    fn blanking_keeps_the_byte_length_and_the_newlines() {
        let src = "ab\u{1F980}\ncd\u{e9}";
        let mut markup = src.as_bytes().to_vec();
        blank(&mut markup, 0..src.len());

        assert_eq!(markup.len(), src.len());
        assert_eq!(std::str::from_utf8(&markup), Ok("      \n    "));
    }

    #[test]
    fn frontmatter_and_generic_attributes_are_script() {
        let out = run(
            "page.astro",
            "---\nconst title = fetchTitle();\nfunction fetchTitle() { return 1 }\n---\n<Layout title={title} />\n",
        );
        assert!(out.syms.iter().any(|s| s.name == "fetchTitle" && s.line == 3));
        assert!(
            out.refs
                .iter()
                .any(|r| r.kind == "component" && r.name == "Layout" && r.line == 5)
        );

        let out = run(
            "gen.svelte",
            "<script lang=\"ts\" generics=\"T extends Record<string, unknown>\">\nfunction inner() {}\n</script>\n<X />\n",
        );
        assert!(out.syms.iter().any(|s| s.name == "inner" && s.line == 2));
        assert!(out.refs.iter().any(|r| r.kind == "component" && r.name == "X"));
    }

    fn template_refs(out: &Extract) -> Vec<(&str, &'static str, Option<&str>, usize)> {
        out.refs
            .iter()
            .filter(|r| matches!(r.kind, "use" | "call" | "mcall"))
            .map(|r| (r.name.as_str(), r.kind, r.qual.as_deref(), r.line))
            .collect()
    }

    #[test]
    fn imported_names_are_read_in_nested_expressions_and_never_in_strings_or_properties() {
        let out = run(
            "page.astro",
            "---\nimport Card from './Card.astro';\nimport { fmt, path as p } from './x';\nimport * as ns from './y';\n---\n<Card />\n{items.map((i) => <li class={`a ${fmt (i)}`}>{p}</li>)}\n<p title=\"fmt\">{ns.run(1)} {ns.NAME} {obj.fmt} {'fmt'} {/* fmt */}</p>\n{cond && <Card />}\n<!-- {fmt(1)} -->\n<p>fmt(2)</p>\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![
                ("fmt", "call", None, 7),
                ("p", "use", None, 7),
                ("run", "mcall", Some("ns"), 8),
                ("NAME", "use", Some("ns"), 8),
            ]
        );
        // the tag `<Card />` is a component reference, not a use of the import
        assert!(out.refs.iter().all(|r| !(r.name == "Card" && r.kind == "use")));
    }

    #[test]
    fn a_name_the_script_used_on_the_same_line_is_recorded_once() {
        let out = run(
            "ui/Row.svelte",
            "<script>import { fmt } from './x'; fmt(1);</script>\n{fmt(2)}{fmt(3)}\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![("fmt", "call", None, 1), ("fmt", "call", None, 2)]
        );
    }

    #[test]
    fn vue_reads_mustaches_and_dynamic_attributes_only() {
        let src = "<script setup>\nimport { fmt, go } from './x';\n</script>\n<template>\n  <a :href=\"fmt('a>b')\" @click='go()' title=\"fmt\" v-if=\"ok\">text</a>\n  <p>{{ fmt }} { go() }</p>\n</template>\n";
        let out = run("Row.vue", src);

        assert_eq!(
            template_refs(&out),
            vec![
                ("fmt", "call", None, 5),
                ("go", "call", None, 5),
                ("fmt", "use", None, 6)
            ]
        );
    }

    #[test]
    fn a_brace_that_never_closes_leaves_the_rest_of_the_template_unread() {
        let out = run(
            "a.svelte",
            "<script>import { fmt } from './x';</script>\n<p>{ fmt(1)</p>\n{fmt(2)}\n",
        );

        assert!(template_refs(&out).is_empty());
    }

    #[test]
    fn markup_in_an_expression_is_text_and_only_its_braces_are_code() {
        let out = run(
            "page.astro",
            "---\nimport { price, title, fmt } from './x';\n---\n{items.map(i => <li title={i.t} class=\"price\" {...title}>Total price: {fmt(i.p)} <br> <b>title</b></li>)}\n{a < price} {a<title}\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![
                ("title", "use", None, 4),
                ("fmt", "call", None, 4),
                ("price", "use", None, 5),
                ("title", "use", None, 5),
            ]
        );
    }

    #[test]
    fn a_quote_or_a_slash_in_markup_text_in_an_expression_does_not_swallow_the_template() {
        let out = run(
            "page.astro",
            "---\nimport { fmt } from './x';\n---\n{ok && <p>use the ` key, it's <a href=http://x>here</a></p>}\n{fmt(1)}\n",
        );

        assert_eq!(template_refs(&out), vec![("fmt", "call", None, 5)]);
    }

    #[test]
    fn a_less_than_in_vue_text_does_not_hide_a_mustache() {
        let out = run(
            "Row.vue",
            "<script setup>\nimport { fmt } from './x';\n</script>\n<template>\n  <p>if a<b then {{ fmt(x) }} done</p>\n  <p>1 < 2 {{ fmt(y) }}</p>\n</template>\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![("fmt", "call", None, 5), ("fmt", "call", None, 6)]
        );
    }

    #[test]
    fn calls_and_uses_of_different_namespaces_on_a_line_are_all_kept() {
        let out = run(
            "page.astro",
            "---\nimport * as ns from './a';\nimport * as other from './b';\nimport { run } from './c';\n---\n{ns.run(1)} {other.run(2)}\n{run} {ns.run()}\n",
        );

        assert_eq!(
            template_refs(&out),
            vec![
                ("run", "mcall", Some("ns"), 6),
                ("run", "mcall", Some("other"), 6),
                ("run", "use", None, 7),
                ("run", "mcall", Some("ns"), 7),
            ]
        );
    }

    #[test]
    fn unterminated_blocks_do_not_panic() {
        for src in [
            "<script>",
            "<script lang=\"ts\"",
            "<script>function a(",
            "---\n",
            "---\nx\n--",
            "<style>",
            "<script/>",
        ] {
            let _ = run("a.vue", src);
        }
    }
}
