//! `harness-memory`: a project memory MCP server for any coding agent harness.
//!
//! Agents remember outcomes, problems, fixes, locations, optimisations and decisions as short
//! notes, search them with full-text ranking, and relate entities in a small graph. Notes belong
//! to one repository (identified by its `origin` remote, see [`project`]) or to the global scope.
//!
//! - [`store`]: the [`store::MemoryStore`] contract, its SQLite backend (WAL, FTS5) and the team
//!   database backends.
//! - [`backend`]: which store to use (the local file, or a team database from the environment or
//!   the config file).
//! - [`push`] and [`push_scripts`]: copy the local notes and scripts into a team database
//!   ([`cli_push`] reads their options).
//! - [`mcp`]: the stdio JSON-RPC server and its tools.
//! - [`limits`]: per-call limits (the amount of memory itself is never capped).
//! - [`open`]: the script and trust stores the server opens on first use.
//! - [`cli_trust`]: the `scripts` and `trust` commands, where a person lists, hides, copies and
//!   approves scripts.
//! - [`cli_notes`]: the `notes list`, `notes get` and `notes delete` commands, where a person
//!   browses and deletes notes.
//! - [`cli_team_docs`]: the `hooks` and `routines` commands, through which Harness Desktop shares
//!   hooks and routines with the team.
//! - [`location`]: where the database lives, and the one-time move from Harness Desktop's folder.
//! - [`support`]: the environment, git runner, secret masking and text checks the rest uses.
//!
//! The local database lives in the per-machine data folder ([`location::database_path`]), never in
//! a repository; everything stored passes through [`support::redact::redact_text`] first.

pub mod backend;
mod cli;
mod cli_discard;
pub mod cli_notes;
pub mod cli_push;
mod cli_scripts;
pub mod cli_team_docs;
pub mod cli_trust;
pub mod limits;
pub mod location;
pub mod mcp;
pub mod open;
pub mod project;
pub mod push;
pub mod push_scripts;
pub mod store;
pub mod support;

pub use location::database_path;
pub use mcp::{Server, serve};
