//! `harness-memory hooks put`: a hook file from standard input into the team database (see the
//! parent module for the command and its refusals).

use std::io::Read;
use std::process::ExitCode;

use super::{Code, Opened, changed, expected_sha, row_json, usage};
use crate::backend::Place;
use crate::cli::{self, Args};
use crate::limits::{HOOK_AUTHOR_MAX, HOOK_BODY_MAX};
use crate::store::hooks::check_author;
use crate::store::hooks::format::check_team_body;
use crate::store::{HookDraft, HookPut, StoreError};
use crate::support::Env;

/// The author when neither `--author` nor a usable user name gives one.
const UNKNOWN_AUTHOR: &str = "unknown";

/// `hooks put`.
pub(super) fn put(args: &[Option<&str>]) -> ExitCode {
    let valued = ["--project", "--expect-sha", "--author"];
    let Some(parsed) = Args::parse(args, &["--json"], &valued) else {
        return usage();
    };

    let (Ok(project), Some(expect)) = (parsed.project(), expected_sha(&parsed)) else {
        return usage();
    };

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return usage();
    }

    let Some(mut opened) = Opened::open() else {
        return ExitCode::FAILURE;
    };

    if opened.selection.hooks_place() == Place::Local {
        return opened.not_shared();
    }

    let body = match read_body() {
        Ok(body) => body,
        Err(reason) => return opened.refused(Code::Invalid, &reason),
    };

    let checked = match check_team_body(&body) {
        Ok(checked) => checked,
        Err(problem) => return opened.problem(&problem),
    };

    let identity = match project {
        Some(path) => match cli::repository(&opened.env, Some(&path)) {
            Some(Some(identity)) => Some(identity.key),
            Some(None) => {
                return opened.refused(
                    Code::Refused,
                    "--project is not inside a repository (the home folder and filesystem roots are none)",
                );
            }
            None => return ExitCode::FAILURE,
        },
        None => None,
    };

    let repo = match checked.repo_for(identity.as_deref()) {
        Ok(repo) => repo,
        Err(problem) => return opened.problem(&problem),
    };

    let author = match parsed.value("--author") {
        Some(author) => match check_author(author) {
            Ok(author) => author,
            Err(error) => return opened.refused(Code::Invalid, &error.to_string()),
        },
        None => default_author(&opened.env),
    };

    let draft = HookDraft {
        id: checked.id,
        repo,
        body,
        author,
    };

    let store = match opened.shared() {
        Ok(store) => store,
        Err(exit) => return exit,
    };

    match store.put_hook(&draft, expect.as_deref()) {
        Ok(HookPut::Saved(hook)) => {
            println!("{}", row_json(&hook));
            ExitCode::SUCCESS
        }
        Ok(HookPut::Exists) => opened.refused(
            Code::Conflict,
            &format!(
                "a team hook with the id {} exists already: update it with --expect-sha",
                draft.id
            ),
        ),
        Ok(HookPut::Missing) => opened.refused(Code::NotFound, &format!("no team hook has the id {}", draft.id)),
        Ok(HookPut::Changed) => opened.refused(Code::Conflict, &changed(&draft.id)),
        Ok(HookPut::Full) => {
            let scope = draft.repo.as_deref().unwrap_or("global");
            opened.refused(
                Code::Limit,
                &format!("{scope} holds 64 team hooks already: remove one from the team first"),
            )
        }
        Err(StoreError::Invalid(reason)) => opened.refused(Code::Invalid, &reason),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// Standard input, when it is at most [`HOOK_BODY_MAX`] bytes of UTF-8.
fn read_body() -> Result<String, String> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(HOOK_BODY_MAX + 1).unwrap_or(u64::MAX);

    std::io::stdin()
        .lock()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read the hook from standard input ({})", error.kind()))?;

    if bytes.len() > HOOK_BODY_MAX {
        return Err(format!(
            "the hook file is invalid: it is larger than {HOOK_BODY_MAX} bytes"
        ));
    }

    String::from_utf8(bytes).map_err(|_| "the hook file is invalid: it is not UTF-8".to_owned())
}

/// The user name (`USER`, else `USERNAME`) trimmed and cut to [`HOOK_AUTHOR_MAX`] characters;
/// [`UNKNOWN_AUTHOR`] when there is none or it holds a control or invisible character.
fn default_author(env: &Env) -> String {
    let name = env.var("USER").or_else(|| env.var("USERNAME")).unwrap_or_default();
    let cut: String = name.trim().chars().take(HOOK_AUTHOR_MAX).collect();

    check_author(&cut).unwrap_or_else(|_| UNKNOWN_AUTHOR.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_author_is_the_user_name() {
        let env = |name: &str, value: &str| Env::for_tests("/home/u").with_var(name, value);

        assert_eq!(default_author(&env("USER", " ada ")), "ada");
        assert_eq!(default_author(&env("USERNAME", "grace")), "grace");
        assert_eq!(default_author(&env("USER", &"a".repeat(100))), "a".repeat(80));
        assert_eq!(default_author(&env("USER", "a\u{202E}b")), UNKNOWN_AUTHOR);
        assert_eq!(default_author(&Env::for_tests("/home/u")), UNKNOWN_AUTHOR);
    }
}
