//! `harness-memory hooks put` and `routines put`: a file from standard input into the team
//! database (see the parent module for the command and its refusals).

use std::io::Read;
use std::process::ExitCode;

use super::{Code, Opened, changed, expected_sha, missing, row_json, usage};
use crate::backend::Place;
use crate::cli::{self, Args};
use crate::limits::TEAM_DOC_BODY_MAX;
use crate::store::{DocDraft, DocKind, DocPut, StoreError};

/// `put`.
pub(super) fn put(kind: DocKind, args: &[Option<&str>]) -> ExitCode {
    let valued = ["--project", "--expect-sha", "--author"];
    let Some(parsed) = Args::parse(args, &["--json"], &valued) else {
        return usage(kind);
    };

    let (Ok(project), Some(expect)) = (parsed.project(), expected_sha(&parsed)) else {
        return usage(kind);
    };

    if !parsed.positional.is_empty() || !parsed.flag("--json") {
        return usage(kind);
    }

    let Some(mut opened) = Opened::open(kind) else {
        return ExitCode::FAILURE;
    };

    if opened.selection.docs_place(kind) == Place::Local {
        return opened.not_shared();
    }

    let body = match read_body(kind) {
        Ok(body) => body,
        Err(reason) => return opened.refused(Code::Invalid, &reason),
    };

    let checked = match kind.check_team_body(&body) {
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

    let repo = match checked.repo_in(kind, identity.as_deref()) {
        Ok(repo) => repo,
        Err(problem) => return opened.problem(&problem),
    };

    let author = match opened.author(parsed.value("--author")) {
        Ok(author) => author,
        Err(exit) => return exit,
    };

    let draft = DocDraft {
        id: checked.id,
        repo,
        body,
        author,
    };

    let store = match opened.shared() {
        Ok(store) => store,
        Err(exit) => return exit,
    };

    let noun = kind.noun();

    match store.put_doc(kind, &draft, expect.as_deref()) {
        Ok(DocPut::Saved(doc)) => {
            println!("{}", row_json(&doc));
            ExitCode::SUCCESS
        }
        Ok(DocPut::Exists) => opened.refused(
            Code::Conflict,
            &format!(
                "a team {noun} with the id {} exists already: update it with --expect-sha",
                draft.id
            ),
        ),
        Ok(DocPut::Missing) => opened.refused(Code::NotFound, &missing(kind, &draft.id)),
        Ok(DocPut::Changed) => opened.refused(Code::Conflict, &changed(kind, &draft.id)),
        Ok(DocPut::Full) => {
            let scope = draft.repo.as_deref().unwrap_or("global");
            opened.refused(
                Code::Limit,
                &format!(
                    "{scope} holds {} team {} already: remove one from the team first",
                    kind.per_scope(),
                    kind.table()
                ),
            )
        }
        Err(StoreError::Invalid(reason)) => opened.refused(Code::Invalid, &reason),
        Err(error) => opened.failed(&error.to_string()),
    }
}

/// Standard input, when it is at most [`TEAM_DOC_BODY_MAX`] bytes of UTF-8.
fn read_body(kind: DocKind) -> Result<String, String> {
    let noun = kind.noun();
    let mut bytes = Vec::new();
    let limit = u64::try_from(TEAM_DOC_BODY_MAX + 1).unwrap_or(u64::MAX);

    std::io::stdin()
        .lock()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read the {noun} from standard input ({})", error.kind()))?;

    if bytes.len() > TEAM_DOC_BODY_MAX {
        return Err(format!(
            "the {noun} file is invalid: it is larger than {TEAM_DOC_BODY_MAX} bytes"
        ));
    }

    String::from_utf8(bytes).map_err(|_| format!("the {noun} file is invalid: it is not UTF-8"))
}
