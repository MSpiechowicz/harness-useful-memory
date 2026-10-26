//! What the script commands share ([`Opened`]: the stores, the trust and the view, opened like
//! the server), `scripts list` ([`list`]), and the commands for scripts read from the team
//! database: `scripts hide` and `scripts unhide` ([`hidden`]), `scripts discard-local`
//! ([`discard`]) and `scripts push` ([`push`]). [`crate::cli_trust`] dispatches them and documents
//! every script command.

pub(crate) mod discard;
pub(crate) mod hidden;
pub(crate) mod list;
pub(crate) mod push;
pub(crate) mod save;

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::backend::{Backend, Place, Secrets, Selection};
use crate::cli::{self, Repos};
use crate::mcp::{Approval, Shown, approval_of};
use crate::open::{open_hidden_scripts, open_scripts, open_trust};
use crate::store::{HiddenScripts, ScriptStore, ScriptSummary, SqliteStore, StoreError, TrustStore, View};
use crate::support::Env;

/// The script store, this machine's trust, the team scripts hidden on this computer (`None` when
/// scripts are not read from the team database) and the visible scripts, opened like the server
/// does.
pub(crate) struct Opened {
    pub scripts: Box<dyn ScriptStore>,
    pub trust: Box<dyn TrustStore>,
    pub hidden: Option<HiddenScripts>,
    pub view: View,
    pub env: Env,
    pub selection: Selection,
    pub secrets: Secrets,
}

impl Opened {
    /// Opens the stores for the scripts visible from `project` (else the working directory);
    /// `None`, with the reason printed, when the settings or the project are wrong.
    pub fn open(pick: &Repos) -> Option<Self> {
        let (env, selection) = cli::settings()?;
        let view = pick.view(&env)?;

        Some(Self {
            scripts: open_scripts(&env, &selection),
            trust: open_trust(&env),
            hidden: open_hidden_scripts(&env, &selection),
            view,
            secrets: selection.backend.secrets(),
            env,
            selection,
        })
    }

    /// Where the script the store shows as `id` is kept, and its id there.
    pub fn spot(&self, id: i64) -> (Place, i64) {
        let single = self.selection.scripts_place().writes();
        self.scripts.origin(id).spot(id, single)
    }

    /// How this machine may run `summary` as listed, as `script_list` decides it (a version that
    /// needs approval is remembered).
    pub fn approval(&mut self, summary: &ScriptSummary) -> Result<Approval, String> {
        let (side, raw) = self.spot(summary.id);
        let team_trusted = side == Place::Remote && self.selection.team_scripts_trusted();
        let key = self.selection.scripts_trust_key(side);

        approval_of(self.trust.as_mut(), team_trusted, &key, raw, summary, &summary.sha256)
    }

    /// Every visible script `query` keeps, with how this machine may run it, as `script_list`
    /// lists them.
    pub fn list(&mut self, query: Option<&str>) -> Result<Vec<(ScriptSummary, Approval)>, String> {
        let summaries = self
            .scripts
            .list_scripts(&self.view, query)
            .map_err(|error| error.to_string())?;

        summaries
            .into_iter()
            .map(|summary| {
                let approval = self.approval(&summary)?;
                Ok((summary, approval))
            })
            .collect()
    }

    /// The `script_list` object of `summary`, hidden on this computer when `hidden`.
    pub fn json(&self, summary: &ScriptSummary, approval: Approval, hidden: bool) -> Value {
        Shown {
            summary,
            sha256: &summary.sha256,
            place: self.spot(summary.id).0,
            read_only: self.scripts.origin(summary.id).is_read_only(),
            hidden,
            approval,
        }
        .json()
    }

    /// Adds `hiddenCount` (the team scripts of the view hidden on this computer) and `localCounts`
    /// (the scripts in the local file per scope, every repository's) to the list `shown` when a
    /// team database is set.
    pub fn add_team_counts(&mut self, shown: &mut Value) -> Result<(), StoreError> {
        if self.selection.backend == Backend::Local {
            return Ok(());
        }

        let (repo, global) = local_counts(&self.env)?;
        shown["hiddenCount"] = json!(hidden::count(self)?);
        shown["localCounts"] = json!({ "repo": repo, "global": global });
        Ok(())
    }

    /// Prints `message` without the database credentials; exit 1.
    pub fn failed(&self, message: &str) -> ExitCode {
        cli::failed(&self.secrets.scrub(message))
    }
}

/// The scripts in the local file per scope, every repository's: `(repository, global)`; none
/// without the file (which is not created).
fn local_counts(env: &Env) -> Result<(usize, usize), StoreError> {
    let Some(path) = crate::database_path(env).filter(|path| path.is_file()) else {
        return Ok((0, 0));
    };

    SqliteStore::open(&path)?.script_scope_counts()
}

/// `harness-memory: script <id> not found`; exit 1.
pub(crate) fn not_found(id: i64) -> ExitCode {
    eprintln!("harness-memory: script {id} not found");
    ExitCode::FAILURE
}
