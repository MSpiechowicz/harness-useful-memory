//! `harness-memory scripts save --json [--project PATH] [--client NAME]`: the user's own script, saved as
//! `script_save` saves an agent's but past the save guards (see [`crate::cli_held`] for the
//! refusals). It reads `{name, description, language, body, timeoutSecs, scope}` (`scope` `repo`,
//! the default, or `global`) on standard input, saves from `--project` (else the working folder)
//! as client `--client` (the author the script shows, as `clientInfo.name` names an MCP client;
//! none without it, and `invalid` for a name the store would not keep as given) and prints `{"name": "...", "scope": "repo", "id": N, "created": true, "approval": "local"}`.
//! The script is trusted on this machine as `script_save` trusts it: never the code of a script
//! waiting for approval (`approval` then says `needed`, and the user approves it with `trust`).

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::cli::Args;
use crate::cli_held::{Code, Saver, arguments_of, client_of, read_draft, refused};
use crate::cli_trust::usage;
use crate::mcp::user;

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
    let Some(parsed) = Args::parse(args, &["--json"], &["--project", "--client"]) else {
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

    let arguments = match read_draft() {
        Ok(Some(draft)) => match arguments_of(draft, &SCRIPT_FIELDS) {
            Ok(arguments) => arguments,
            Err(message) => return refused(Code::Invalid, &message),
        },
        Ok(None) => return refused(Code::Invalid, "the draft goes on standard input, as a JSON object"),
        Err(message) => return refused(Code::Invalid, &message),
    };

    let Some(mut saver) = Saver::open(project, client) else {
        return ExitCode::FAILURE;
    };

    let saved = match user::save_script(&mut saver.server, Value::Object(arguments)) {
        Ok(saved) => saved,
        Err(message) => return saver.refused(Code::Refused, &message),
    };

    let script = &saved["script"];
    println!(
        "{}",
        json!({
            "name": script["name"],
            "scope": script["scope"],
            "id": script["id"],
            "created": saved["created"],
            "approval": script["approval"],
        })
    );
    ExitCode::SUCCESS
}
