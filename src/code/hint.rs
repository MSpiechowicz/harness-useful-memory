//! `harness-memory code hint`: one line that tells an agent a name it is searching for is in the code index.
//!
//! Harness Useful Skills runs it from a hook that sees a text search (`grep`, `rg`) for a name. It must cost
//! next to nothing and change nothing, so it never builds, refreshes or creates the index and never starts
//! the worker: it opens `code.sqlite3` read-only and asks one indexed question, whether the repository's
//! definitions of the name exist. The graph is not loaded.
//!
//! There is no answer (`None`) unless the code index is on, the project is a git repository the code tools
//! would read, the repository has an index whose current generation a worker validated, and the query is a
//! name (`name`, `Type::name`, `Type.name`) that the index defines.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::store::SCHEMA_VERSION;
use super::switch;
use crate::project::{canonical_home, code_root_refusal};
use crate::support::Env;
use crate::support::paths::validate_project_dir;
use crate::support::private::data_folder;
use crate::support::text::display_escaped;

/// The length of a name that is asked about.
const QUERY_LEN: std::ops::RangeInclusive<usize> = 3..=80;

/// The most characters of a hint.
const MAX_HINT: usize = 300;

/// Definitions counted for a name: past it the hint says `N+`.
const MAX_COUNTED: usize = 1_000;

/// How long a read waits for a writer: a hint is never worth a wait.
const BUSY: Duration = Duration::from_millis(50);

/// The hint for `query` in the repository around `project`, or `None`.
pub fn hint(env: &Env, project: &str, query: &str) -> Option<String> {
    if !name_like(query) {
        return None;
    }

    let data = data_folder(env)?;

    if !switch::enabled(env, &data) {
        return None;
    }

    let dir = validate_project_dir(env, Path::new(project)).ok()?;
    let root = work_tree(&dir)?;

    if code_root_refusal(&canonical_home(env), &root).is_some() {
        return None;
    }

    let conn = open(&data.join("code.sqlite3"))?;
    let repo = validated(&conn, root.to_str()?)?;
    let count = definitions(&conn, repo, query)?;

    (count > 0).then(|| text(query, count))
}

/// True for a name an agent could search for: `^[A-Za-z_$][\w$]*(::|\.)?[\w$]*$`, 3 to 80 characters.
fn name_like(query: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    let Some(first) = query.chars().next() else {
        return false;
    };

    if !QUERY_LEN.contains(&query.chars().count()) || !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }

    // at most one separator, and words on both sides of it
    let (head, tail) = query
        .split_once("::")
        .or_else(|| query.split_once('.'))
        .unwrap_or((query, ""));

    head.chars().all(word) && tail.chars().all(word)
}

/// The git work tree that holds `dir`: the nearest folder with a `.git` entry. No git is run.
fn work_tree(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|folder| std::fs::symlink_metadata(folder.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

/// The index file, read-only: `None` when it does not exist, is not a file or is not of the current schema.
fn open(path: &Path) -> Option<Connection> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }

    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;

    conn.busy_timeout(BUSY).ok()?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).ok()?;

    (version == SCHEMA_VERSION).then_some(conn)
}

/// The id of the repository at `root` when its current generation is one a worker validated and it is not
/// marked too large.
fn validated(conn: &Connection, root: &str) -> Option<i64> {
    conn.query_row(
        "SELECT id, generation, validated_generation, too_large IS NOT NULL FROM repos WHERE root = ?1",
        [root],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
            ))
        },
    )
    .optional()
    .ok()?
    .filter(|(_, generation, validated, too_large)| *validated >= 0 && validated == generation && !too_large)
    .map(|(id, ..)| id)
}

/// How many definitions of the repository the name `query` (`name`, `Type::name`, `Type.name`) names, at most
/// [`MAX_COUNTED`].
fn definitions(conn: &Connection, repo: i64, query: &str) -> Option<usize> {
    let split = query.split_once("::").or_else(|| query.split_once('.'));
    let (qual, name) = match split {
        Some((qual, name)) => (Some(qual), name),
        None => (None, query),
    };

    if name.is_empty() {
        return None;
    }

    let mut statement = conn
        .prepare_cached(
            "SELECT s.parent, f.path FROM symbols s JOIN files f ON f.id = s.file_id \
             WHERE s.repo_id = ?1 AND s.name = ?2 LIMIT ?3",
        )
        .ok()?;
    let rows = statement
        .query_map(rusqlite::params![repo, name, MAX_COUNTED as i64], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;
    let mut count = 0;

    for row in rows {
        let (parent, path) = row.ok()?;

        if qual.is_none_or(|qual| parent.as_deref() == Some(qual) || path.contains(qual)) {
            count += 1;
        }
    }

    Some(count)
}

/// The one line for `count` definitions of `query`. The name is written out like any text from outside.
fn text(query: &str, count: usize) -> String {
    let name = display_escaped(query);
    let many = if count >= MAX_COUNTED {
        format!("{MAX_COUNTED}+ definitions")
    } else if count == 1 {
        "1 definition".to_string()
    } else {
        format!("{count} definitions")
    };
    let full = format!(
        "`{name}` is in the code index ({many}). code_symbol {name} shows callers and call sites, code_impact {name} what breaks, code_path traces flows."
    );

    if full.chars().count() <= MAX_HINT {
        return full;
    }

    format!(
        "`{name}` is in the code index ({many}): code_symbol shows callers and call sites, code_impact what breaks, code_path traces flows."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_names_are_asked_about() {
        for good in [
            "foo",
            "validate_paths",
            "Type::name",
            "Type.name",
            "$el",
            "_x1",
            "a.b",
            "Aa::Bb",
        ] {
            assert!(name_like(good), "{good}");
        }

        for bad in [
            "",
            "ab",
            "1abc",
            "a::b::c",
            "a.b.c",
            "a::b.c",
            "a-b",
            "a b",
            "src/lib.rs",
            "foo()",
            "foo\n",
            "fooé",
            &"x".repeat(81),
        ] {
            assert!(!name_like(bad), "{bad}");
        }

        assert!(name_like(&"x".repeat(80)));
    }

    #[test]
    fn the_longest_hint_stays_within_its_line() {
        let query = format!("{}::{}", "A".repeat(37), "b".repeat(41));

        assert!(name_like(&query));
        assert!(text(&query, 1000).chars().count() <= MAX_HINT);
        assert!(text("validate_paths", 3).contains("code_symbol validate_paths shows callers"));
    }
}
