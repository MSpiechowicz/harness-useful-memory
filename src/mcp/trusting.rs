//! Whether `script_save` trusts what it saves ([`Trusting`]), and holding a saved script back
//! from team trust ([`crate::store::TrustStore::hold_back`]) when it does not: a script an agent
//! saves while a save guard applies ([`super::guards::applies`]), and a held script the user
//! saves ([`super::user`]), need the user's approval before they run, also while every team
//! script is trusted. The version is held back by name before the save, so no save reaches team
//! trust unrecorded, and by its id once saved.

use super::Server;
use crate::store::{Script, ScriptDraft, script_sha256};

/// Why `script_save` trusts nothing while a save guard applies to its chat or repository.
const GUARDED: &str = "the user's save settings for Harness Memory apply to this chat or repository";

/// Whether a saved script is trusted on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trusting {
    /// As the module docs of [`super::calls_scripts`] say.
    AsSaved,
    /// Never, and held back: an agent saves it while a save guard applies.
    Guarded,
    /// Never, and held back: a held script the user saves needs their approval before it runs.
    Held,
}

impl Trusting {
    /// Why the script is not trusted, when it is held back.
    pub(super) fn held_back(self) -> Option<&'static str> {
        match self {
            Self::AsSaved => None,
            Self::Guarded => Some(GUARDED),
            Self::Held => Some("the user saved it without approving it"),
        }
    }
}

/// Holds `draft` back by name in the place the server saves scripts to, before it is saved.
pub(super) fn hold_back_draft(server: &mut Server, draft: &ScriptDraft) -> Result<(), String> {
    let place = server.place_key(server.scripts_place.writes());
    let sha256 = script_sha256(draft.language, &draft.body);

    server
        .trust
        .hold_back(&place, None, &draft.name, &sha256)
        .map_err(|error| {
            format!("nothing was saved: this machine could not keep the script waiting for approval ({error})")
        })
}

/// Holds back version `sha256` of the saved `script` by its id where it is kept.
pub(super) fn hold_back_saved(server: &mut Server, script: &Script, sha256: &str) -> Result<(), String> {
    let (side, raw) = server.spot(script.summary.id);
    let place = server.place_key(side);

    server
        .trust
        .hold_back(&place, Some(raw), &script.summary.name, sha256)
        .map_err(|error| {
            format!(
                "saved script {} {}, but this machine could not keep it waiting for approval: {error}",
                script.summary.id, script.summary.name
            )
        })
}
