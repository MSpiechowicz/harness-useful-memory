//! The `settings` command of the `harness-memory` executable, where the person at the terminal
//! (and Harness Desktop on their behalf) reads and changes the settings the memory database
//! keeps, which a team shares when the notes are in a team database. There is no MCP tool for it.
//!
//! - `harness-memory settings get --json` prints `{"english_notes": bool}`: whether agents' notes
//!   have to be in English (on when nothing is stored). It reads the store the notes are saved
//!   to, the team database when any scope saves there, else this computer's file.
//! - `harness-memory settings set english_notes=true|false --json` stores the value in every
//!   store a scope saves to, and prints what is stored now, as `get` does. A database whose
//!   schema this server could not extend (no right to change it) has no settings storage: the
//!   command then exits 1 with the reason.
//!
//! Other failures exit 1 with the reason on stderr, usage errors exit 2, as in [`crate::cli`].

use std::process::ExitCode;

use serde_json::json;

use crate::cli::{self, Args};
use crate::open::open_notes;
use crate::store::StoreError;

/// The usage of this command, for the executable's usage line.
pub const USAGE: &str = "settings get --json | settings set english_notes=true|false --json";

/// The setting `get` and `set` name.
const ENGLISH_NOTES: &str = "english_notes";

/// What `settings` was asked to do.
enum Ask {
    Get,
    Set(bool),
}

/// `harness-memory settings <args>`.
pub fn settings(args: &[Option<&str>]) -> ExitCode {
    let Some(ask) = parse(args) else {
        return cli::usage(USAGE);
    };

    let Some((env, selection)) = cli::settings() else {
        return ExitCode::FAILURE;
    };

    let Some(mut store) = open_notes(&env, &selection) else {
        return ExitCode::FAILURE;
    };

    let secrets = selection.backend.secrets();
    let failed = |error: StoreError| {
        eprintln!("harness-memory: {}", secrets.scrub(&error.to_string()));
        ExitCode::FAILURE
    };

    if let Ask::Set(on) = ask
        && let Err(error) = store.set_english_notes(on)
    {
        return failed(error);
    }

    match store.english_notes() {
        Ok(on) => {
            println!("{}", json!({ ENGLISH_NOTES: on }));
            ExitCode::SUCCESS
        }
        Err(error) => failed(error),
    }
}

/// `get --json` or `set english_notes=BOOL --json`; `None` for anything else.
fn parse(args: &[Option<&str>]) -> Option<Ask> {
    let (command, rest) = args.split_first()?;
    let parsed = Args::parse(rest, &["--json"], &[])?;

    if !parsed.flag("--json") {
        return None;
    }

    match (command.as_ref()?, &parsed.positional[..]) {
        (&"get", []) => Some(Ask::Get),
        (&"set", [assignment]) => {
            let (name, value) = assignment.split_once('=')?;

            if name != ENGLISH_NOTES {
                return None;
            }

            match value {
                "true" => Some(Ask::Set(true)),
                "false" => Some(Ask::Set(false)),
                _ => None,
            }
        }
        _ => None,
    }
}
