//! `tools/call`: argument parsing, scope resolution and the compact text replies.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Server;
use super::blocks::{FRAME, hit_block, note_block, rewrite_line, saved_line, similar_line};
use super::reply::{Capped, now};
use super::scope::{kind, own_root, provenance, scope_arg, target, view};
use crate::limits::{
    DEPTH_MAX, FEEDBACK_REASON_MAX, GET_REPLY_MAX, LIST_REPLY_MAX, NEIGHBORS_LIMIT_DEFAULT, SEARCH_LIMIT_DEFAULT,
    SEARCH_LIMIT_MAX, SIMILAR_MAX,
};
use crate::store::{Forget, Link, NoteDraft, NoteOrigin, NotePatch, Saved, SearchQuery, Target, View, clean};
use crate::support::lang::Lang;

/// A tool result.
pub(super) struct Reply {
    pub text: String,
    pub is_error: bool,
    /// Sent as `structuredContent` next to the text, when set.
    pub structured: Option<Value>,
}

/// Runs tool `name`; `None` when there is no such tool.
pub(super) fn call(server: &mut Server, name: &str, arguments: Value) -> Option<Reply> {
    // A note that is not in English is refused before the guards, so it is never held for review.
    if let Some(reply) = super::english::gate(server, name, &arguments) {
        return Some(reply);
    }

    // The save guards refuse or hold a write before any tool body runs.
    if let Some(reply) = super::guards::gate(server, name, &arguments) {
        return Some(reply);
    }

    // A read says when the user does not plainly let agents save what it reads, in room its
    // capped reply keeps free.
    let notice = super::saving::notice(server, name, &arguments);
    // The notes store decides now whether it is offline, so the notice has room in a capped reply.
    let _ = server.store.ready();
    let offline = server.store.offline() || server.scripts.offline();
    let reserved =
        notice.as_ref().map_or(0, super::saving::Notice::room) + if offline { OFFLINE_NOTICE.len() } else { 0 };
    let mut reply = run(server, name, arguments, reserved)?;

    if let Some(notice) = notice {
        notice.add_to(&mut reply);
    }

    // Said last, so the agent and the user know the notes and scripts they see are this computer's.
    if !reply.is_error && (server.store.offline() || server.scripts.offline()) {
        reply.text.push_str(OFFLINE_NOTICE);
    }

    Some(reply)
}

/// Ends a reply while the offline fallback serves notes or scripts from this computer.
const OFFLINE_NOTICE: &str = "\nOffline: the team database is unreachable, so this reply comes from this computer's \
     notes and scripts. What is saved now is copied to the team database when it is back.";

