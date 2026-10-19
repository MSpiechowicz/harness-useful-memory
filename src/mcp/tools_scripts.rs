//! The script tool definitions (`tools/list`).

use serde_json::{Value, json};

use super::tools::{project_path, text, tool};
use crate::limits::{
    QUERY_MAX, SCRIPT_ARG_MAX, SCRIPT_ARGS_MAX, SCRIPT_BODY_MAX, SCRIPT_DESCRIPTION_MAX, SCRIPT_NAME_MAX,
    SCRIPT_STDIN_MAX, SCRIPT_TIMEOUT_DEFAULT, SCRIPT_TIMEOUT_MAX,
};
use crate::store::Language;

pub(super) fn script_save() -> Value {
    let definition = tool(
        "script_save",
        "Save a deterministic action you repeat (a curl or gh fetch, a report, a lookup) as a script, so later \
         sessions run it with script_run instead of working it out again. A script with the same name in the \
         scope is replaced. Scripts run with the user's environment: read secrets from environment variables \
         (for example $GH_TOKEN), never write them into the body (a body holding a credential is refused). \
         Arguments arrive as $1, $2 (sys.argv, process.argv), never through a shell. What you save runs here \
         without approval, since you wrote it: never save the code of a script that waits for approval, edited \
         or not, to run it (an unedited copy waits for approval too). When scripts are set to remote+local, \
         new scripts are saved on this computer, next to the team's.",
        json!({
            "scope": scope(false, "repo"),
            "name": {
                "type": "string",
                "pattern": "^[a-z0-9][a-z0-9_-]*$",
                "maxLength": SCRIPT_NAME_MAX,
                "description": "Short name, unique in the scope, for example open-prs",
            },
            "description": text(&format!(
                "What the script does and its arguments (at most {SCRIPT_DESCRIPTION_MAX} characters)"
            )),
            "language": language(),
            "body": text(&format!("The script source (at most {SCRIPT_BODY_MAX} bytes)")),
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "maximum": SCRIPT_TIMEOUT_MAX,
                "default": SCRIPT_TIMEOUT_DEFAULT,
                "description": "Seconds a run may take before it is killed",
            },
            "project_path": project_path(),
        }),
        &["name", "description", "language", "body"],
    );

    // Saving replaces a script with the same name.
    annotated(definition, true, false)
}

pub(super) fn script_list() -> Value {
    tool(
        "script_list",
        "List saved scripts, repository scripts first: check here before working out a repeated action \
         again. Without a query every visible script is listed. `place` says where a script is kept (team or \
         local); `readOnly` marks a team script that cannot be changed from here (scripts set to \
         remote+local).",
        json!({
            "scope": scope(true, "all"),
            "query": text(&format!(
                "Keeps scripts whose name or description contains it (at most {QUERY_MAX} characters)"
            )),
            "project_path": project_path(),
        }),
        &[],
    )
}

pub(super) fn script_get() -> Value {
    tool(
        "script_get",
        "Read a script with its body, by id or name (a name is looked up in this repository first, then in \
         the global scope, a script saved on this computer before a team one with the same name). `approval` says why this machine runs it (local: saved here, approved: approved by \
         the user here, team: every team script is trusted) or that the user must approve it first (needed).",
        with_ref(json!({ "scope": scope(true, "all"), "project_path": project_path() })),
        &[],
    )
}

pub(super) fn script_run() -> Value {
    let definition = tool(
        "script_run",
        "Run a saved script by id or name and get its exit code, stdout and stderr (capped, secrets masked). \
         Prefer it over repeating the steps by hand. A script saved on another machine, or changed, renamed or \
         moved since this machine approved it, runs only after the user approves it: when Harness Desktop \
         runs, the call waits up to 45 s for the user's answer there (when your client stops waiting sooner, \
         call script_run again once the user approved). You cannot approve a script: when the run is refused, \
         ask the user to approve it in Harness Desktop → Memory (the user can also run `harness-memory trust` \
         in a terminal), then call script_run again.",
        with_ref(json!({
            "args": {
                "type": "array",
                "items": { "type": "string", "maxLength": SCRIPT_ARG_MAX },
                "maxItems": SCRIPT_ARGS_MAX,
                "description": "Arguments, passed as they are (no shell)",
            },
            "stdin": text(&format!("Standard input (at most {SCRIPT_STDIN_MAX} bytes)")),
            "scope": scope(true, "all"),
            "project_path": project_path(),
        })),
        &[],
    );

    // A script can do anything the user can, the network included.
    annotated(definition, true, true)
}

pub(super) fn script_delete() -> Value {
    let definition = tool(
        "script_delete",
        "Delete a stale or wrong script by id or name. A team script is read-only when scripts are set to \
         remote+local (refused): save a script of your own instead.",
        with_ref(json!({ "scope": scope(true, "all"), "project_path": project_path() })),
        &[],
    );

    annotated(definition, true, false)
}

/// `definition` with the MCP tool annotations of a tool that changes things: whether it may
/// destroy data and whether it reaches outside the memory (clients older than the annotations
/// ignore them).
fn annotated(mut definition: Value, destructive: bool, open_world: bool) -> Value {
    definition["annotations"] = json!({
        "readOnlyHint": false,
        "destructiveHint": destructive,
        "openWorldHint": open_world,
    });
    definition
}

/// `properties` with the `id` and `name` that pick a script first.
fn with_ref(properties: Value) -> Value {
    let mut all = json!({
        "id": { "type": "integer", "description": "Script id (give id or name)" },
        "name": text("Script name (give id or name)"),
    });

    if let (Some(all), Value::Object(rest)) = (all.as_object_mut(), properties) {
        all.extend(rest);
    }

    all
}

fn language() -> Value {
    let languages: Vec<&str> = Language::ALL.iter().map(|language| language.as_str()).collect();
    json!({ "type": "string", "enum": languages, "description": "The interpreter (python: python3, else python)" })
}

fn scope(all: bool, default: &str) -> Value {
    let (values, description): (&[&str], &str) = if all {
        (
            &["repo", "global", "all"],
            "repo: this repository; global: shared by all repositories; all: both",
        )
    } else {
        (
            &["repo", "global"],
            "repo: this repository; global: shared by all repositories",
        )
    };

    json!({
        "type": "string",
        "enum": values,
        "default": default,
        "description": description,
    })
}
