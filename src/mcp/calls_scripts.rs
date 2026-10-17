//! `tools/call` for the script tools: argument checks, the trust check before a run, and the
//! replies (text and `structuredContent`, see [`super::script_blocks`]).
//!
//! A script runs only when this machine trusts its version (saved here, or approved by a person
//! here, see [`super::approval`]); no agent call can approve one, and unknown arguments are
//! refused. The hash ([`script_sha256`]) covers the language and the body read for the run and is
//! never taken from the store, so a body or language changed behind the store's back is not
//! trusted. `script_save` trusts the draft the agent sent, and only when the store gives back that
//! draft unchanged; never the code of a script waiting for approval, or of one reported as waiting
//! before. A body with a [`hidden_character`], or a Python body that declares a
//! [`foreign_encoding`], never runs.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::Server;
use super::approval::{self, Answer, RunInput, approval_of};
use super::calls::{NO_PROJECT, Reply, parse, provenance, target};
use super::reply::now;
use super::script_blocks::{Approval, SCRIPTS_FRAME, Shown, run_json, run_text, saved_line};
use crate::backend::Place;
use crate::limits::{
    QUERY_MAX, SCRIPT_ARG_MAX, SCRIPT_ARGS_MAX, SCRIPT_STDERR_MAX, SCRIPT_STDIN_MAX, SCRIPT_STDOUT_MAX,
    SCRIPT_TIMEOUT_DEFAULT,
};
use crate::store::{Language, Scope, Script, ScriptDraft, ScriptRef, ScriptSummary, StoreError, Target, View};
use crate::store::{TrustOrigin, TrustedVersion, checked_draft, declares_encoding, script_sha256};
use crate::store::{foreign_encoding, foreign_encoding_error, hidden_character, hidden_character_error};
use crate::support::runner::{self, Run};

