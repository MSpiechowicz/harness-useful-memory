//! The note commands of the `harness-memory` executable, for the person at the terminal (and
//! Harness Desktop on their behalf), never for an agent's MCP calls. Each sees the notes
//! `note_search` would with `project_path` set to `--project`: that repository's notes and the
//! global ones, only the global ones without `--project`. The store is the one the MCP server
//! opens (the local file, the team database, or both as the routing says), so the team notes
//! hidden on this computer are left out as they are for the agent.
//!
//! - `harness-memory notes list --json [--project PATH | --repo KEY | --all-repos] [--kind KIND] [--scope repo|global]
//!   [--place local|team] [--query TEXT] [--sort updated|title|kind|scope|place] [--order asc|desc]
//!   [--offset N] [--limit N]` prints `{"total": N, "kinds": [{"kind", "count"}], "scopes":
//!   {"repo": N, "global": N}, "places": {"local": N, "team": N}, "notes": [...]}`: one page of the
//!   notes (see [`crate::store::listing`] for the matching and the order). `--query` keeps the
//!   notes whose title, body or tags contain it, ignoring case, `--kind` the notes of one kind,
//!   `--scope` those of one scope, `--place` those kept in one place (`local`: this computer's
//!   file, `team`: the team database). Each filter combines with the others: the counts per kind
//!   (every kind, in the order the tool schemas list them) follow every filter but `--kind`, the
//!   counts per scope every filter but `--scope`, the counts per place every filter but
//!   `--place`, the total all four. Newest first unless `--sort title` (A to Z), `kind` (by kind
//!   name), `scope` (global notes first) or `place` (notes on this computer first), all ascending;
//!   `--order` turns any round, ties go by id. `--limit` is 1 to 200, 50 when not given. A note
//!   object has `id`, `kind`, `title`, `scope` (`repo` or `global`), `repo`
//!   (`github.com/o/r`, a `path:` key whole, `null` for a global note), `place` (`team` or
//!   `local`: where this note is kept), `readOnly` (a team note of a scope routed
//!   `remote+local`, which cannot be changed from here), `hidden` (hidden on this computer),
//!   `tags`, `entities`, `author` (`source` and `root`, shown like a script's writer),
//!   `createdAt` and `updatedAt` (RFC 3339), `revision` and `hits`. With a team database the
//!   object also has `hiddenCount` (the team notes of the view hidden on this computer) and
//!   `localCounts` (`{"repo": N, "global": N}`: the notes in the local file per scope, every
//!   repository's, 0 without the file). `--hidden` lists only the hidden team notes of the view
//!   instead (the filters, the order and the paging apply to them, `total` counts them); a hidden
//!   note the team database no longer has is dropped from the hidden list.
//! - `harness-memory notes get <id> --json [--project PATH | --repo KEY | --all-repos] [--hidden]` prints
//!   `{"note": {... "body"}, "links": [{"id", "title", "kind", "relation", "direction"}]}`: the
//!   note object with its body, and up to 50 notes linked through the graph: the notes linked to
//!   the entity at the other end of a relation that touches one of this note's entities
//!   (`direction` `out` when this note's entity is the relation's source, `in` when it is the
//!   target). Showing the note counts no hit (`note_get` does count one). A hidden team note is
//!   not found unless `--hidden` is given.
//! - `harness-memory notes hide <id> [--project PATH | --repo KEY | --all-repos] [--json]` hides a team note on this computer
//!   (the team database is not changed) and prints `hid note <id> <title>`, or `{"hidden": <id>}`;
//!   `notes unhide <id> [--project PATH | --repo KEY | --all-repos] [--json]` shows it again (`unhid note <id>`, or
//!   `{"unhidden": <id>}`). A note saved on this computer is refused (delete it instead), and
//!   without a team database read for notes there is nothing to hide (exit 1).
//! - `harness-memory notes delete <id> [--project PATH | --repo KEY | --all-repos]` deletes the note as `note_delete` does
//!   and prints `deleted note <id> <title>`.
//! - `harness-memory notes delete --all --yes [--project PATH | --repo KEY] [--json]` deletes every note
//!   `notes list` shows without filters (all pages) in one write per store, as `note_delete`
//!   deletes one, and prints `deleted N notes` (and how many came from the team database), or
//!   `{"deleted": N, "team": M}` with `--json` (`team`: how many were kept in the team database).
//!   In a scope routed `remote+local` only the notes on this computer go: the team notes stay,
//!   counted as `kept` (`{"deleted": N, "team": M, "kept": K}`). Without `--yes` it deletes
//!   nothing and exits 2.
//! - `harness-memory notes discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes
//!   [--json]` deletes notes from the local file in the scopes routed `remote`, where they are not
//!   read (every repository's, see [`discard`]), and prints `deleted N notes from this computer`,
//!   or `{"deleted": N}`.
//!
//! A note that is not visible exits 1 with `harness-memory: note <id> not found` as the last line
//! of stderr. Other exit codes and errors as in [`crate::cli`].

