//! Whether a held item still leads to the project the guard checked (see `held save` in
//! [`super`]). The item keeps the agent's `project_path` as sent, and a link in it can be
//! retargeted before the user approves, so a save from that path would reach another project.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::held::{HeldItem, HeldTool};
use crate::mcp::worktree::map;
use crate::project::ProjectResolver;
use crate::support::Env;
use crate::support::paths::validate_project_dir;

/// The refusal text when `item` no longer leads to the project it was held for: its
/// `project_path` is gone or is now in another project than the item's `project_root` (older
/// items: `repo_root`). A linked worktree counts as the repository it was made from, as in the
/// guards. `None` when it still does, for a save to the global scope (which does not depend on the
/// project) and for one held for a folder that is no project.
///
/// The scope is the one the save will have: the `scope` of `draft`, the user's edit, else the one
/// held, so a global item edited into a repository one is checked.
pub(super) fn moved(env: &Env, item: &HeldItem, draft: Option<&Map<String, Value>>) -> Option<String> {
    if saves_globally(item, draft) {
        return None;
    }

    // An item held for a folder that is no project (the home folder) compares nothing. Older
    // items have no `project_root` at all and are checked against their `repo_root`.
    let expected = match &item.project_root {
        Some(Some(root)) => root.as_str(),
        Some(None) => return None,
        None => item.repo_root.as_deref()?,
    };

    let Some(now) = project_now(env, item.project_path.as_deref()) else {
        return Some("the folder this save was held for is gone, so nothing was saved".to_owned());
    };

    // `expected` is canonical since it was held, so it is not resolved again: a root that is a
    // link now would lead to where the link points, and the same on both sides.
    let held = PathBuf::from(expected);

    if map(&now).path() != map(&held).path() || root_replaced(env, item, expected) {
        return Some("the folder this save was held for now leads to another project, so nothing was saved".to_owned());
    }

    None
}

/// True when the project root the item was held for (`expected`, canonical when it was held) is a
/// link now, or leads elsewhere than it did: it does not canonicalise to itself. It is asked
/// whatever the `project_path` is, a subfolder of the root included, since the root is compared
/// without following a link at it: a root swapped for a link to a worktree of another repository
/// that holds the same subfolder would map to that repository and read as the same project. A
/// subfolder that a link replaced counts only for where it leads (a worktree of the same
/// repository is the same project). Older items, which have no `project_root`, are not asked.
fn root_replaced(env: &Env, item: &HeldItem, expected: &str) -> bool {
    if !matches!(item.project_root, Some(Some(_))) {
        return false;
    }

    validate_project_dir(env, Path::new(expected)).is_ok_and(|now| now != Path::new(expected))
}

/// True when the save goes to the global scope: `draft` names it, or names no scope and the held
/// arguments do. A held `note_update` holds a `scope` only when the agent passed one, so one
/// whose note is global (`current`) counts too. A scope is read as the tools read one (blanks and
/// case ignored).
fn saves_globally(item: &HeldItem, draft: Option<&Map<String, Value>>) -> bool {
    let is_global = |scope: &Value| {
        scope
            .as_str()
            .is_some_and(|scope| scope.trim().eq_ignore_ascii_case("global"))
    };

    if let Some(scope) = draft.and_then(|draft| draft.get("scope")) {
        return is_global(scope);
    }

    if item.tool == HeldTool::NoteUpdate && is_global(&item.current["scope"]) {
        return true;
    }

    item.arguments.get("scope").is_some_and(is_global)
}

/// The canonical root of the project `raw` leads to now; the folder itself when it is no project
/// (the home folder). `None` when there is no such folder.
fn project_now(env: &Env, raw: Option<&str>) -> Option<PathBuf> {
    let raw = raw?;
    let folder = validate_project_dir(env, Path::new(raw)).ok()?;

    match ProjectResolver::new(env.clone(), None).resolve(Some(raw)) {
        Ok(Some(project)) => Some(project.root),
        _ => Some(folder),
    }
}
