//! Where a tool call reads or writes: the `scope` argument, the repository `project_path` names
//! (else the one the server runs in), and who is writing.

use super::Server;
use crate::store::{NoteKind, ProjectKey, Provenance, Target, View};

/// Sent when a repository-scope call has no repository.
pub(super) const NO_PROJECT: &str = "cannot tell which repository this is (the server runs outside one, or in the home folder \
                          or a filesystem root). Retry the same call with the argument project_path set to the absolute \
                          path of the repository you are working in (its git root, e.g. \"/Users/me/work/repo\"), or with \
                          scope \"global\" for knowledge that belongs to no repository";

/// Sent when `same_checkout` is asked for outside a checkout.
const NO_CHECKOUT: &str = "same_checkout needs the server to run inside a repository checkout";

/// The note kind `raw` names (blanks around it and case ignored).
pub(super) fn kind(raw: &str) -> Result<NoteKind, String> {
    NoteKind::parse(raw).ok_or_else(|| {
        let kinds: Vec<&str> = NoteKind::ALL.iter().map(|kind| kind.as_str()).collect();
        format!("kind must be one of {}", kinds.join(", "))
    })
}

/// `scope` (blanks around it and case ignored) or its default, checked against `repo`, `global`,
/// `both`.
pub(super) fn scope_arg(raw: Option<&str>, default: &'static str) -> Result<&'static str, String> {
    let given = raw
        .map(|raw| raw.trim().to_ascii_lowercase())
        .filter(|raw| !raw.is_empty());

    match given.as_deref().unwrap_or(default) {
        "repo" => Ok("repo"),
        "global" => Ok("global"),
        "both" => Ok("both"),
        _ => Err("scope must be repo, global or both".to_owned()),
    }
}

/// The scope a write goes to (default: this repository).
pub(super) fn target(server: &mut Server, scope: Option<&str>, project_path: Option<&str>) -> Result<Target, String> {
    match scope_arg(scope, "repo")? {
        "global" => Ok(Target::Global),
        "repo" => match resolve(server, project_path)? {
            Some(key) => Ok(Target::Repo(key)),
            None => Err(NO_PROJECT.to_owned()),
        },
        _ => Err("scope must be repo or global here".to_owned()),
    }
}

/// `Projects::resolve`, with the fix added to a refused `project_path`.
fn resolve(server: &mut Server, project_path: Option<&str>) -> Result<Option<ProjectKey>, String> {
    server.projects.resolve(project_path).map_err(|reason| {
        format!(
            "{reason} (got {:?}). Pass project_path as the absolute path of the repository you are working in \
             (its git root, e.g. \"/Users/me/work/repo\"), or use scope \"global\"",
            project_path.unwrap_or_default()
        )
    })
}

/// What a read may see (default: this repository, when known, and the global scope).
pub(super) fn view(server: &mut Server, scope: Option<&str>, project_path: Option<&str>) -> Result<View, String> {
    match scope_arg(scope, "both")? {
        "global" => Ok(View {
            repo: None,
            global: true,
        }),
        "repo" => match resolve(server, project_path)? {
            Some(key) => Ok(View {
                repo: Some(key),
                global: false,
            }),
            None => Err(NO_PROJECT.to_owned()),
        },
        _ => Ok(View {
            repo: resolve(server, project_path)?,
            global: true,
        }),
    }
}

/// The root of the checkout the server runs in (never the one `project_path` names).
pub(super) fn own_root(server: &mut Server) -> Result<std::path::PathBuf, String> {
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
