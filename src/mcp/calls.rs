//! `tools/call`: argument parsing, scope resolution and the compact text replies.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Server;
use super::blocks::{FRAME, hit_block, note_block, saved_line, similar_line};
use super::reply::{Capped, now};
use crate::limits::{
    DEPTH_MAX, GET_REPLY_MAX, LIST_REPLY_MAX, NEIGHBORS_LIMIT_DEFAULT, SEARCH_LIMIT_DEFAULT, SIMILAR_MAX,
};
use crate::store::{
    Forget, Link, NoteDraft, NoteKind, NoteOrigin, NotePatch, Provenance, SearchQuery, Target, View, clean,
};

/// Sent when a repository-scope call has no repository.
pub(super) const NO_PROJECT: &str = "cannot tell which repository this is (the server runs outside one, or in the home folder \
                          or a filesystem root): pass project_path with the absolute repository path, or use \
                          scope \"global\"";

/// Sent when `same_checkout` is asked for outside a checkout.
const NO_CHECKOUT: &str = "same_checkout needs the server to run inside a repository checkout";

/// A tool result.
pub(super) struct Reply {
    pub text: String,
    pub is_error: bool,
    /// Sent as `structuredContent` next to the text, when set.
    pub structured: Option<Value>,
}

/// Runs tool `name`; `None` when there is no such tool.
pub(super) fn call(server: &mut Server, name: &str, arguments: Value) -> Option<Reply> {
    let result = match name {
        "note_save" => parse(arguments).and_then(|args| remember(server, args)),
        "note_update" => parse(arguments).and_then(|args| update(server, args)),
        "note_search" => parse(arguments).and_then(|args| search(server, args)),
        "note_get" => parse(arguments).and_then(|args| get(server, args)),
        "note_delete" => parse(arguments).and_then(|args| delete_note(server, args)),
        "entity_link" => parse(arguments).and_then(|args| link(server, args)),
        "entity_neighbors" => parse(arguments).and_then(|args| neighbors(server, args)),
        "entity_delete" => parse(arguments).and_then(|args| delete_entity(server, args)),
        _ => return super::calls_scripts::call(server, name, arguments),
    };

    Some(match result {
        Ok(text) => Reply {
            text,
            is_error: false,
            structured: None,
        },
        Err(text) => Reply {
            text,
            is_error: true,
            structured: None,
        },
    })
}

