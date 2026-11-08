//! `harness-memory scripts save --json [--project PATH] [--client NAME]`: the user's own script, saved as
//! `script_save` saves an agent's but past the save guards (see [`crate::cli_held`] for the
//! refusals). It reads `{name, description, language, body, timeoutSecs, scope}` (`scope` `repo`,
//! the default, or `global`) on standard input, saves from `--project` (else the working folder)
//! as client `--client` (the author the script shows, as `clientInfo.name` names an MCP client;
//! none without it, and `invalid` for a name the store would not keep as given) and prints `{"name": "...", "scope": "repo", "id": N, "created": true, "approval": "local"}`.
//! The script is trusted on this machine as `script_save` trusts it: never the code of a script
//! waiting for approval (`approval` then says `needed`, and the user approves it with `trust`).
//!
//! `--place remote|local` picks where the script goes when scripts are set to `remote+local`
//! (without it a script goes where `remote_local_saves` says, the team database by default, except
//! that a script of this computer with that name in the scope is replaced there, online or not, and
//! listed as waiting for nothing). `local` saves to this computer whatever that setting says, and
//! stays there: the script is not listed as waiting for the team database, also while it is
//! unreachable, and one an agent saved there while it was down is taken off the list. `remote` saves to the team database,
//! as a save with scripts set to `remote` does (the hidden name check, the store's own rules,
//! trusted on this machine under the team database's name as any save there), and fails, saving
//! nothing, when it cannot be reached: the script is not kept on this computer instead, nor listed as
//! waiting. For any other routing `--place` is `invalid`. The answer then adds `"place":
//! "remote"|"local"`.

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::cli::Args;
use crate::cli_held::{Code, Saver, arguments_of, check_place, client_of, place_of, read_draft, refused};
use crate::cli_trust::usage;
use crate::mcp::user;
use crate::store::pending::mark_not_listed;

/// The fields of a script draft, each with the tool argument it gives.
const SCRIPT_FIELDS: [(&str, &str); 6] = [
    ("name", "name"),
    ("description", "description"),
    ("language", "language"),
    ("body", "body"),
    ("timeoutSecs", "timeout_secs"),
    ("scope", "scope"),
];

/// `scripts save`.
pub(crate) fn save(args: &[Option<&str>]) -> ExitCode {
    let Some(parsed) = Args::parse(args, &["--json"], &["--project", "--client", "--place"]) else {
        return usage();
    };

    let project = parsed.value("--project");

    if !parsed.flag("--json") || !parsed.positional.is_empty() || project.is_some_and(|path| path.trim().is_empty()) {
        return usage();
    }

    let client = match client_of(parsed.value("--client")) {
        Ok(client) => client,
        Err(exit) => return exit,
    };

    let place = match place_of(parsed.value("--place")) {
        Ok(place) => place,
        Err(exit) => return exit,
    };

    let arguments = match read_draft() {
        Ok(Some(draft)) => match arguments_of(draft, &SCRIPT_FIELDS) {
            Ok(arguments) => arguments,
            Err(message) => return refused(Code::Invalid, &message),
        },
        Ok(None) => return refused(Code::Invalid, "the draft goes on standard input, as a JSON object"),
        Err(message) => return refused(Code::Invalid, &message),
    };

    let Some((mut saver, routed)) = Saver::open_routed(project, client) else {
        return ExitCode::FAILURE;
    };

    if place.is_some()
        && let Err(exit) = check_place(routed.scripts, "scripts")
    {
        return exit;
    }

    // A place a person picked wins over `remote_local_saves`, either way.
    let arguments = Value::Object(arguments);
    let saved = match place {
        Some(place) => user::save_script_to(&mut saver.server, arguments, place),
        None => user::save_script(&mut saver.server, arguments),
    };

    let saved = match saved {
        Ok(saved) => saved,
        Err(message) => return saver.refused(Code::Refused, &message),
    };

    let script = &saved["script"];
    let mut shown = json!({
        "name": script["name"],
        "scope": script["scope"],
        "id": script["id"],
        "created": saved["created"],
        "approval": script["approval"],
    });

    if saved["notListed"] == json!(true) {
        mark_not_listed(&mut shown);
    }

    if let Some(place) = place {
        shown["place"] = json!(place.as_str());
    }

    println!("{shown}");
    ExitCode::SUCCESS
}