/// Runs script tool `name`; `None` when there is no such tool.
pub(super) fn call(server: &mut Server, name: &str, arguments: Value) -> Option<Reply> {
    let result = match name {
        "script_save" => parse(arguments).and_then(|args| save(server, args)),
        "script_list" => parse(arguments).and_then(|args| list(server, args)),
        "script_get" => parse(arguments).and_then(|args| get(server, args)),
        "script_run" => parse(arguments).and_then(|args| run(server, args)),
        "script_delete" => parse(arguments).and_then(|args| delete(server, args)),
        _ => return None,
    };

    Some(result.unwrap_or_else(|text| Reply {
        text,
        is_error: true,
        structured: None,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveArgs {
    scope: Option<String>,
    name: String,
    description: String,
    language: String,
    body: String,
    timeout_secs: Option<u64>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    scope: Option<String>,
    query: Option<String>,
    project_path: Option<String>,
}

/// The arguments of `script_get` and `script_delete`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefArgs {
    id: Option<i64>,
    name: Option<String>,
    scope: Option<String>,
    project_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    id: Option<i64>,
    name: Option<String>,
    args: Option<Vec<String>>,
    stdin: Option<String>,
    scope: Option<String>,
    project_path: Option<String>,
}

fn save(server: &mut Server, args: SaveArgs) -> Result<Reply, String> {
    let target = target(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let Some(language) = Language::parse(&args.language) else {
        let languages: Vec<&str> = Language::ALL.iter().map(|language| language.as_str()).collect();
        return Err(format!("language must be one of {}", languages.join(", ")));
    };

    let draft = ScriptDraft {
        name: args.name,
        description: args.description,
        language,
        body: args.body,
        timeout_secs: args.timeout_secs.unwrap_or(SCRIPT_TIMEOUT_DEFAULT),
        provenance: provenance(server, args.project_path.as_deref()),
    };
    let draft = checked_draft(&draft).map_err(|error| error.to_string())?;

    // Looked up before the save, which may replace the script waiting for approval.
    let view = view(server, None, args.project_path.as_deref())?;
    let copied = unapproved_copy(server, &view, &draft.body);
    let seen = server
        .trust
        .was_unapproved(&every_language_sha256(&draft.body))
        .map_err(|error| error.to_string());

    let saved = server
        .scripts
        .save_script(&target, &draft)
        .map_err(|error| error.to_string())?;
    let script = &saved.script;

    // What the agent sent is what is trusted, never what the store gives back.
    let untrusted_because = if !stored_as_drafted(script, &draft, &target) {
        Some("the stored version differs from what was saved".to_owned())
    } else {
        match (copied, seen) {
            (Ok(Some(original)), _) => Some(copy_reason(&original)),
            (_, Ok(true)) => Some("its code waited for approval on this machine before".to_owned()),
            (Err(error), _) | (_, Err(error)) => Some(format!(
                "the scripts waiting for approval could not be checked ({error})"
            )),
            (Ok(None), Ok(false)) => None,
        }
    };

    let mut note = String::new();

    match untrusted_because {
        Some(reason) => note = format!("\nit needs the user's approval before it runs: {reason}"),
        None => trust_draft(server, script.summary.id, &draft, &target).map_err(|error| {
            format!(
                "saved script {} {}, but this machine could not trust it: {error}",
                script.summary.id, script.summary.name
            )
        })?,
    }

    let sha256 = script_sha256(script.summary.language, &script.body);
    let approval = approval_for(server, server.scripts_place, &script.summary, &sha256)?;
    let shown = shown(server, &script.summary, &sha256, approval);
    Ok(Reply {
        text: format!("{}{note}", saved_line(&shown, saved.created)),
        is_error: false,
        structured: Some(json!({ "script": shown.json(), "created": saved.created })),
    })
}

/// True when the store gave back `draft` as saved into `target`: the same name, scope,
/// repository, language and body (a database trigger or another writer may change a row as it is
/// written).
fn stored_as_drafted(script: &Script, draft: &ScriptDraft, target: &Target) -> bool {
    let (scope, project) = scope_of(target);
    let summary = &script.summary;

    script.body == draft.body
        && summary.language == draft.language
        && summary.name == draft.name
        && summary.scope == scope
        && summary.project.as_deref() == project
}

/// The scope and repository key of `target`.
fn scope_of(target: &Target) -> (Scope, Option<&str>) {
    match target {
        Target::Repo(key) => (Scope::Repo, Some(key.key.as_str())),
        Target::Global => (Scope::Global, None),
    }
}

/// The trust hash of `body` in every language: a version of it reported as waiting for approval
/// in any language is not trusted when saved again.
fn every_language_sha256(body: &str) -> Vec<String> {
    Language::ALL
        .iter()
        .map(|language| script_sha256(*language, body))
        .collect()
}

/// Trusts `draft`, saved here into `target` as script `id`, as saved on this machine.
fn trust_draft(server: &mut Server, id: i64, draft: &ScriptDraft, target: &Target) -> Result<(), StoreError> {
    let place = server.place_key(server.scripts_place);
    let (scope, project) = scope_of(target);
    let version = TrustedVersion {
        place: &place,
        script_id: id,
        name: &draft.name,
        scope,
        project,
        language: draft.language,
        body: &draft.body,
        origin: TrustOrigin::Saved,
    };

    server.trust.trust(&version)
}

/// The first script visible in `view`, in either script store, that waits for approval and has
/// `body`: saving its code again must not trust it. Its id and name, `None` when there is none.
fn unapproved_copy(server: &mut Server, view: &View, body: &str) -> Result<Option<(i64, String)>, String> {
    let places = [server.scripts_place, server.other_place()];

    for place in places {
        let store = if place == server.scripts_place {
            Some(&mut server.scripts)
        } else {
            server.other_scripts.as_mut()
        };

        let Some(store) = store else {
            continue;
        };

        // The same body in any language: the hash of the body in the script's own language.
        let candidates: Vec<ScriptSummary> = store
            .list_scripts(view, None)
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|summary| summary.sha256 == script_sha256(summary.language, body))
            .collect();

        for summary in candidates {
            if !approval_for(server, place, &summary, &summary.sha256)?.is_trusted() {
                return Ok(Some((summary.id, summary.name)));
            }
        }
    }

    Ok(None)
}

/// Why a saved script was not trusted: it is the code of `original`, which waits for approval.
fn copy_reason((id, name): &(i64, String)) -> String {
    format!("it is the code of script {id} {name}, which is not approved on this machine")
}

fn list(server: &mut Server, args: ListArgs) -> Result<Reply, String> {
    let view = view(server, args.scope.as_deref(), args.project_path.as_deref())?;
    let query = args.query.as_deref().map(str::trim).filter(|query| !query.is_empty());

    if query.is_some_and(|query| query.chars().count() > QUERY_MAX) {
        return Err(format!("query is longer than {QUERY_MAX} characters"));
    }

    let summaries = server
        .scripts
        .list_scripts(&view, query)
        .map_err(|error| error.to_string())?;

    let mut lines = Vec::with_capacity(summaries.len() + 1);
    let mut objects = Vec::with_capacity(summaries.len());

    for summary in &summaries {
        let approval = approval_for(server, server.scripts_place, summary, &summary.sha256)?;
        let shown = shown(server, summary, &summary.sha256, approval);
        lines.push(shown.line());
        objects.push(shown.json());
    }

    let text = if lines.is_empty() {
        "no scripts".to_owned()
    } else {
        format!("{SCRIPTS_FRAME}\n{}", lines.join("\n"))
    };

    Ok(Reply {
        text,
        is_error: false,
        structured: Some(json!({ "scripts": objects })),
    })
}

fn get(server: &mut Server, args: RefArgs) -> Result<Reply, String> {
    let (_, script) = find(server, args.id, args.name, args.scope, args.project_path.as_deref())?;
    let sha256 = script_sha256(script.summary.language, &script.body);
    let approval = approval_for(server, server.scripts_place, &script.summary, &sha256)?;
    let home = server.projects.home();

    let shown = shown(server, &script.summary, &sha256, approval);
    Ok(Reply {
        text: format!("{SCRIPTS_FRAME}\n{}", shown.block(&script.body, &home)),
        is_error: false,
        structured: Some(json!({ "script": shown.json_with_body(&script.body) })),
    })
}

fn delete(server: &mut Server, args: RefArgs) -> Result<Reply, String> {
    let (view, script) = find(server, args.id, args.name, args.scope, args.project_path.as_deref())?;
    let ScriptSummary { id, name, .. } = &script.summary;

    // Remembers a version waiting for approval, so saving its code after the delete runs nothing.
    let sha256 = script_sha256(script.summary.language, &script.body);
    approval_for(server, server.scripts_place, &script.summary, &sha256)?;

    let deleted = server
        .scripts
        .delete_script(&view, &ScriptRef::Id(*id))
        .map_err(|error| error.to_string())?;

    if !deleted {
        return Err(format!("script {id} not found"));
    }

    Ok(Reply {
        text: format!("deleted script {id} {name}"),
        is_error: false,
        structured: Some(json!({ "deleted": true, "id": id })),
    })
}

fn run(server: &mut Server, args: RunArgs) -> Result<Reply, String> {
    let script_args = args.args.unwrap_or_default();
    let stdin = args.stdin.unwrap_or_default();
    check_input(&script_args, &stdin)?;

    let project_path = args.project_path.as_deref();
    let (_, script) = find(server, args.id, args.name, args.scope, project_path)?;
    let sha256 = script_sha256(script.summary.language, &script.body);

    if let Some(found) = hidden_character(&script.body) {
        return Err(format!(
            "script {} was not run: {}",
            script.summary.name,
            hidden_character_error(found)
        ));
    }

    if let Some(encoding) = foreign_encoding(script.summary.language, &script.body) {
        return Err(format!(
            "script {} was not run: {}",
            script.summary.name,
            foreign_encoding_error(&encoding)
        ));
    }

    let cwd = working_dir(server, project_path);
    let mut notes = String::new();

    if !approval_for(server, server.scripts_place, &script.summary, &sha256)?.is_trusted() {
        let summary = shown(server, &script.summary, &sha256, Approval::Needed).json();
        let input = RunInput {
            args: &script_args,
            stdin: &stdin,
            cwd: &cwd,
        };
        let answer = approval::ask(server, &script, summary, &input);

        if let Some(refused) = approval::refusal(&script.summary, &answer) {
            return Ok(refused);
        }

        if answer == Answer::Allowed
            && let Err(error) = trust(server, &script, TrustOrigin::Approved)
        {
            notes.push_str(&format!("\n(the user's approval was not recorded: {error})"));
        }
    }

    let (interpreters, file_name) = interpreter(script.summary.language);
    let timeout_secs = script.summary.timeout_secs;

    // Python reads a body after a UTF-8 byte-order mark as UTF-8 whatever it declares. A body that
    // declares UTF-8 itself goes without one, since Python refuses `utf8` after it.
    let bom = script.summary.language == Language::Python && !declares_encoding(&script.body);

    let finished = runner::run(
        server.projects.env(),
        &Run {
            interpreters,
            file_name,
            body: &script.body,
            bom,
            args: &script_args,
            stdin: &stdin,
            cwd: &cwd,
            timeout: Duration::from_secs(timeout_secs),
            stdout_max: SCRIPT_STDOUT_MAX,
            stderr_max: SCRIPT_STDERR_MAX,
        },
    )?;

    let mut text = run_text(&script.summary, timeout_secs, &finished);
    text.push_str(&notes);

    if let Err(error) = server.scripts.record_run(script.summary.id, finished.exit_code, now()) {
        text.push_str(&format!("\n(the run was not counted: {error})"));
    }

    Ok(Reply {
        text,
        is_error: false,
        structured: Some(run_json(script.summary.id, &sha256, &finished)),
    })
}

/// Fails when the arguments or stdin break a limit or hold a NUL byte.
fn check_input(args: &[String], stdin: &str) -> Result<(), String> {
    if args.len() > SCRIPT_ARGS_MAX {
        return Err(format!("at most {SCRIPT_ARGS_MAX} arguments"));
    }

    if args.iter().any(|arg| arg.len() > SCRIPT_ARG_MAX) {
        return Err(format!("an argument is longer than {SCRIPT_ARG_MAX} bytes"));
    }

    if args.iter().any(|arg| arg.contains('\0')) {
        return Err("an argument holds a NUL byte".to_owned());
    }

    if stdin.len() > SCRIPT_STDIN_MAX {
        return Err(format!("stdin is larger than {SCRIPT_STDIN_MAX} bytes"));
    }

    Ok(())
}

/// The interpreters to look for and the file name for `language`.
fn interpreter(language: Language) -> (&'static [&'static str], &'static str) {
    match language {
        Language::Sh => (&["sh"], "script.sh"),
        Language::Bash => (&["bash"], "script.sh"),
        Language::Python => (&["python3", "python"], "script.py"),
        Language::Node => (&["node"], "script.js"),
        Language::Bun => (&["bun"], "script.ts"),
    }
}

/// Where a script runs: the root of the repository the call is about (the script's own for a
/// repository script), else the server's working directory, made absolute.
fn working_dir(server: &mut Server, project_path: Option<&str>) -> PathBuf {
    if let Ok(Some(key)) = server.projects.resolve(project_path) {
        return key.root;
    }

    let cwd = server
        .projects
        .cwd()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    std::path::absolute(&cwd).unwrap_or(cwd)
}

/// The script `id` or `name` picks (exactly one of them) in the scripts `scope` shows, with that
/// view.
fn find(
    server: &mut Server,
    id: Option<i64>,
    name: Option<String>,
    scope: Option<String>,
    project_path: Option<&str>,
) -> Result<(View, Script), String> {
    let name = name.map(|name| name.trim().to_owned()).filter(|name| !name.is_empty());
    let (script_ref, missing) = match (id, name) {
        (Some(id), None) => (ScriptRef::Id(id), format!("script {id} not found")),
        (None, Some(name)) => {
            let missing = format!("no script named {} here or in the global scope", super::short(&name));
            (ScriptRef::Name(name), missing)
        }
        _ => return Err("give exactly one of: id | name".to_owned()),
    };

    let view = view(server, scope.as_deref(), project_path)?;
    let script = server
        .scripts
        .get_script(&view, &script_ref)
        .map_err(|error| error.to_string())?
        .ok_or(missing)?;

    Ok((view, script))
}

/// What a script read may see (default `all`: this repository, when known, and the global scope).
fn view(server: &mut Server, scope: Option<&str>, project_path: Option<&str>) -> Result<View, String> {
    match scope.map(str::trim).filter(|scope| !scope.is_empty()).unwrap_or("all") {
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
        "all" => Ok(View {
            repo: server.projects.resolve(project_path)?,
            global: true,
        }),
        _ => Err("scope must be repo, global or all".to_owned()),
    }
}

/// How this machine may run version `sha256` of `summary`, a script read from the store in
/// `place` (a version that needs approval is remembered, see [`approval_of`]).
fn approval_for(server: &mut Server, place: Place, summary: &ScriptSummary, sha256: &str) -> Result<Approval, String> {
    let team_trusted = server.team_trusted(place);
    let key = server.place_key(place);
    approval_of(server.trust.as_mut(), team_trusted, &key, summary, sha256)
}

/// Trusts the version of `script` read from the server's script store, as approved here.
fn trust(server: &mut Server, script: &Script, origin: TrustOrigin) -> Result<(), StoreError> {
    let place = server.place_key(server.scripts_place);
    server.trust.trust(&TrustedVersion::of(&place, script, origin))
}

fn shown<'a>(server: &Server, summary: &'a ScriptSummary, sha256: &'a str, approval: Approval) -> Shown<'a> {
    Shown {
        summary,
        sha256,
        place: server.scripts_place,
        approval,
    }
}