#[derive(Deserialize)]
struct RememberArgs {
    title: String,
    body: String,
    kind: String,
    tags: Option<Vec<String>>,
    entities: Option<Vec<String>>,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
struct UpdateArgs {
    id: i64,
    title: Option<String>,
    body: Option<String>,
    append: Option<String>,
    kind: Option<String>,
    tags: Option<Vec<String>>,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: Option<String>,
    kind: Option<String>,
    limit: Option<i64>,
    scope: Option<String>,
    project_path: Option<String>,
    same_checkout: Option<bool>,
}

#[derive(Deserialize)]
struct GetArgs {
    ids: Vec<i64>,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
struct LinkArgs {
    from: String,
    relation: String,
    to: String,
    note_id: Option<i64>,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
struct NeighborsArgs {
    entity: String,
    depth: Option<i64>,
    limit: Option<i64>,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteDeleteArgs {
    id: i64,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityDeleteArgs {
    entity: Option<String>,
    from: Option<String>,
    relation: Option<String>,
    to: Option<String>,
    scope: Option<String>,
    project_path: Option<String>,
}

fn remember(server: &mut Server, args: RememberArgs) -> Result<String, String> {
    let target = target(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let draft = NoteDraft {
        title: args.title,
        body: args.body,
        kind: kind(&args.kind)?,
        tags: args.tags.unwrap_or_default(),
        entities: args.entities.unwrap_or_default(),
        provenance: provenance(server, args.project_path.as_deref()),
    };

    let saved = server
        .store
        .remember(&target, &draft)
        .map_err(|error| error.to_string())?;
    let similar = server
        .store
        .similar(&View::around(&target), saved.id, SIMILAR_MAX)
        .map_err(|error| error.to_string())?;

    if similar.is_empty() {
        return Ok(saved_line(&saved));
    }

    let now = now();
    let home = server.projects.home();
    let mut text = format!(
        "{FRAME}\n{}\nsimilar notes (update or delete them if outdated or duplicate):",
        saved_line(&saved)
    );

    for hit in &similar {
        text.push('\n');
        text.push_str(&similar_line(hit, now, &home));

        if server.store.origin(hit.id, hit.scope) == (NoteOrigin::Team { read_only: true }) {
            text.push_str(" (team, read-only)");
        }
    }

    Ok(text)
}

fn update(server: &mut Server, args: UpdateArgs) -> Result<String, String> {
    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let patch = NotePatch {
        title: args.title,
        body: args.body,
        append: args.append,
        kind: args.kind.as_deref().map(kind).transpose()?,
        tags: args.tags,
        provenance: provenance(server, args.project_path.as_deref()),
    };

    let saved = server
        .store
        .update(&view, args.id, &patch)
        .map_err(|error| error.to_string())?;
    Ok(saved_line(&saved))
}

fn search(server: &mut Server, args: SearchArgs) -> Result<String, String> {
    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let limit = args
        .limit
        .map_or(SEARCH_LIMIT_DEFAULT, |limit| usize::try_from(limit.max(1)).unwrap_or(1));
    let checkout = if args.same_checkout.unwrap_or(false) {
        Some(own_root(server)?)
    } else {
        None
    };

    let query = SearchQuery {
        text: args.query.unwrap_or_default(),
        kind: args.kind.as_deref().map(kind).transpose()?,
        view,
        limit,
        checkout,
        exclude: Vec::new(),
    };

    let hits = server.store.search(&query).map_err(|error| error.to_string())?;
    let scripts = super::search_scripts::block(server, &query.view, &query.text);
    let home = server.projects.home();

    // The saved scripts always fit: the notes get the room they leave.
    let room = scripts.as_ref().map_or(0, |block| block.len() + 1);
    let mut reply = Capped::new(LIST_REPLY_MAX.saturating_sub(room));

    if !hits.is_empty() {
        reply.push(FRAME);
    }

    if query.view.repo.is_none() && query.view.global && scope_arg(args.scope.as_deref(), "both")? == "both" {
        reply.push("(repository unknown: global notes only; pass project_path for this repository's notes)");
    }

    if hits.is_empty() {
        reply.push("no matching notes");
    }

    for hit in &hits {
        reply.push(&hit_block(hit, &home));
    }

    let mut text = reply.finish();

    if let Some(block) = scripts {
        text.push('\n');
        text.push_str(&block);
    }

    Ok(text)
}

fn get(server: &mut Server, args: GetArgs) -> Result<String, String> {
    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let notes = server.store.get(&view, &args.ids).map_err(|error| error.to_string())?;
    let now = now();
    let home = server.projects.home();
    let mut reply = Capped::new(GET_REPLY_MAX);

    let missing: Vec<String> = args
        .ids
        .iter()
        .filter(|id| !notes.iter().any(|note| note.id == **id))
        .map(i64::to_string)
        .collect();

    if notes.is_empty() {
        return Err(format!("not found: {}", missing.join(", ")));
    }

    reply.push(FRAME);

    if !missing.is_empty() {
        reply.push(&format!("not found: {}", missing.join(", ")));
    }

    for (index, note) in notes.iter().enumerate() {
        let block = note_block(note, now, &home);
        let block = if index == 0 { block } else { format!("\n{block}") };
        reply.push_cut(&block);
    }

    Ok(reply.finish())
}

fn link(server: &mut Server, args: LinkArgs) -> Result<String, String> {
    let target = target(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let link = Link {
        from: args.from,
        relation: args.relation,
        to: args.to,
        note_id: args.note_id,
    };

    let edge = server.store.link(&target, &link).map_err(|error| error.to_string())?;
    let note = edge.note_id.map(|id| format!(", note #{id}")).unwrap_or_default();

    Ok(format!(
        "linked {} {} {} ({}{note})",
        clean::line(&edge.from),
        clean::line(&edge.relation),
        clean::line(&edge.to),
        edge.scope.as_str()
    ))
}

fn neighbors(server: &mut Server, args: NeighborsArgs) -> Result<String, String> {
    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let depth = u8::try_from(args.depth.unwrap_or(1).clamp(1, i64::from(DEPTH_MAX))).unwrap_or(1);
    let limit = args.limit.map_or(NEIGHBORS_LIMIT_DEFAULT, |limit| {
        usize::try_from(limit.max(1)).unwrap_or(1)
    });

    let around = server
        .store
        .neighbors(&view, &args.entity, depth, limit)
        .map_err(|error| error.to_string())?;

    let mut reply = Capped::new(LIST_REPLY_MAX);

    if !around.edges.is_empty() || !around.notes.is_empty() {
        reply.push(FRAME);
    }

    if around.edges.is_empty() {
        reply.push("no relations");
    }

    for edge in &around.edges {
        let note = edge.note_id.map(|id| format!(" (note #{id})")).unwrap_or_default();
        reply.push(&format!(
            "- {} {} {} [{}]{note}",
            clean::line(&edge.from),
            clean::line(&edge.relation),
            clean::line(&edge.to),
            edge.scope.as_str()
        ));
    }

    if around.more_edges > 0 {
        reply.push(&format!("- … {} more relations (raise limit)", around.more_edges));
    }

    if !around.notes.is_empty() {
        reply.push("notes:");
    }

    for (id, title) in &around.notes {
        reply.push(&format!("- #{id} {}", clean::line(title)));
    }

    Ok(reply.finish())
}

fn delete_note(server: &mut Server, args: NoteDeleteArgs) -> Result<String, String> {
    // A note is looked up in this repository and the global scope unless a scope is named.
    let scope = args.scope.as_deref().unwrap_or("both");
    let view = view(server, Some(scope), args.project_path.as_deref())?;

    forget_with(server, &view, &Target::Global, &Forget::Note(args.id))?;
    Ok(format!("forgot note {}", args.id))
}

fn delete_entity(server: &mut Server, args: EntityDeleteArgs) -> Result<String, String> {
    let relation_form = args.from.is_some() || args.relation.is_some() || args.to.is_some();
    let forms = usize::from(args.entity.is_some()) + usize::from(relation_form);

    if forms != 1 {
        return Err("give exactly one of: entity | from + relation + to".to_owned());
    }

    let target = target(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let view = View::around(&target);

    if let Some(name) = args.entity {
        let deleted = forget_with(server, &view, &target, &Forget::Entity(name.clone()))?;
        return Ok(format!(
            "forgot entity {} and {} relations",
            clean::line(&name),
            deleted.saturating_sub(1)
        ));
    }

    let (Some(from), Some(relation), Some(to)) = (args.from, args.relation, args.to) else {
        return Err("a relation needs from, relation and to".to_owned());
    };

    let what = Forget::Relation {
        from: from.clone(),
        relation: relation.clone(),
        to: to.clone(),
    };
    forget_with(server, &view, &target, &what)?;
    Ok(format!(
        "forgot relation {} {} {}",
        clean::line(&from),
        clean::line(&relation),
        clean::line(&to)
    ))
}

fn forget_with(server: &mut Server, view: &View, target: &Target, what: &Forget) -> Result<usize, String> {
    server
        .store
        .forget(view, target, what)
        .map_err(|error| error.to_string())
}

/// Parses tool arguments; serde's message names the offending field.
pub(super) fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

fn kind(raw: &str) -> Result<NoteKind, String> {
    NoteKind::parse(raw).ok_or_else(|| {
        let kinds: Vec<&str> = NoteKind::ALL.iter().map(|kind| kind.as_str()).collect();
        format!("kind must be one of {}", kinds.join(", "))
    })
}

/// `scope` or its default, checked against `repo`, `global`, `both`.
fn scope_arg<'a>(raw: Option<&'a str>, default: &'a str) -> Result<&'a str, String> {
    match raw.map(str::trim).filter(|raw| !raw.is_empty()).unwrap_or(default) {
        scope @ ("repo" | "global" | "both") => Ok(scope),
        _ => Err("scope must be repo, global or both".to_owned()),
    }
}

/// The scope a write goes to (default: this repository).
pub(super) fn target(server: &mut Server, scope: Option<&str>, project_path: Option<&str>) -> Result<Target, String> {
    match scope_arg(scope, "repo")? {
        "global" => Ok(Target::Global),
        "repo" => match server.projects.resolve(project_path)? {
            Some(key) => Ok(Target::Repo(key)),
            None => Err(NO_PROJECT.to_owned()),
        },
        _ => Err("scope must be repo or global here".to_owned()),
    }
}

/// What a read may see (default: this repository, when known, and the global scope).
fn view(server: &mut Server, scope: Option<&str>, project_path: Option<&str>) -> Result<View, String> {
    match scope_arg(scope, "both")? {
        "global" => Ok(View {
            repo: None,
            global: true,
        }),
        "repo" => match server.projects.resolve(project_path)? {
            Some(key) => Ok(View {
                repo: Some(key),
                global: false,
            }),
            None => Err(NO_PROJECT.to_owned()),
        },
        _ => Ok(View {
            repo: server.projects.resolve(project_path)?,
            global: true,
        }),
    }
}

/// The root of the checkout the server runs in (never the one `project_path` names).
fn own_root(server: &mut Server) -> Result<std::path::PathBuf, String> {
    match server.projects.resolve(None)? {
        Some(key) => Ok(key.root),
        None => Err(NO_CHECKOUT.to_owned()),
    }
}

/// Who is writing: the client, the root of the checkout the server runs in (`None` outside one),
/// and whether `project_path` named another repository than that one. The root is always the
/// writer's own, never the checkout `project_path` named, so a note does not claim to come from a
/// checkout that did not write it.
pub(super) fn provenance(server: &mut Server, project_path: Option<&str>) -> Provenance {
    let named = server.projects.resolve(project_path).ok().flatten().map(|key| key.root);
    let given = project_path.is_some_and(|raw| !raw.trim().is_empty());

    let own = if given {
        server.projects.resolve(None).ok().flatten().map(|key| key.root)
    } else {
        named.clone()
    };

    Provenance {
        via_override: named != own,
        root: own,
        source: server.client.clone(),
    }
}
