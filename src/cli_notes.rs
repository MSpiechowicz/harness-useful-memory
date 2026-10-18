//! The note commands of the `harness-memory` executable, for the person at the terminal (and
//! Harness Desktop on their behalf), never for an agent's MCP calls. Each sees the notes
//! `note_search` would with `project_path` set to `--project`: that repository's notes and the
//! global ones, only the global ones without `--project`. The store is the one the MCP server
//! opens (the local file, the team database, or both split by scope).
//!
//! - `harness-memory notes list --json [--project PATH] [--kind KIND] [--scope repo|global]
//!   [--query TEXT] [--sort updated|title|kind|scope|place] [--order asc|desc] [--offset N]
//!   [--limit N]` prints `{"total": N, "kinds": [{"kind", "count"}], "scopes": {"repo": N,
//!   "global": N}, "notes": [...]}`: one page of the notes (see [`crate::store::listing`] for the
//!   matching and the order). `--query` keeps the notes whose title, body or tags contain it,
//!   ignoring case, `--kind` the notes of one kind, `--scope` those of one scope. Each filter
//!   combines with the others: the counts per kind (every kind, in the order the tool schemas list
//!   them) follow `--query` and `--scope`, the counts per scope follow `--query` and `--kind`, the
//!   total follows all three. Newest first unless `--sort title` (A to Z), `kind` (by kind name),
//!   `scope` (global notes first) or `place` (notes on this computer first), all ascending;
//!   `--order` turns any round, ties go by id. `--limit` is 1 to 200, 50 when not given. A note
//!   object has `id`, `kind`, `title`, `scope` (`repo` or `global`), `repo`
//!   (`github.com/o/r`, a `path:` key whole, `null` for a global note), `place` (`team` or
//!   `local`: where it is kept), `tags`, `entities`, `author` (`source` and `root`, shown like a
//!   script's writer), `createdAt` and `updatedAt` (RFC 3339), `revision` and `hits`.
//! - `harness-memory notes get <id> --json [--project PATH]` prints
//!   `{"note": {... "body"}, "links": [{"id", "title", "kind", "relation", "direction"}]}`: the
//!   note object with its body, and up to 50 notes linked through the graph: the notes linked to
//!   the entity at the other end of a relation that touches one of this note's entities
//!   (`direction` `out` when this note's entity is the relation's source, `in` when it is the
//!   target). Showing the note counts no hit (`note_get` does count one).
//! - `harness-memory notes delete <id> [--project PATH]` deletes the note as `note_delete` does
//!   and prints `deleted note <id> <title>`.
//! - `harness-memory notes delete --all --yes [--project PATH] [--json]` deletes every note
//!   `notes list` shows without filters (all pages) in one write per store, as `note_delete`
//!   deletes one, and prints `deleted N notes` (and how many came from the team database), or
//!   `{"deleted": N, "team": M}` with `--json` (`team`: how many were kept in the team database).
//!   Without `--yes` it deletes nothing and exits 2.
//!
//! A note that is not visible exits 1 with `harness-memory: note <id> not found` as the last line
//! of stderr. Other exit codes and errors as in [`crate::cli`].

use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::backend::Place;
use crate::backend::{Secrets, Selection};
use crate::cli::{self, Args, Delete, PAGE_OPTIONS, Page};
use crate::limits::NEIGHBORS_LIMIT_MAX;
use crate::mcp::{author_json, place_label, repo_label};
use crate::open::{notes_place, open_notes};
use crate::store::{Forget, MemoryStore, Note, NoteKind, NoteListQuery, NoteRow, NoteSort, Scope, StoreError};
use crate::store::{Target, View, clean};
use crate::support::time::rfc3339;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "notes list --json [--project PATH] [--kind KIND] [--scope repo|global] [--query TEXT] \
                         [--sort updated|title|kind|scope|place] [--order asc|desc] [--offset N] [--limit N] | \
                         notes get <id> --json [--project PATH] | notes delete <id> [--project PATH] | notes delete \
                         --all --yes [--project PATH] [--json]";

/// The most linked notes `notes get` lists.
const LINKS_MAX: usize = 50;