use std::collections::BTreeMap;
use std::process::ExitCode;

use serde_json::{Map, Value, json};

mod discard;
mod hidden;

use crate::backend::{Backend, NotesPlace, Place, Secrets, Selection};
use crate::cli::{self, Args, Delete, PAGE_OPTIONS, PICK_VALUED, Page, Repos};
use crate::limits::{LIST_PAGE_MAX, NEIGHBORS_LIMIT_MAX};
use crate::mcp::{author_json, place_label, repo_label};
use crate::open::{lone_notes_place, notes_place, open_hidden, open_notes};
use crate::store::{Forget, HiddenNotes, MemoryStore, Note, NoteKind, NoteListQuery, NoteOrigin, NoteRow, NoteSort};
use crate::store::{ProjectKey, Scope, SqliteStore, StoreError, Target, View, clean};
use crate::support::Env;
use crate::support::time::rfc3339;

/// The usage of these commands, for the executable's usage line.
pub const USAGE: &str = "notes list --json [--project PATH | --repo KEY | --all-repos] [--kind KIND] [--scope repo|global] \
                         [--place local|team] [--query TEXT] [--sort updated|title|kind|scope|place] [--order asc|desc] [--offset N] [--limit N] \
                         [--hidden] | notes repos --json | notes repos --json | notes get <id> --json [--project PATH | --repo KEY | --all-repos] [--hidden] | notes hide <id> \
                         [--project PATH | --repo KEY | --all-repos] [--json] | notes unhide <id> [--project PATH | --repo KEY | --all-repos] [--json] | notes delete \
                         <id> [--project PATH | --repo KEY | --all-repos] | notes delete --all --yes [--project PATH | --repo KEY] [--json] | notes \
                         discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes [--json]";

/// The most linked notes `notes get` lists.
const LINKS_MAX: usize = 50;