/// Runs the body of tool `name`, keeping `reserved` bytes of a capped reply free; `None` when
/// there is no such tool.
fn run(server: &mut Server, name: &str, arguments: Value, reserved: usize) -> Option<Reply> {
    let result = match name {
        "note_save" => parse(arguments).and_then(|args| remember(server, args)),
        "note_update" => parse(arguments).and_then(|args| update(server, args)),
        "note_search" => parse(arguments).and_then(|args| search(server, args, reserved)),
        "note_get" => parse(arguments).and_then(|args| get(server, args, reserved)),
        "note_delete" => parse(arguments).and_then(|args| delete_note(server, args)),
        "note_feedback" => parse(arguments).and_then(|args| feedback(server, args)),
        "entity_link" => parse(arguments).and_then(|args| link(server, args)),
        "entity_neighbors" => parse(arguments).and_then(|args| neighbors(server, args, reserved)),
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
pub(super) struct RememberArgs {
    pub title: String,
    pub body: String,
    pub kind: String,
    pub tags: Option<Vec<String>>,
    pub entities: Option<Vec<String>>,
    pub scope: Option<String>,
    pub project_path: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct UpdateArgs {
    pub id: i64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub append: Option<String>,
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
    pub scope: Option<String>,
    pub project_path: Option<String>,
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
struct FeedbackArgs {
    ids: Vec<i64>,
    reason: Option<String>,
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
    let (target, saved) = save_note(server, args)?;
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

/// Saves the note `args` describe (the body of `note_save`): where it went, and the result.
pub(super) fn save_note(server: &mut Server, args: RememberArgs) -> Result<(Target, Saved), String> {
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

    super::vectors::refresh(server, saved.id);

    Ok((target, saved))
}

fn update(server: &mut Server, args: UpdateArgs) -> Result<String, String> {
    update_note(server, args).map(|saved| saved_line(&saved))
}

/// Changes the note `args` name (the body of `note_update`).
pub(super) fn update_note(server: &mut Server, args: UpdateArgs) -> Result<Saved, String> {
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

    super::vectors::refresh(server, saved.id);

    Ok(saved)
}

fn search(server: &mut Server, args: SearchArgs, reserved: usize) -> Result<String, String> {
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

    let found = super::vectors::search(server, &query).map_err(|error| error.to_string())?;
    let hits = found.hits;
    let scripts = super::search_scripts::block(server, &query.view, &query.text);
    let home = server.projects.home();

    // The saved scripts always fit: the notes get the room they and the notice leave.
    let room = scripts.as_ref().map_or(0, |block| block.len() + 1);
    let mut reply = Capped::new(LIST_REPLY_MAX.saturating_sub(room + reserved));

    if !hits.is_empty() {
        reply.push(FRAME);
    }

    if query.view.repo.is_none() && query.view.global && scope_arg(args.scope.as_deref(), "both")? == "both" {
        reply.push("(repository unknown: global notes only; pass project_path for this repository's notes)");
    }

    if let Some(notice) = found.notice {
        reply.push(notice);
    }

    if hits.is_empty() {
        reply.push("no matching notes");
    }

    // Only the notes the reply really shows are counted as shown: the cap can leave some out.
    let mut shown: Vec<i64> = Vec::new();

    for hit in &hits {
        if reply.push(&hit_block(hit, &home)) {
            shown.push(hit.id);
        }
    }

    let mut text = reply.finish();

    // The desktop searches in the background (to save or import notes): that is not an agent
    // being shown a note. The counter is written after the reply (see [`Server::after_reply`]) and never fails the search.
    if server.client.as_deref() != Some(DESKTOP_CLIENT) && !shown.is_empty() {
        server.queue_shown(&shown);
    }

    if let Some(block) = scripts {
        text.push('\n');
        text.push_str(&block);
    }

    Ok(text)
}

/// The client name Harness Desktop initializes the server with.
const DESKTOP_CLIENT: &str = "harness-desktop";

/// The body of `note_feedback`: counts the notes among `args.ids` this caller can see as useful.
/// The reason is for the agent to word its answer: harness-memory keeps none of it.
fn feedback(server: &mut Server, args: FeedbackArgs) -> Result<String, String> {
    if args.ids.len() > SEARCH_LIMIT_MAX {
        return Err(format!("ids must list at most {SEARCH_LIMIT_MAX} note ids"));
    }

    if args
        .reason
        .as_deref()
        .is_some_and(|reason| reason.chars().count() > FEEDBACK_REASON_MAX)
    {
        return Err(format!("reason must be at most {FEEDBACK_REASON_MAX} characters"));
    }

    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;

    // Each id once, in the order asked.
    let mut asked: Vec<i64> = Vec::with_capacity(args.ids.len());

    for id in &args.ids {
        if !asked.contains(id) {
            asked.push(*id);
        }
    }

    // A read that counts no hit: the notes hidden on this computer or out of the view are not found,
    // and a note whose database (or hidden list) cannot be read is not checked, so not counted.
    let checked = server.store.countable(&view, &asked);
    let visible: Vec<i64> = asked
        .iter()
        .copied()
        .filter(|id| checked.visible.contains(id))
        .collect();

    // Written after the reply, once per note and session; a counter never fails the call.
    if !visible.is_empty() {
        server.queue_useful(&visible, now());
    }

    let mut text = match visible.len() {
        0 => "marked no notes useful".to_owned(),
        1 => format!("marked 1 note useful: #{}", visible[0]),
        count => {
            let marked: Vec<String> = visible.iter().map(|id| format!("#{id}")).collect();
            format!("marked {count} notes useful: {}", marked.join(", "))
        }
    };

    let listed = |wanted: &dyn Fn(&i64) -> bool| -> Vec<String> {
        asked.iter().filter(|id| wanted(id)).map(i64::to_string).collect()
    };
    let missing = listed(&|id| !checked.visible.contains(id) && !checked.unchecked.contains(id));
    let unchecked = listed(&|id| !checked.visible.contains(id) && checked.unchecked.contains(id));

    if !missing.is_empty() {
        text.push_str(&format!("\nnot found: {}", missing.join(", ")));
    }

    if !unchecked.is_empty() {
        text.push_str(&format!("\nnot checked: {}", unchecked.join(", ")));
    }

    Ok(text)
}

fn get(server: &mut Server, args: GetArgs, reserved: usize) -> Result<String, String> {
    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let notes = server.store.get(&view, &args.ids).map_err(|error| error.to_string())?;
    let now = now();
    let home = server.projects.home();
    let mut reply = Capped::new(GET_REPLY_MAX.saturating_sub(reserved));

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

    // The setting is read only when a note could use the hint; one that cannot be read gives none.
    let flagged = notes.iter().any(|note| note.lang == Some(Lang::Other));
    let rewrite = flagged && server.store.english_notes().unwrap_or(false);

    for (index, note) in notes.iter().enumerate() {
        let mut block = note_block(note, now, &home);

        // The hint asks for a `note_update`, so only a note the caller may change gets it (a team
        // note in a `remote+local` scope is read-only here).
        let writable = server.store.origin(note.id, note.scope) != (NoteOrigin::Team { read_only: true });

        if rewrite && writable && note.lang == Some(Lang::Other) {
            block.push('\n');
            block.push_str(&rewrite_line(note));
        }

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

fn neighbors(server: &mut Server, args: NeighborsArgs, reserved: usize) -> Result<String, String> {
    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let depth = u8::try_from(args.depth.unwrap_or(1).clamp(1, i64::from(DEPTH_MAX))).unwrap_or(1);
    let limit = args.limit.map_or(NEIGHBORS_LIMIT_DEFAULT, |limit| {
        usize::try_from(limit.max(1)).unwrap_or(1)
    });

    let around = server
        .store
        .neighbors(&view, &args.entity, depth, limit)
        .map_err(|error| error.to_string())?;

    let mut reply = Capped::new(LIST_REPLY_MAX.saturating_sub(reserved));

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
    super::vectors::forgot(server, args.id);

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
    serde_json::from_value(arguments).map_err(|error| {
        format!("invalid arguments: {error}. Check the tool's input schema in tools/list for the required fields")
    })
}