/// `harness-memory notes <args>`.
pub fn notes(args: &[Option<&str>]) -> ExitCode {
    match args {
        [Some("list"), rest @ ..] => list(rest),
        [Some("get"), rest @ ..] => get(rest),
        [Some("delete"), rest @ ..] => delete(rest),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    cli::usage(USAGE)
}

/// `notes list`.
fn list(args: &[Option<&str>]) -> ExitCode {
    let Some((project, query)) = parse_list(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(project.as_deref()) else {
        return ExitCode::FAILURE;
    };

    let query = NoteListQuery {
        view: opened.view.clone(),
        ..query
    };

    let page = match opened.store.list_notes(&query) {
        Ok(page) => page,
        Err(error) => return opened.failed(&error),
    };

    let kinds: Vec<Value> = page
        .kinds
        .iter()
        .map(|(kind, count)| json!({ "kind": kind.as_str(), "count": count }))
        .collect();
    let mut scopes = Map::new();

    for (scope, count) in &page.scopes {
        scopes.insert(scope.as_str().to_owned(), json!(count));
    }

    let notes: Vec<Value> = page.rows.iter().map(|row| opened.summary(row)).collect();

    println!(
        "{}",
        json!({ "total": page.total, "kinds": kinds, "scopes": scopes, "notes": notes })
    );
    ExitCode::SUCCESS
}

/// `notes get`.
fn get(args: &[Option<&str>]) -> ExitCode {
    let Some((id, project)) = parse_get(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(project.as_deref()) else {
        return ExitCode::FAILURE;
    };

    let note = match opened.note(id) {
        Ok(Some(note)) => note,
        Ok(None) => return not_found(id),
        Err(error) => return opened.failed(&error),
    };

    let links = match opened.links(&note) {
        Ok(links) => links,
        Err(error) => return opened.failed(&error),
    };

    let mut shown = opened.summary(&NoteRow::from(note.clone()));
    shown["body"] = Value::String(note.body);

    println!("{}", json!({ "note": shown, "links": links }));
    ExitCode::SUCCESS
}

/// `notes delete`.
fn delete(args: &[Option<&str>]) -> ExitCode {
    match Delete::parse(args) {
        Some(Delete::One { id, project }) => delete_one(id, project.as_deref()),
        Some(Delete::All { project, json }) => delete_all(project.as_deref(), json),
        Some(Delete::Unconfirmed) => cli::unconfirmed("note"),
        None => usage(),
    }
}

/// `notes delete <id>`.
fn delete_one(id: i64, project: Option<&str>) -> ExitCode {
    let Some(mut opened) = Opened::open(project) else {
        return ExitCode::FAILURE;
    };

    let note = match opened.note(id) {
        Ok(Some(note)) => note,
        Ok(None) => return not_found(id),
        Err(error) => return opened.failed(&error),
    };

    match opened.store.forget(&opened.view, &Target::Global, &Forget::Note(id)) {
        Ok(_) => {
            println!("deleted note {id} {}", clean::line(&note.title));
            ExitCode::SUCCESS
        }
        Err(StoreError::NotFound(_)) => not_found(id),
        Err(error) => opened.failed(&error),
    }
}

/// `notes delete --all --yes`.
fn delete_all(project: Option<&str>, json: bool) -> ExitCode {
    let Some(mut opened) = Opened::open(project) else {
        return ExitCode::FAILURE;
    };

    let forgotten = match opened.store.forget_notes(&opened.view) {
        Ok(forgotten) => forgotten,
        Err(error) => return opened.failed(&error),
    };

    let team: usize = [Scope::Repo, Scope::Global]
        .into_iter()
        .filter(|scope| notes_place(&opened.selection, *scope) == Place::Remote)
        .map(|scope| forgotten.of(scope))
        .sum();

    cli::deleted_all("note", forgotten.total(), team, json);
    ExitCode::SUCCESS
}

fn not_found(id: i64) -> ExitCode {
    eprintln!("harness-memory: note {id} not found");
    ExitCode::FAILURE
}

/// The options after `notes list`: `--project` and the query (its view still empty); `None`
/// without `--json`, for an unknown or repeated option, or a value that is not valid.
fn parse_list(args: &[Option<&str>]) -> Option<(Option<String>, NoteListQuery)> {
    let valued = [&PAGE_OPTIONS[..], &["--project", "--kind", "--scope", "--sort"]].concat();
    let parsed = Args::parse(args, &["--json"], &valued)?;

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return None;
    }

    let kind = match parsed.value("--kind") {
        Some(raw) => Some(NoteKind::parse(raw)?),
        None => None,
    };

    let scope = parsed.scope().ok()?;

    let sort = match parsed.value("--sort") {
        None | Some("updated") => NoteSort::Updated,
        Some("title") => NoteSort::Title,
        Some("kind") => NoteSort::Kind,
        Some("scope") => NoteSort::Scope,
        Some("place") => NoteSort::Place,
        Some(_) => return None,
    };

    let Page {
        offset,
        limit,
        query,
        descending,
    } = Page::of(&parsed)?;

    let query = NoteListQuery {
        view: View {
            repo: None,
            global: false,
        },
        text: query,
        kind,
        scope,
        entity: None,
        sort,
        descending: descending.unwrap_or(sort == NoteSort::Updated),
        offset,
        limit,
    };

    Some((parsed.project().ok()?, query))
}

/// The note id and `--project` after `notes get`, which needs `--json`; `None` for anything
/// else.
fn parse_get(args: &[Option<&str>]) -> Option<(i64, Option<String>)> {
    let parsed = Args::parse(args, &["--json"], &["--project"])?;

    let [id] = parsed.positional[..] else {
        return None;
    };

    if !parsed.flag("--json") {
        return None;
    }

    Some((id.parse().ok()?, parsed.project().ok()?))
}

/// The notes store and the view of the notes `--project` makes visible, opened like the server.
struct Opened {
    store: Box<dyn MemoryStore>,
    view: View,
    selection: Selection,
    secrets: Secrets,
}

impl Opened {
    /// Opens the store for the notes visible from `project` (only the global ones without it);
    /// `None`, with the reason printed, when the settings, the project or the store are wrong.
    fn open(project: Option<&str>) -> Option<Self> {
        let (env, selection) = cli::settings()?;

        let repo = match project {
            Some(path) => cli::repository(&env, Some(path))?,
            None => None,
        };

        let store = open_notes(&env, &selection)?;

        Some(Self {
            store,
            view: View { repo, global: true },
            secrets: selection.backend.secrets(),
            selection,
        })
    }

    /// Note `id` when it is visible (counting no hit).
    fn note(&mut self, id: i64) -> Result<Option<Note>, StoreError> {
        Ok(self.store.peek(&self.view, &[id])?.into_iter().next())
    }

    /// The note object of `row` (see the module docs).
    fn summary(&self, row: &NoteRow) -> Value {
        json!({
            "id": row.id,
            "kind": row.kind.as_str(),
            "title": row.title,
            "scope": row.scope.as_str(),
            "repo": row.project.as_deref().map(repo_label),
            "place": place_label(notes_place(&self.selection, row.scope)),
            "tags": row.tags,
            "entities": row.entities,
            "author": author_json(&row.provenance),
            "createdAt": rfc3339(row.created_at),
            "updatedAt": rfc3339(row.updated_at),
            "revision": row.revision,
            "hits": row.hits,
        })
    }

    /// The notes linked to `note` through the graph (see the module docs), at most
    /// [`LINKS_MAX`], each once per relation and direction. At most [`NEIGHBORS_LIMIT_MAX`]
    /// relations are followed.
    fn links(&mut self, note: &Note) -> Result<Vec<Value>, StoreError> {
        let mut links: Vec<(i64, String, &'static str)> = Vec::new();
        let mut shown: Vec<Value> = Vec::new();
        let mut followed = 0;

        for entity in &note.entities {
            let around = match self.store.neighbors(&self.view, entity, 1, NEIGHBORS_LIMIT_MAX) {
                Ok(around) => around,
                Err(StoreError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };

            let own = clean::normalize(entity);

            for edge in around.edges {
                let (direction, other) = if clean::normalize(&edge.from) == own {
                    ("out", edge.to)
                } else if clean::normalize(&edge.to) == own {
                    ("in", edge.from)
                } else {
                    continue;
                };

                if followed >= NEIGHBORS_LIMIT_MAX || shown.len() >= LINKS_MAX {
                    return Ok(shown);
                }

                followed += 1;

                for row in self.linked_to(&other, edge.scope)? {
                    let key = (row.id, edge.relation.clone(), direction);

                    if row.id == note.id || links.contains(&key) || shown.len() >= LINKS_MAX {
                        continue;
                    }

                    links.push(key);
                    shown.push(json!({
                        "id": row.id,
                        "title": row.title,
                        "kind": row.kind.as_str(),
                        "relation": edge.relation,
                        "direction": direction,
                    }));
                }
            }
        }

        Ok(shown)
    }

    /// The newest notes linked to the entity `name` of `scope` (relations join entities of one
    /// scope), at most [`LINKS_MAX`].
    fn linked_to(&mut self, name: &str, scope: Scope) -> Result<Vec<NoteRow>, StoreError> {
        let view = match scope {
            Scope::Repo => View {
                repo: self.view.repo.clone(),
                global: false,
            },
            Scope::Global => View {
                repo: None,
                global: true,
            },
        };

        let page = self.store.list_notes(&NoteListQuery {
            view,
            text: String::new(),
            kind: None,
            scope: None,
            entity: Some(name.to_owned()),
            sort: NoteSort::Updated,
            descending: true,
            offset: 0,
            limit: LINKS_MAX,
        })?;

        Ok(page.rows)
    }

    /// Prints `error` without the database credentials; exit 1.
    fn failed(&self, error: &StoreError) -> ExitCode {
        eprintln!("harness-memory: {}", self.secrets.scrub(&error.to_string()));
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn list_options_default_to_the_newest_page_and_check_their_values() {
        let (project, query) = parse_list(&args(&["--json"])).expect("valid");
        assert_eq!(project, None);
        assert_eq!(
            (query.sort, query.descending, query.offset, query.limit, query.kind),
            (NoteSort::Updated, true, 0, 50, None)
        );

        let raw = args(&[
            "--json",
            "--sort",
            "title",
            "--kind",
            "Optimization",
            "--project=/w",
            "--offset",
            "3",
        ]);
        let (project, query) = parse_list(&raw).expect("valid");
        assert_eq!(project.as_deref(), Some("/w"));
        assert_eq!(
            (query.sort, query.descending, query.offset, query.kind),
            (NoteSort::Title, false, 3, Some(NoteKind::Optimisation))
        );

        assert_eq!(query.scope, None);

        for (raw, scope) in [("repo", Scope::Repo), ("global", Scope::Global)] {
            let (_, query) = parse_list(&args(&["--json", "--scope", raw])).expect("valid");
            assert_eq!(query.scope, Some(scope), "{raw}");
        }

        for (raw, sort) in [
            ("kind", NoteSort::Kind),
            ("scope", NoteSort::Scope),
            ("place", NoteSort::Place),
        ] {
            let (_, query) = parse_list(&args(&["--json", "--sort", raw])).expect("valid");
            assert_eq!((query.sort, query.descending), (sort, false), "{raw}");
        }

        for bad in [
            &[][..],
            &["--json", "extra"],
            &["--json", "--kind", "idea"],
            &["--json", "--sort", "hits"],
            &["--json", "--limit", "201"],
            &["--json", "--project", ""],
            &["--json", "--scope", "team"],
            &["--json", "--scope", "repo", "--scope", "repo"],
        ] {
            assert!(parse_list(&args(bad)).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn get_needs_json_and_one_id() {
        assert_eq!(parse_get(&args(&["7", "--json"])), Some((7, None)));
        assert_eq!(
            parse_get(&args(&["7", "--json", "--project", "/w"])),
            Some((7, Some("/w".to_owned())))
        );

        assert!(parse_get(&args(&["7"])).is_none());
        assert!(parse_get(&args(&["7", "8", "--json"])).is_none());
        assert!(parse_get(&args(&["x", "--json"])).is_none());
    }
}
