//! The definitions of the code index tools: `code_map`, `code_symbol`, `code_impact` and `code_path`.
//! They are listed only while the code index is on.

use serde_json::{Value, json};

use super::tools::{text, tool};

/// The map budget in tokens: default and bounds.
pub(super) const MAP_BUDGET_DEFAULT: i64 = 700;
pub(super) const MAP_BUDGET_MIN: i64 = 200;
pub(super) const MAP_BUDGET_MAX: i64 = 4000;

/// The `limit` of `code_symbol`: default and bounds.
pub(super) const SYMBOL_LIMIT_DEFAULT: i64 = 20;
pub(super) const SYMBOL_LIMIT_MIN: i64 = 1;
pub(super) const SYMBOL_LIMIT_MAX: i64 = 50;

/// The `depth` of `code_impact`: default and bounds.
pub(super) const IMPACT_DEPTH_DEFAULT: i64 = 2;
pub(super) const IMPACT_DEPTH_MIN: i64 = 1;
pub(super) const IMPACT_DEPTH_MAX: i64 = 4;

/// The `max_depth` of `code_path`: default and bounds.
pub(super) const PATH_DEPTH_DEFAULT: i64 = 8;
pub(super) const PATH_DEPTH_MIN: i64 = 1;
pub(super) const PATH_DEPTH_MAX: i64 = 10;

/// The longest `symbol`, `focus`, `from` or `to` accepted, in characters.
pub(super) const QUERY_CHARS_MAX: usize = 300;

/// The line added to the server instructions while the code index is on.
pub(super) const INSTRUCTION: &str = "- Before grep: code_map, code_symbol, code_impact, code_path. A to B: code_path \
(symbol, file, command or words). Grepped for a start? Use code_path from there.";

/// The code index tools.
pub(super) fn definitions() -> Vec<Value> {
    vec![code_map(), code_symbol(), code_impact(), code_path()]
}

fn code_map() -> Value {
    tool(
        "code_map",
        "Ranked map of a project's key files and their signatures. Use it first in an unfamiliar area, before \
         grep or opening files. focus narrows it to a path prefix.",
        json!({
            "project_path": project_path(),
            "focus": text("Path prefix to centre the map on, such as src/api"),
            "budget": {
                "type": "integer",
                "minimum": MAP_BUDGET_MIN,
                "maximum": MAP_BUDGET_MAX,
                "default": MAP_BUDGET_DEFAULT,
                "description": "Size of the map in tokens",
            },
        }),
        &["project_path"],
    )
}

fn code_symbol() -> Value {
    tool(
        "code_symbol",
        "Where a function, type or constant (or a file's definitions) is defined, what it uses and who uses it, with \
         file:line and the call-site source line. Prefer it to grep. `(by name)` and `?` mark weak links.",
        json!({
            "project_path": project_path(),
            "symbol": text("name, Type::name, module::name, path/fragment::name or a file path"),
            "limit": {
                "type": "integer",
                "minimum": SYMBOL_LIMIT_MIN,
                "maximum": SYMBOL_LIMIT_MAX,
                "default": SYMBOL_LIMIT_DEFAULT,
                "description": "Entries per list",
            },
        }),
        &["project_path", "symbol"],
    )
}

fn code_impact() -> Value {
    tool(
        "code_impact",
        "Everything that depends on a symbol or a file, by caller depth, as caller → callee. Use it before changing a \
         signature or behaviour. `(by name)` and `?` mark weak links.",
        json!({
            "project_path": project_path(),
            "symbol": text("name, Type::name, module::name, path/fragment::name or a file path"),
            "depth": {
                "type": "integer",
                "minimum": IMPACT_DEPTH_MIN,
                "maximum": IMPACT_DEPTH_MAX,
                "default": IMPACT_DEPTH_DEFAULT,
                "description": "Caller hops",
            },
        }),
        &["project_path", "symbol"],
    )
}

fn code_path() -> Value {
    tool(
        "code_path",
        "How A reaches B: the shortest chain of calls, renders, shared constants and name links, with call-site \
         file:line and source. Ends: symbol, file, command or plain words. If you grepped for a start, call this \
         from there.",
        json!({
            "project_path": project_path(),
            "from": text(
                "Where the chain starts: a symbol (name, Type::name, path/fragment::name), a file path, a command \
                 or event name, or plain words of the question"
            ),
            "to": text("Where the chain ends, written the same way"),
            "max_depth": {
                "type": "integer",
                "minimum": PATH_DEPTH_MIN,
                "maximum": PATH_DEPTH_MAX,
                "default": PATH_DEPTH_DEFAULT,
                "description": "Longest chain in hops",
            },
        }),
        &["project_path", "from", "to"],
    )
}

fn project_path() -> Value {
    text("Absolute path of the repository you are working in (its git root)")
}
