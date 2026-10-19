//! Per-call limits. The number of notes and the database size are never capped: the agents keep
//! memory current by updating and forgetting notes (see the server instructions).

/// Title length, in characters.
pub const TITLE_MAX: usize = 120;

/// Body length, in characters.
pub const BODY_MAX: usize = 4000;

/// Tags per note.
pub const TAGS_MAX: usize = 8;

/// Tag length, in characters (`[a-z0-9_-]`).
pub const TAG_MAX: usize = 32;

/// Entity name length, in characters.
pub const ENTITY_MAX: usize = 80;

/// Relation label length, in characters.
pub const RELATION_MAX: usize = 40;

/// Entities linked by one `note_save`.
pub const ENTITIES_PER_NOTE: usize = 10;

/// Search text length, in characters.
pub const QUERY_MAX: usize = 256;

/// Words of the search text used in the full-text query.
pub const QUERY_TOKENS_MAX: usize = 16;

/// Search rows when the agent gives no limit.
pub const SEARCH_LIMIT_DEFAULT: usize = 8;

/// Most search rows.
pub const SEARCH_LIMIT_MAX: usize = 20;

/// Rows of one page of `notes list` and `scripts list` when none is asked for.
pub const LIST_PAGE_DEFAULT: usize = 50;

/// Most rows of one page of `notes list` and `scripts list`.
pub const LIST_PAGE_MAX: usize = 200;

/// Tokens in a search snippet (FTS5 `snippet()`).
pub const SNIPPET_TOKENS: usize = 24;

/// Snippet length, in characters.
pub const SNIPPET_MAX: usize = 240;

/// Similar notes listed after `note_save`.
pub const SIMILAR_MAX: usize = 3;

/// Ids per `note_get`.
pub const GET_IDS_MAX: usize = 5;

/// Deepest `entity_neighbors` walk.
pub const DEPTH_MAX: u8 = 2;

/// Relations listed by `entity_neighbors` when the agent gives no limit.
pub const NEIGHBORS_LIMIT_DEFAULT: usize = 20;

/// Most relations listed by `entity_neighbors`.
pub const NEIGHBORS_LIMIT_MAX: usize = 50;

/// Notes listed by `entity_neighbors`.
pub const NEIGHBOR_NOTES_MAX: usize = 10;

/// Team notes hidden on this computer per team database (`harness-memory notes hide`), and note
/// ids a list or a search leaves out ([`crate::store::NoteListQuery::exclude`]).
pub const HIDDEN_MAX: usize = 500;

/// Note ids one command takes in a list (`push --overwrite`, `notes discard-local`).
pub const IDS_MAX: usize = 100_000;

/// Size of the note ids read from standard input (`-`), in bytes.
pub const STDIN_IDS_BYTES_MAX: usize = 2 * 1024 * 1024;

/// Longest input line (one JSON-RPC message), in bytes.
pub const LINE_MAX: usize = 1024 * 1024;

/// Reply size of `note_search` and `entity_neighbors`, in bytes.
pub const LIST_REPLY_MAX: usize = 4 * 1024;

/// Reply size of `note_get`, in bytes.
pub const GET_REPLY_MAX: usize = 16 * 1024;

/// Script name length, in characters (`[a-z0-9][a-z0-9_-]*`).
pub const SCRIPT_NAME_MAX: usize = 64;

/// Script description length, in characters.
pub const SCRIPT_DESCRIPTION_MAX: usize = 500;

/// Script body size, in bytes.
pub const SCRIPT_BODY_MAX: usize = 64 * 1024;

/// Arguments per `script_run`.
pub const SCRIPT_ARGS_MAX: usize = 32;

/// Length of one `script_run` argument, in bytes.
pub const SCRIPT_ARG_MAX: usize = 4096;

/// Standard input of one `script_run`, in bytes.
pub const SCRIPT_STDIN_MAX: usize = 64 * 1024;

/// Seconds a script may run when it names no timeout.
pub const SCRIPT_TIMEOUT_DEFAULT: u64 = 60;

/// Longest script timeout, in seconds.
pub const SCRIPT_TIMEOUT_MAX: u64 = 600;

/// Script standard output kept in the reply, in bytes.
pub const SCRIPT_STDOUT_MAX: usize = 32 * 1024;

/// Script standard error kept in the reply, in bytes.
pub const SCRIPT_STDERR_MAX: usize = 8 * 1024;

/// Size of one team document body (a Harness Desktop hook or routine file), in bytes.
pub const TEAM_DOC_BODY_MAX: usize = 16 * 1024;

/// Length of a team document's author label, in characters.
pub const TEAM_DOC_AUTHOR_MAX: usize = 80;

/// Size of one team hook body (a Harness Desktop hook file), in bytes.
pub const HOOK_BODY_MAX: usize = TEAM_DOC_BODY_MAX;

/// Team hooks per scope: the global scope, and each repository.
pub const HOOKS_PER_SCOPE: usize = 64;

/// Length of a team hook's author label, in characters.
pub const HOOK_AUTHOR_MAX: usize = TEAM_DOC_AUTHOR_MAX;

/// Size of one team routine body (a Harness Desktop routine file), in bytes.
pub const ROUTINE_BODY_MAX: usize = TEAM_DOC_BODY_MAX;

/// Team routines per scope: the global scope, and each repository.
pub const ROUTINES_PER_SCOPE: usize = 64;