/// `harness-memory notes <args>`.
pub fn notes(args: &[Option<&str>]) -> ExitCode {
    match args {
        [Some("list"), rest @ ..] => list(rest),
        [Some("get"), rest @ ..] => get(rest),
        [Some("repos"), rest @ ..] => repos(rest),
        [Some("delete"), rest @ ..] => delete(rest),
        [Some("hide"), rest @ ..] => hidden::hide(rest),
        [Some("unhide"), rest @ ..] => hidden::unhide(rest),
        [Some("discard-local"), rest @ ..] => discard::discard_local(rest),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    cli::usage(USAGE)
}

/// `notes list`.
fn list(args: &[Option<&str>]) -> ExitCode {
    let Some((pick, query, show_hidden)) = parse_list(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(&pick) else {
        return ExitCode::FAILURE;
    };

    let query = NoteListQuery {
        view: opened.view.clone(),
        ..query
    };

    // Every hidden note is a team note, and a lone store keeps every note in one place.
    let page = if show_hidden {
        hidden::page(&mut opened, &query).map(|page| page.kept_in(Place::Remote, &query))
    } else {
        let lone = lone_notes_place(&opened.selection);
        opened.store.list_notes(&query).map(|page| match lone {
            Some(place) => page.kept_in(place, &query),
            None => page,
        })
    };

    let page = match page {
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

    let mut places = Map::new();

    for (place, count) in &page.places {
        places.insert(place_label(*place).to_owned(), json!(count));
    }

    let notes: Vec<Value> = page.rows.iter().map(|row| opened.summary(row, show_hidden)).collect();
    let mut shown = json!({
        "total": page.total,
        "kinds": kinds,
        "scopes": scopes,
        "places": places,
        "notes": notes,
    });

    if let Err(error) = opened.add_team_counts(&mut shown) {
        return opened.failed(&error);
    }

    println!("{shown}");
    ExitCode::SUCCESS
}

/// `notes repos --json`: the repositories that have notes, each with its key, the label notes
/// show it by and how many notes it has, by label, and how many global notes there are.
fn repos(args: &[Option<&str>]) -> ExitCode {
    match Args::parse(args, &["--json"], &[]) {
        Some(parsed) if parsed.flag("--json") && parsed.positional.is_empty() => {}
        _ => return usage(),
    }

    let Some(mut opened) = Opened::open(&Repos::Every) else {
        return ExitCode::FAILURE;
    };

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut global = 0_usize;
    let mut offset = 0;

    loop {
        let page = opened.store.list_notes(&NoteListQuery {
            view: opened.view.clone(),
            text: String::new(),
            kind: None,
            scope: None,
            place: None,
            entity: None,
            sort: NoteSort::Updated,
            descending: true,
            offset,
            limit: LIST_PAGE_MAX,
            exclude: Vec::new(),
        });

        let page = match page {
            Ok(page) => page,
            Err(error) => return opened.failed(&error),
        };

        for row in &page.rows {
            match row.project.as_deref() {
                Some(key) => *counts.entry(key.to_owned()).or_default() += 1,
                None => global += 1,
            }
        }

        offset += page.rows.len();

        if page.rows.is_empty() || offset >= page.total {
            break;
        }
    }

    println!("{}", json!({ "repos": cli::repos_json(counts), "global": global }));
    ExitCode::SUCCESS
}

/// `notes get`.
fn get(args: &[Option<&str>]) -> ExitCode {
    let Some((id, pick, show_hidden)) = parse_get(args) else {
        return usage();
    };

    let Some(mut opened) = Opened::open(&pick) else {
        return ExitCode::FAILURE;
    };

    let found = match opened.note(id) {
        Ok(Some(note)) => Ok(Some((note, false))),
        Ok(None) if show_hidden => hidden::note(&mut opened, id).map(|note| note.map(|note| (note, true))),
        Ok(None) => Ok(None),
        Err(error) => Err(error),
    };

    let (note, is_hidden) = match found {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(id),
        Err(error) => return opened.failed(&error),
    };

    // Links are looked up where the note lives: the repository of a note read among every repository.
    if opened.view.sees_every_repo() {
        opened.view = View {
            repo: note.project.as_ref().map(|key| ProjectKey {
                key: key.clone(),
                root: std::path::PathBuf::new(),
                remote: None,
            }),
            global: true,
        };
    }

    let links = match opened.links(&note) {
        Ok(links) => links,
        Err(error) => return opened.failed(&error),
    };

    let mut shown = opened.summary(&NoteRow::from(note.clone()), is_hidden);
    shown["body"] = Value::String(note.body);

    println!("{}", json!({ "note": shown, "links": links }));
    ExitCode::SUCCESS
}

/// `notes delete`.
fn delete(args: &[Option<&str>]) -> ExitCode {
    match Delete::parse(args) {
        Some(Delete::One { id, pick }) => delete_one(id, &pick),
        Some(Delete::All { pick, scope, json }) => delete_all(&pick, scope, json),
        Some(Delete::Unconfirmed) => cli::unconfirmed("note"),
        None => usage(),
    }
}

/// `notes delete <id>`.
fn delete_one(id: i64, pick: &Repos) -> ExitCode {
    let Some(mut opened) = Opened::open(pick) else {
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
fn delete_all(pick: &Repos, scope: Option<Scope>, json: bool) -> ExitCode {
    let Some(mut opened) = Opened::open(pick) else {
        return ExitCode::FAILURE;
    };

    opened.view = cli::narrowed(opened.view, scope);

    let forgotten = match opened.store.forget_notes(&opened.view, &[]) {
        Ok(forgotten) => forgotten,
        Err(error) => return opened.failed(&error),
    };

    let team: usize = [Scope::Repo, Scope::Global]
        .into_iter()
        .filter(|scope| notes_place(&opened.selection, *scope).writes() == Place::Remote)
        .map(|scope| forgotten.of(scope))
        .sum();

    let kept = match opened.kept_team_notes() {
        Ok(kept) => kept,
        Err(error) => return opened.failed(&error),
    };

    cli::deleted_all_keeping("note", (forgotten.total(), team), kept, json);
    ExitCode::SUCCESS
}

fn not_found(id: i64) -> ExitCode {
    eprintln!("harness-memory: note {id} not found");
    ExitCode::FAILURE
}

/// The notes in the local file per scope, every repository's: `(repository, global)`; none
/// without the file (which is not created).
fn local_counts(env: &Env) -> Result<(usize, usize), StoreError> {
    let Some(path) = crate::database_path(env).filter(|path| path.is_file()) else {
        return Ok((0, 0));
    };

    SqliteStore::open(&path)?.scope_counts()
}

/// The options after `notes list`: `--project`, the query (its view still empty) and
/// `--hidden`; `None` without `--json`, for an unknown or repeated option, or a value that is not
/// valid.
fn parse_list(args: &[Option<&str>]) -> Option<(Repos, NoteListQuery, bool)> {
    let valued = [
        &PAGE_OPTIONS[..],
        &PICK_VALUED[..],
        &["--kind", "--scope", "--place", "--sort"],
    ]
    .concat();
    let parsed = Args::parse(args, &["--json", "--hidden", "--all-repos"], &valued)?;

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
        place: parsed.place().ok()?,
        entity: None,
        sort,
        descending: descending.unwrap_or(sort == NoteSort::Updated),
        offset,
        limit,
        exclude: Vec::new(),
    };

    Some((parsed.repos().ok()?, query, parsed.flag("--hidden")))
}

/// The note id, `--project` and `--hidden` after `notes get`, which needs `--json`; `None` for
/// anything else.
fn parse_get(args: &[Option<&str>]) -> Option<(i64, Repos, bool)> {
    let parsed = Args::parse(args, &["--json", "--hidden", "--all-repos"], &PICK_VALUED)?;

    let [id] = parsed.positional[..] else {
        return None;
    };

    if !parsed.flag("--json") {
        return None;
    }

    Some((id.parse().ok()?, parsed.repos().ok()?, parsed.flag("--hidden")))
}

/// The notes store and the view of the notes `--project` makes visible, opened like the server,
/// with the team notes hidden on this computer (`None` when no scope reads the team database).
struct Opened {
    store: Box<dyn MemoryStore>,
    hidden: Option<HiddenNotes>,
    view: View,
    env: Env,
    selection: Selection,
    secrets: Secrets,
}

impl Opened {
    /// Opens the store for the notes `pick` makes visible (only the global ones without any);
    /// `None`, with the reason printed, when the settings, the project or the store are wrong.
    fn open(pick: &Repos) -> Option<Self> {
        let (env, selection) = cli::settings()?;

        let view = match pick {
            Repos::Project(None) => View {
                repo: None,
                global: true,
            },
            pick => pick.view(&env)?,
        };

        let store = open_notes(&env, &selection)?;

        Some(Self {
            store,
            hidden: open_hidden(&env, &selection),
            view,
            secrets: selection.backend.secrets(),
            env,
            selection,
        })
    }

    /// Adds `hiddenCount` and `localCounts` to the list `shown` when a team database is set.
    fn add_team_counts(&mut self, shown: &mut Value) -> Result<(), StoreError> {
        if self.selection.backend == Backend::Local {
            return Ok(());
        }

        let (repo, global) = local_counts(&self.env)?;
        shown["hiddenCount"] = json!(hidden::count(self)?);
        shown["localCounts"] = json!({ "repo": repo, "global": global });
        Ok(())
    }

    /// How many team notes of the view stay in the scopes routed `remote+local` (where deleting
    /// every note only deletes those on this computer); `None` when no scope is.
    fn kept_team_notes(&mut self) -> Result<Option<usize>, StoreError> {
        let shared = |scope| notes_place(&self.selection, scope) == NotesPlace::RemoteLocal;
        let view = View {
            repo: self.view.repo.clone().filter(|_| shared(Scope::Repo)),
            global: self.view.global && shared(Scope::Global),
        };

        if view.repo.is_none() && !view.global {
            return Ok(None);
        }

        let page = self.store.list_notes(&NoteListQuery {
            view,
            text: String::new(),
            kind: None,
            scope: None,
            place: None,
            entity: None,
            sort: NoteSort::Updated,
            descending: true,
            offset: 0,
            limit: 1,
            exclude: Vec::new(),
        })?;

        Ok(Some(page.total))
    }

    /// Note `id` when it is visible (counting no hit).
    fn note(&mut self, id: i64) -> Result<Option<Note>, StoreError> {
        Ok(self.store.peek(&self.view, &[id])?.into_iter().next())
    }

    /// The note object of `row`, hidden on this computer when `hidden` (see the module docs).
    fn summary(&self, row: &NoteRow, hidden: bool) -> Value {
        let (place, read_only) = match self.store.origin(row.id, row.scope) {
            NoteOrigin::Single => (place_label(notes_place(&self.selection, row.scope).writes()), false),
            NoteOrigin::Team { read_only } => (place_label(Place::Remote), read_only),
            NoteOrigin::Local => (place_label(Place::Local), false),
        };

        json!({
            "id": row.id,
            "kind": row.kind.as_str(),
            "title": row.title,
            "scope": row.scope.as_str(),
            "repo": row.project.as_deref().map(repo_label),
            "repoKey": row.project,
            "place": place,
            "readOnly": read_only,
            "hidden": hidden,
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
            place: None,
            entity: Some(name.to_owned()),
            sort: NoteSort::Updated,
            descending: true,
            offset: 0,
            limit: LINKS_MAX,
            exclude: Vec::new(),
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
        let (project, query, hidden) = parse_list(&args(&["--json"])).expect("valid");
        assert_eq!((project, hidden), (Repos::Project(None), false));
        assert!(parse_list(&args(&["--json", "--hidden"])).expect("valid").2);
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
        let (project, query, _) = parse_list(&raw).expect("valid");
        assert_eq!(project, Repos::Project(Some("/w".to_owned())));
        assert_eq!(
            (query.sort, query.descending, query.offset, query.kind),
            (NoteSort::Title, false, 3, Some(NoteKind::Optimisation))
        );

        assert_eq!(query.scope, None);

        for (raw, scope) in [("repo", Scope::Repo), ("global", Scope::Global)] {
            let (_, query, _) = parse_list(&args(&["--json", "--scope", raw])).expect("valid");
            assert_eq!(query.scope, Some(scope), "{raw}");
        }

        for (raw, sort) in [
            ("kind", NoteSort::Kind),
            ("scope", NoteSort::Scope),
            ("place", NoteSort::Place),
        ] {
            let (_, query, _) = parse_list(&args(&["--json", "--sort", raw])).expect("valid");
            assert_eq!((query.sort, query.descending), (sort, false), "{raw}");
        }

        assert_eq!(query.place, None);

        for (raw, place) in [("local", Place::Local), ("team", Place::Remote)] {
            let (_, query, _) = parse_list(&args(&["--json", "--place", raw])).expect("valid");
            assert_eq!(query.place, Some(place), "{raw}");
        }

        let (_, query, hidden) =
            parse_list(&args(&["--json", "--hidden", "--place=team", "--scope", "repo"])).expect("valid");
        assert_eq!(
            (query.place, query.scope, hidden),
            (Some(Place::Remote), Some(Scope::Repo), true)
        );

        for bad in [
            &[][..],
            &["--json", "extra"],
            &["--json", "--kind", "idea"],
            &["--json", "--sort", "hits"],
            &["--json", "--limit", "201"],
            &["--json", "--project", ""],
            &["--json", "--scope", "team"],
            &["--json", "--scope", "repo", "--scope", "repo"],
            &["--json", "--place", "remote"],
            &["--json", "--place", "Team"],
            &["--json", "--place", ""],
            &["--json", "--place"],
            &["--json", "--place", "local", "--place", "team"],
            &["--json", "--place", "team", "--place", "team"],
        ] {
            assert!(parse_list(&args(bad)).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn get_needs_json_and_one_id() {
        assert_eq!(
            parse_get(&args(&["7", "--json"])),
            Some((7, Repos::Project(None), false))
        );
        assert_eq!(
            parse_get(&args(&["7", "--json", "--project", "/w"])),
            Some((7, Repos::Project(Some("/w".to_owned())), false))
        );
        assert_eq!(
            parse_get(&args(&["--hidden", "7", "--json"])),
            Some((7, Repos::Project(None), true))
        );

        assert!(parse_get(&args(&["7"])).is_none());
        assert!(parse_get(&args(&["7", "8", "--json"])).is_none());
        assert!(parse_get(&args(&["x", "--json"])).is_none());
    }
}
