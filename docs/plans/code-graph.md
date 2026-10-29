# Code graph in harness-memory (approved 2026-10-29)

Approved plan for the code index (harness-memory 2.10.0), the Graphify retirement in harness-useful-skills (0.5.0)
and the Harness Desktop changes. The reference implementation is a throwaway prototype (see "Prototype"), measured
with `claude -p` A/B runs: input tokens 106k→22k, 154k→61k, 93k→26k on harness-useful-desktop; 37k→16k,
69k→72k, 41k→17k on track-things-frontend; 29k→16k, 51k→60k, 12k→18k on fzf.

## Settled decisions

- Tools live in harness-memory: `code_map`, `code_symbol`, `code_impact`, `code_path`. Every one takes a required
  `project_path` resolved by the existing `ProjectResolver::resolve`.
- Storage: a local `code.sqlite3` next to `memory.sqlite3` (`database_path(env).with_file_name("code.sqlite3")`,
  so `HARNESS_MEMORY_DIR` is respected), 0600 file / 0700 folder. Never written to a team DB. Many repos in one
  file, keyed by `repo_id`.
- Freshness is refresh-on-read: each `code_*` call stat-checks files (mtime + size), re-parses only changed files,
  and rebuilds the in-memory graph only when the repo's `generation` moved. No watcher, no commit hook, no Rebuild
  button.
- Languages v1: Rust; TS/JS/JSX/TSX/MJS/CJS; `.svelte`/`.vue`/`.astro` through ONE generic single-file-component
  handler; Go (through its `tags.scm`); Python. No framework-specific code (no Tauri, React, Vue code).
  Cross-language links come only from the generic rule "a string literal names a function/component defined in
  another file" (weak `(by name)` link: must cross languages or hit a function with no direct callers; ties go
  to entry points; at most 3 candidates).
- The code index is ON by default (missing `code.toml` = on). Off hides the tools and keeps the file.
  Env override `HARNESS_MEMORY_CODE_INDEX=0|1` for tests and the A/B.
- Only git work trees are indexed (decision changed 2026-10-29, after the full-scope review: a prompt-injected agent
  could read any folder's constants through the tools). `project_path` must resolve to a folder whose `.git` is a real
  folder or a gitfile, else `The code index only works inside a git repository.` Caps: 200k walked entries, 20k
  indexed files, 1 MB per file.
- Speed-ups, each gated by the benchmark (golden edges must be a superset, A/B input tokens must not regress):
  (a) receiver types from written annotations so `obj.method()` resolves,
  (b) constant / enum-member mention refs so event producers and consumers link,
  (c) `code_path(from, to)`,
  (d) short tool descriptions (≈200 chars) and a smaller default map budget (≈700 tokens, tuned by the A/B).
- Non-goals: LSP/SCIP mode, watchers, team/remote code index, more languages, fallback graph without
  harness-memory, framework-specific code.

## Prototype

`/tmp/claude-1000/-home-frogius-Projekty-harness-useful-desktop/e4fcd5f1-c568-4ce6-986c-fb1e87860c77/scratchpad/codegraph/src/{extract,store,graph,main}.rs`
(throwaway; may disappear after the session). Port the design, not the code quality. Known prototype bugs that
must NOT be ported:

1. `extract.rs::compiled()` uses one `static OnceLock` for every `TagLang` (Python would get Go's query). Use one
   compiled query per language.
2. `extract_sfc` blanks script blocks char-by-char; multi-byte characters shift byte offsets and can panic. Blank
   byte-for-byte (one ASCII space per UTF-8 byte, newlines kept).
3. Files are read whole before the 1 MB check. Check size first, read with `take(limit + 1)`.
4. Changes to `Cargo.toml` / `go.mod` / `tsconfig.json` (and Python roots) rebuild `packages` but never bump
   `generation`, and these files are re-read on every call. Stamp them like source files; bump `generation`
   when `packages_hash` changes.
5. `DefaultHasher` is not stable across Rust versions: use `sha2` (already a dependency).
6. The mtime stamp uses `{:?}` (platform specific): store `mtime_ns` and `size` as integers.
7. `unwrap`, `fidx[&fid]`, `file_stem().unwrap()` everywhere: the MCP server must never panic on input.
8. Stored relative paths keep Windows `\`: always store `/`-separated paths.
9. `ts_aliases` ignores `extends` and `jsconfig.json`: follow `extends` (relative, bounded depth) and read
   `jsconfig.json` too.

## harness-memory design

New module `src/code/`:

- `mod.rs`: `CodeIndex { conn, cache (LRU, max 3 roots) }`, `open(env)`, `graph(&mut self, root) -> (&Graph, Refresh)`.
- `switch.rs`: `code.toml` (`enabled = bool`) in the data folder, read/written like `embed::enabled`/`set_enabled`
  (`support::private::read_private`, `write_atomic`); env override.
- `walk.rs`: bounded walk + safe read (see Security).
- `store.rs`: schema + incremental writes.
- `builds.rs`: `toast_kind()` rule, `builds` rows, status files.
- `extract/{mod,rust,ts,sfc,tags,go,python}.rs`; `graph/{mod,ts,rust,go,python,infer,rank,render}.rs`.
- `src/mcp/tools_code.rs` (definitions), `src/mcp/calls_code.rs` (dispatch), `src/cli_code.rs`.

Wiring: `lib.rs` (`pub mod code; pub mod cli_code;`), `main.rs` (`FEATURES` gains `"code-index"`,
`code` subcommand, `USAGE`), `mcp/mod.rs` (`Server.code: Option<CodeIndex>` opened lazily, `code_on` read at
initialize), `mcp/calls.rs` (first line of `call()`: `code_*` goes to `calls_code::call` BEFORE the English gate,
save guards, saving notice and `store.ready()` so code calls never touch the team DB nor print the offline notice),
`mcp/tools.rs` (`definitions(english, code_on)` and one instruction line when on), `mcp/protocol.rs`.
`calls_code::call` re-checks the switch on every call; off answers
"The code index is off. Turn it on in Harness Desktop → Settings → Memory."

CLI: `harness-memory code status|enable|disable [--json]` and `code hint --project PATH --query NAME [--json]` (see Deviations).

### Tools

| Tool | Arguments (bold = required) | Reply |
|---|---|---|
| `code_map` | **project_path**, `focus` (path prefix), `budget` 200..4000 (default ≈700) | ranked files with up to 6 signatures each, tests excluded |
| `code_symbol` | **project_path**, **symbol** (`name`, `Type::name`, `mod::name`, `path/frag::name`), `limit` 1..50 (20) | `kind label path:line-end`, signature, `uses:`, `used by N site(s) + M test(s)`, `(by name)` and `?` markers, up to 12 "Similar:" names when nothing matches |
| `code_impact` | **project_path**, **symbol**, `depth` 1..4 (2) | `dN path  [caller → callee]`, tests summarised per file, ambiguous edges counted |
| `code_path` | **project_path**, **from**, **to**, `max_depth` 1..10 (6) | `from → to: N hops`, one line per hop `label  path:line` with `(by name)` on string links and under it `at path:line | source line`; ends are symbols, files or strings (see Deviations); tries `to → from` ("reverse") when no forward chain; else "no chain within N hops" |

Diagnostics (refresh timings, token estimates) go to stderr only.

### Schema (`code.sqlite3`, `PRAGMA user_version = 3` in the first draft, 8 now, see Deviations; an older version = drop all and rebuild, counts as a full build; a newer one is refused untouched; WAL; `busy_timeout` 60 s, 250 ms per attempt at the write lock)

```
repos(id PK, root TEXT UNIQUE, key TEXT, generation INT, packages_hash TEXT, indexed_at INT, used_at INT)
files(id PK, repo_id FK CASCADE, path TEXT ('/'-separated, relative), hash TEXT (sha256 hex prefix), mtime_ns INT, size INT, lang TEXT, UNIQUE(repo_id, path))
symbols(id PK, repo_id, file_id FK CASCADE, name, kind, parent, line, end_line, sig, ret TEXT (written return type), test INT)
refs(id PK, repo_id, file_id FK CASCADE, from_symbol FK CASCADE, name, kind (call|mcall|macro|type|new|component|str|use|fnref), qual, recv TEXT (receiver variable), line)
binds(repo_id, file_id FK CASCADE, scope_symbol INT NULL, name TEXT (var, param, self.field), type TEXT, kind TEXT (var|param|field|let))
imports(repo_id, file_id FK CASCADE, local, orig, source)
packages(repo_id, kind (crate|gomod|alias|pyroot), name, dir)
builds(id PK, repo_id, kind (full|bulk), status (running|ok|error), started_at, finished_at, files_total, files_done, error TEXT)
indexes: symbols(repo_id,name), symbols(file_id), refs(file_id), binds(file_id), imports(file_id), packages(repo_id)
```

### Refresh

1. Walk without a lock and compare stamps; nothing changed (packages included) → cached graph.
2. Otherwise `BEGIN IMMEDIATE`, reload stamps inside the transaction, walk again, classify with `toast_kind`, parse,
   write. For full/bulk builds commit every 500 files (so other sessions only wait briefly), bump `generation` at
   the end. A second process blocks on `busy_timeout`, then finds nothing changed: sessions share one build and
   only the lock holder creates the `builds` row. A session that gets the lock while another session's `running`
   row for the same repository is live joins it: it commits in batches like the owner, but creates no row and
   writes no status file (the owner's stand for the shared work).
3. Check the existing `Stop` and a 120 s time budget per call between files (`pace.rs`): a cancelled or too slow
   (`indexing took too long`) request rolls back the batch in flight, keeps what was committed, and the next call goes
   on from there.
4. A build lets go of the write lock for 120 ms once it held it 1.5 s, so a waiting session gets a turn. A session
   that cannot get the lock within 2 s answers from the last committed graph (`Refresh.stale`) unless nothing is
   committed yet, and then keeps waiting within its budget.
5. With the lock held, `running` build rows older than 5 min are ended as errors (their process died) and the
   repository's `building` status file that nobody touched for 60 s turns into `error`; a build start also drops the
   repositories not used for 30 days (`repos.used_at`, written at most once a day) and the finished `builds` rows
   older than 30 days. A build that is cancelled or runs past its budget ends its row, and the status file, as
   `error` (the Desktop decides what to show for a cancel).

### Resolution order (from the prototype)

Same file for unqualified names only → qualifier (`Type::method` via impl/receiver parent, module paths via crate
names from `Cargo.toml`, Go package via `go.mod` + directory, TS namespace import) → explicit import (Rust `use`,
TS imports with tsconfig `paths`, Python imports) → same package → non-test definitions → drop when more than 8
candidates. External crates/packages stay unresolved.

TypeScript details: `ns.fn()` on `import * as ns` narrows to the exports of that module (functions, arrow
functions, classes), following `export ... from` barrels (each module once per search, at most 256 modules and
4 links deep, every visit paid from the scan budget, duplicate `export * from` lines kept once). Aliases come from
every `tsconfig*.json` / `jsconfig*.json` (following `extends` and project `references`), and a SvelteKit app
(`svelte.config.*`) gets `$lib` -> `src/lib` unless its configs define it; where several configs define the same
prefix for their own folders, the first that leads to a file wins. A specifier that is neither relative nor a known
alias is an npm package and stays external, unless the repository defines the imported name: then the name
decides, like any name in reach (`?` when several fit, and never better than `?` for a bare package name such as
`date-fns`). Method calls (`x.foo()`) only resolve to methods. Types only
to type kinds. Macros only to macros. Inline `#[cfg(test)]`/`#[test]` symbols count as tests. `impact` output must
show `caller → callee` per entry (agents invent chains otherwise).

### Speed-up details

- (a) Extractors emit `binds` from written annotations: TS `const x: T`, params, `new T()`, return types into
  `symbols.ret`; Rust struct fields as `self.f`, `let x: T`, `-> T`, `self` → impl type; Go receivers, params,
  `x := NewT()` / `&T{}` through `ret`; Python param/variable annotations, `x = T()` when `T` is a class, `self` →
  class. `mcall` refs carry `recv`; `graph/infer.rs` maps recv → type (scope → file → fields) and uses it as `qual`.
- (b) Enum variants/members and constants become symbols (Rust `enum_variant` with the enum as parent, TS enum
  members, Go `const_spec`, Python `Enum` members and module constants). Identifiers in argument lists, case/match
  arms, comparisons and assignment right-hand sides become `use` refs only when they look like constants
  (`^[A-Z][A-Za-z0-9_]+$`); they resolve only to const/static/enum-member kinds; unresolved `use` refs are dropped
  at load.
- Python resolver: module path → file through `pyroot` packages (repo root, `src/`, directories above a top-level
  `__init__.py` chain); relative imports (`from . import x`, `from ..m import y`). Tests: `test_*.py`, `*_test.py`,
  `tests/`, `conftest.py`.

### Security

- Root from `ProjectResolver` (canonical, never home or `/`). The code tools also refuse a folder that holds the
  home folder (`/home`, `/Users`) and system folders (`/etc`, `/usr`, `/var`, `C:\Windows`, ...).
- `ignore::WalkBuilder` with all of the crate's own filters off (`standard_filters(false)`): `follow_links(false)`,
  `same_file_system(true)`, files over 1 MB left out by the walk, fixed exclusions (`node_modules`, `target`, `.venv`,
  `venv`, `__pycache__`, `vendor` anywhere; `dist` and `build` only at the top or next to a project manifest, since
  `src/node/build/` is source; hidden folders are walked unless they hold tool state or caches: `.git`, `.hg`,
  `.svn`, `.svelte-kit`, `.next`, `.nuxt`, `.output`, `.cache`, `.turbo`, `.venv`, `.tox`, `.mypy_cache`,
  `.pytest_cache`, `.idea`, `.vscode`, `.gradle`, `.terraform`, `.yarn`, `.pnpm-store`, `.direnv` and a few more
  generated ones; hidden files are skipped); caps 200k walked entries / 20k indexed files → "repository too large to index".
- Ignore rules are read by `walk.rs`, never by the crate (it followed links and had no size cap: `.gitignore ->
  /dev/zero` aborted the server, a `.git` file pointed it at other folders). `.ignore`, `.gitignore` and
  `.git/info/exclude` (only of a real `.git` folder) are small regular files (≤ 256 KiB each and in total per walk,
  lines over 1 KiB or with more than 64 wildcards dropped, ≤ 20k patterns each, 100k per walk) opened without
  following a link and inside the root. The server reads them on every call in its own process, so the caps are small
  (the first caps, 1 MB / 4 MB / 4 KiB, cost the server about 580 MB at its peak), and the compiled rules are kept
  between calls per (path, modification time, size), at most 256 files and an estimate of 8 MiB of memory (a hundred bytes per byte of text, 512 per pattern; a file over it is compiled again on every call).
  They rank as in ripgrep: every `.ignore` on the way up (closest first) before any `.gitignore`, those before the
  exclude file, whatever the depth (a `.ignore` at the root can bring back a file that a `.gitignore` below hides).
  Negation, folder-only patterns, an ignored folder is not entered, a rule can bring a hidden file or a tool folder back. A refused
  ignore file (a link, over a cap, past the total, or with a dropped line) counts as skipped. Only the rules of the
  folders on the way to the current folder stay in memory (the walk is depth first), and the walk, ignore files
  included, is held to the 120 s time budget ("indexing took too long").
- Read: open without following a link, `fstat` the handle, regular file with one name ≤ 1 MB; on Unix compare dev+ino
  with the walk's lstat (swap) and check the descriptor's real path is under the root (`/proc/self/fd` on Linux,
  the canonical path elsewhere); a file with more than one hard link is left out; `take(limit + 1)`; non-UTF-8 skipped
  and counted; permission errors skipped, never fatal.
- Graph bounds: a file contributes at most 20k definitions, 100k references, 20k imports and 50k bindings (the rest is
  dropped and counted as "cut at the size caps"); a step that settles a reference names at most 64 definitions, a name
  with more than 1024 definitions is only looked up by file and owner, all scans share a budget, the graph keeps at
  most 4M edges, 500k definitions and 1M references (past which: "repository too large to index"; a graph costs about
  500 bytes per reference, measured on a repository of 2,600 files with 250k references, so a full one is about half
  a gigabyte, and a `use` reference no constant can resolve to is not loaded and does not count). The load polls the
  time budget. A list of more than 1024 definitions is ambiguous for the narrow steps too (it is not cut to its first
  1024).
- The same caps hold while writing: the repository row counts its definitions, references and imports, and a refresh
  that would pass a cap fails with "repository too large to index" before it writes (earlier batches stay, so the
  file never holds more than the caps). It marks the repository with a digest of the files it saw
  (`repos.too_large`): while the files are the same, later calls fail at once without reading. A change (files
  deleted to fit, for one) is a new attempt, and a complete refresh takes the mark off.
- A reply served from the last committed state while another session still builds starts with a note saying the
  results may be incomplete. A dead `running` build row of any repository is ended as an error, and the status file of
  that repository stops saying `building`.
- Stored paths relative, `/`-separated; nothing outside the root is read; errors never echo file contents.

### Worker process (added 2026-10-29, after the fourth review round)

Every review round found another way for a hostile repository to exhaust the memory or time of the MCP server
(ignore files, quadratic and exponential resolution, string amplification, deep recursion). The point fixes stay, and
a structural backstop makes any future shape fail safely: the server never aborts, hangs or runs out of memory because
of repository content.

- `harness-memory code-worker` (hidden: not in the usage line or `features`; `src/code/worker.rs`) reads one JSON
  request line on stdin (`{root, budget_ms}`), clamps `budget_ms` to the server's 120 s budget, refuses a root the
  server would refuse (not a git work tree, broad, hidden in home), applies its own limits (also `RLIMIT_CORE` 0 and, on
  Linux, `PR_SET_DUMPABLE` 0, so the repository's text is in no core dump; SIGTERM and SIGINT unblocked, which the
  server's threads block and a child inherits), runs the existing refresh with the
  `StatusFile` observer, then loads the whole graph and ranks it once (`Graph::trial`, the resolution and the PageRank
  are the memory-heavy stages), records `repos.validated_generation = generation` and prints `{ok, generation, counts,
  ...}` or `{ok:false, error}`. Exit 0 or 1.
- Limits: memory by `RLIMIT_DATA` on Linux (heap and mappings since kernel 4.7), 2 GiB by default
  (`HARNESS_MEMORY_CODE_WORKER_MEMORY_MB`, clamped to 256..65536), set through the `rlimit` crate (MIT, unix only).
  The worker gets `MIMALLOC_ARENA_RESERVE=65536`: mimalloc otherwise reserves a gigabyte of address space at a time,
  which the limit counts, and nothing runs below about 1.1 GiB. macOS: `RLIMIT_AS` is tried and its failure ignored
  (the kernel does not enforce it reliably). Windows: not limited (a Job Object needs unsafe code or a dependency not
  approved). Time: the server kills the process at the call's budget plus a 12 s grace (longer than the 5 s the worker may wait for the write lock to record its own failure; `Child::kill`, every OS), and on Unix
  `RLIMIT_CPU` ends it ten seconds of processor time past the budget. The worker dies with its server: the server keeps
  the worker's stdin pipe open and a worker thread blocks reading it, so EOF stops the worker on every operating system
  (the flag is polled, and the process exits five seconds later if nothing polled), replacing the polling of
  `getppid()`; `PR_SET_PDEATHSIG` on Linux as well.
- Server (`src/code/supervise.rs`, `CodeIndex::supervised`): the lock-free stamp check stays. Nothing changed and
  `validated_generation == generation`: load or serve the cached graph in process, no spawn. Otherwise spawn
  `current_exe() code-worker` (the server's `Env` variables except the team database ones, `HOME`, stdin request, stdout
  report, stderr forwarded line by line with a prefix, cut to 1 KiB per line and 256 KiB in all), wait with the
  deadline, and kill it on the deadline or on the request's `Stop` (a SIGTERM to the server is a `Stop`). The graph is
  loaded in process only inside a snapshot in which `validated_generation == generation`; if another session wrote in
  between, a worker is asked again (three tries, then "The code index kept changing while it was read. Try again.").
- A worker that dies by SIGABRT, SIGSEGV, SIGBUS, SIGILL, SIGFPE or SIGXCPU, exits non-zero without a report, panics,
  runs out of SQLite memory or is killed past its deadline is answered `This repository could not be indexed within
  the code index limits.`. The repository is marked failed (`repos.failed`) with the digest of the files the server
  walked, the memory limit, the harness-memory version and the time: later calls fail at once, without a spawn, only
  while all of them are the same and the mark is under 24 hours old. A complete refresh or a validated generation
  takes the mark off, and an unchanged index at a validated generation is served whatever the mark says. A worker
  that something else ended (SIGKILL the server did not send: the system's out of memory killer, the user; SIGTERM) is
  answered `The code index worker was stopped before it finished. Try again.` with no mark, and a worker that was
  ended after it recorded the validated generation, or when the call was cancelled, is served or cancelled, never
  marked. The `builds` rows the worker started (they
  carry its pid, `builds.pid`) and their status file end as errors. A worker that says `{ok:false, error}` itself
  (too large, cancelled, too slow, database busy) is a normal error and passes through unmarked. A cancel kills the
  worker and ends its build as an error with `cancelled`, without a mark.
- Concurrency is unchanged: workers meet at `BEGIN IMMEDIATE`; one that finds the lock held for long answers from the
  committed state (`stale`), validates that generation (or finds it validated already) and the server prefixes the
  note as before. A worker that only joined a running build writes no status file of its own, but when it finishes
  the work it turns the owner's `building` file into `ready` (the owner's worker may have died on the way). A
  generation that is already validated is not tried again. A `running` build row whose pid is gone (`/proc/<pid>` on
  Linux, elsewhere only the 300 s rule) is ended at once, so nobody joins a dead build; the status file says
  `building` only after the row is committed.
- `Store::open` takes no write lock for a file of the current schema version, so a worker starts at once while
  another session builds.
- The graphs in memory: at most 3 roots and at most about 1.5 GB in all by `Graph::estimated_bytes` (500 bytes per
  reference, 300 per definition, 200 per file; a graph at the caps is about 700 MB), least recently used dropped
  first, the newest always kept.
- `HARNESS_MEMORY_CODE_INPROCESS=1` (tests only, honoured by debug builds only) runs the old in-process path. The unit tests of `CodeIndex` need
  nothing: a `CodeIndex` indexes in process until `set_worker`. Debug builds only (`cfg!(debug_assertions)`):
  `HARNESS_MEMORY_CODE_WORKER_TEST=alloc|spin|abort|kill|late-abort` makes the worker allocate past its limit, spin,
  abort, SIGKILL itself right after the first file of its build was read (or after the refresh), or abort after it
  validated the generation, and write its pid to `code-worker.pid`; `HARNESS_MEMORY_CODE_GRACE_MS` shortens the grace;
  `HARNESS_MEMORY_CODE_BUDGET_MS` shortens the call budget; `HARNESS_MEMORY_CODE_WORKER_BIN` names the executable a
  test that drives a `Server` from a test executable starts as the worker. Tests: `tests/code_worker.rs`.

### Deviations from the first draft

- Schema version 11 instead of 3 (`PRAGMA user_version`; an older file is dropped and rebuilt). 2: receiver types
  (`binds`, `symbols.ret`, `refs.recv`); 3: `repos.used_at`; 4: the row totals and `too_large` on `repos`; 5: bindings
  in the repository totals; 6: `repos.validated_generation`, `repos.failed` and `builds.pid` for the worker process;
  7: no string literal value in a value signature; 8: no string literal content in any signature, no `str` reference
  that holds a credential format or has the digits of a random key, and `repos.validated_bytes` (the size of the
  graph a worker validated); 9: string literals in type positions (TS/JS and Python) are shown again, values stay
  blanked; 10: the literal scan follows comments, parenthesised return types, Python lambda parameters and a `/` that may open a regular expression; 11: `prop` references (a function kept in a property of an object literal). The planned `refs(repo_id, name)` index was dropped.
- The walk lists folders itself (`std::fs::read_dir`, one entry at a time) instead of through the `ignore` crate's
  walker, whose sorting reads a whole folder, with full paths, before it yields an entry: the caps count what is read.
  A folder holds at most 50,000 entries (more is `repository too large to index`), the walk is at most 128 folders deep
  and a path at most 1,024 bytes below the root (deeper and longer entries are skipped and counted), the paths of all the files kept at most 16 MiB (more is `repository too large to index`; the walk holds the path once per file, `Entry` has no absolute path and `read_text` joins the root), and the files kept are sorted by
  path at the end, which is the order the library walk gave. The same-file-system check compares device numbers
  (Unix only: a Windows volume mount is a reparse point, which is not entered). The ignore files of at most 64
  folders on one path count (matching an entry costs one match per file of the chain).
- `estimated_bytes` of a graph counts the text of names, signatures, parents, qualifiers and receivers, and a worker
  records `max(estimate, resident memory growth of the load)` with the validated generation. The server refuses a
  graph recorded as over 700 MB (`The code graph of this repository is too large to serve.`) without loading it.
  Refusing (and not loading, answering and dropping) because the answer needs the graph in this process anyway.
- The processor limit of a worker has a hard limit five seconds above the soft one (the soft one sends SIGXCPU, which
  names the repository; a hard limit equal to it sends SIGKILL). A SIGKILL of a worker that had used the processor time
  it was allowed (`/proc/<pid>/stat`, Linux, sampled while the server waits) is blamed on the repository too, so a user's
  own `ulimit -t` as hard limit still marks it.
- Strings in signatures are elided for every definition, not only constants (see README), except short literals in a
  type position of TypeScript, JavaScript and Python (`type X = "a" | "b"`, `m: "x"`, `Literal["a"]`): type literals
  are visible, values hidden (lexical scan in `src/code/extract/literals/typed.rs`, unclear positions are blanked). Unresolved `str` references
  stay in `code.sqlite3` (a definition added later must link to a file nobody changed), but not in the graph
  (`Graph::load` drops `str` references to a name nothing defines), and a string that holds a credential format
  (`redact::has_credentials`), is over 64 characters or has more than two digits is not a code name, so it is not stored.
- The ignore file caps are the same in the server and in the worker (256 KiB per walk, lines of 1 KiB with at most 64
  wildcards), not larger in the worker: both walk the repository, and two readings of one ignore file would make the
  server see another file set than the worker indexed (a worker spawned on every call). The failed and too-large
  marks hold the digest of the walk, so they cannot be checked before the walk; the compiled-rules cache and the small
  caps bound what that walk costs.
- Windows: std offers no file id or link count without an unstable feature (`windows_by_handle`), and the crate forbids
  `unsafe` (so no call to `GetFileInformationByHandle` or `GetFinalPathNameByHandleW`). On Windows a hard link is
  therefore not noticed, a file swapped between the walk and the read is told only by its size and modification time,
  and the "inside the root" check canonicalizes the path instead of the open handle. A link or a junction is still
  opened as itself (`FILE_FLAG_OPEN_REPARSE_POINT`) and refused. On Unix a link count of 0 counts as unknown, not safe.
  A full local cross-check needs a MinGW C compiler (bundled SQLite, tree-sitter), so only the Windows-only helpers
  were type-checked on their own.
- Symbol kinds beyond the list above: `member` (a function-valued property of an interface or object literal) and
  `enum_member`; reference kind `fnref` (a function passed as a value: it only means one of the same file or an
  imported one).
- `dist` and `build` are excluded only at the top of the repository or next to a project manifest, not everywhere.
- `code_impact` lists where the symbol is defined as `d0 path  [defined here: ...]`, not counted as an affected file;
  `code_path` from a symbol to itself is `0 hops`.
- The code tools answer `the code index was written by a newer harness-memory` for a file of a newer schema instead of
  rebuilding it.

## Build toast (status file for Harness Desktop)

- `toast_kind(prior_files, changed, removed) -> Option<BuildKind>`: `Full` when the repo has no indexed files
  (first index, schema reset, dropped); `Bulk` when `changed + removed >= 200`; otherwise `None` (no `builds`
  row, no file). Decided inside the `BEGIN IMMEDIATE` transaction after the second walk.
- Status file `<data dir>/code-builds/<sha256(root)[..16]>.json`, 0600, `write_atomic`:
  `{"version":1,"buildId":42,"root":"…","repoName":"<last path component>","kind":"full","phase":"building",
  "filesTotal":N,"filesDone":n,"startedAt":…,"finishedAt":null,"error":null}`. Written at start, progress at most
  every 500 ms, then `ready` / `error`. Finished files older than 10 min are deleted on the next build. Off = no
  index, no file.
- Desktop: unseen `buildId` + `building` → one toast "Building code graph for {repo}…" ("{done} of {total} files",
  no auto-dismiss); same `buildId` → `toasts.update`; `ready` → "Code graph ready · {n} files", success,
  auto-dismiss 4 s; `error` → "Code graph could not be built" + reason. A new build for the same root within
  10 min of the last shown toast is suppressed (its `ready` ignored too). A `building` file untouched for 60 s is
  stale and dismissed.

## Harness Useful Skills 0.5.0 (Graphify retirement)

- Create `memory-paths.js` (`workspaceMemoryPaths`, trimmed `prepareStorage`, `existingDirectory`; no
  generations/runtime) and `graphify-retirement.js` (`retireGraphifyData(profile)`, `migrateStageFiles(dir)`).
- Delete `graphify.js`, `graph-*.js`, `graph-test.js`, `dependencies.js`, `dependency-policy.js`,
  `dependency-transport.js`, `setup-lock.js`, `setup-runner.js`, `dependencies/`, `licenses/graphify/`, the
  Graphify entry in `upstreams.lock.json`, Graphify tests (port the `workspaceMemoryPaths` boundary cases into
  `test/memory-paths.test.js`).
- `memory.js` action enum `status|search|save`, status reflects native memory only; `mcp-memory-server.js` drops
  `graph_build`/`graph_query`; `local-facts.js` imports from `memory-paths.js`; doctor keeps the Facts step and
  adds an informational "Code graph" line probing `harness-memory features`.
- Stage `graphify-memory` → `code-graph` (label "Code graph") in `workflow-settings.js` (reading falls back to
  `graphify-memory.json`), `workflow-context.js`, `command-core.js` (legacy aliases accepted), `extension.js`,
  `claude/settings.js` (`CODE_GRAPH`, fallback `GRAPHIFY_MEMORY`), `hooks/register.ts`,
  `.claude-plugin/plugin.json` (`userConfig.code_graph`; keep `graphify_memory` declared for one release, marked
  deprecated), opencode settings.
- Skill text (`skills/us-manage-memory`, `us-run-workflow`, `us-research/references/scout-brief.md`,
  `us-use-harness-memory`): `code_map` first in an unfamiliar area, `code_symbol` instead of grep, `code_impact`
  before signature changes, `code_path` for "how does A reach B"; without harness-memory, no code graph.
- Render scripts: `replaceAll('code-graph','code_graph')`. Regenerate `claude/skills`, `codex/skills`,
  `opencode/skills` and `plugins/*` with the scripts, never by hand.
- `retireGraphifyData` touches fixed names only: `<profile>/harness-useful-skills/dependencies` if lstat says a
  real directory (not a symlink), owned by the current uid, parent realpath matches; in `memory/<64-hex>/` only
  `generations/`, `runtime/`, `current.json`; `rmdir` the hash dir only if empty; never follows links, never
  throws, idempotent; facts survive.

## Harness Desktop

- `skills_config/workflow.rs`: `STAGES[5] = "code-graph"`, fallback to `graphify-memory.json` when
  `code-graph.json` is missing; snapshots, `claude_skills_config.rs`, mock fixtures; a legacy-only test.
- `ecosystem/memory_features.rs`: `CODE_INDEX = "code-index"`; new `ecosystem/memory_code.rs` (`code status|enable|disable --json`,
  `CodeBuildWatcher` like `RequestWatcher`), `model/memory_code.rs` (ts-rs), fake_agent `code` subcommand.
- `src-tauri/src/commands/memory_code.rs`: `memory_code_index_get|set`, `memory_code_builds_listen`; the watcher
  rides the existing 500 ms memory-requests tick.
- UI: `toasts.update(id, patch)`, `stores/memoryCode.svelte.ts` with a pure tested `decide()`,
  `features/settings/CodeIndexRow.svelte` after `MemorySearchRows` (shown only when supported), i18n in 6 locales.
  Copy: title "Code index", description "Lets agents look up where code is defined and who uses it, from a local
  index of the project."

## Benchmarks

- Deterministic: per-language fixtures in `tests/fixtures/code/<lang>/` and golden edge lists (`tests/code_golden.rs`,
  insta). Public repos pinned by commit in `bench/repos.toml`, fetched by `bench/fetch.sh` into
  `target/bench-repos/<name>`: junegunn/fzf (Go), vuejs/vitepress (Vue/TS), encode/httpx (Python).
- Private repos (local only, gitignored `bench/local.toml` + `bench/local/`): track-things-frontend,
  track-things-backend, harness-useful-desktop.
- A/B: `bench/ab/run.sh` runs `claude -p --model sonnet --output-format json --strict-mcp-config` with
  harness-memory under an isolated `HARNESS_MEMORY_DIR`, arm A `HARNESS_MEMORY_CODE_INDEX=0`, arm B `=1`;
  records input tokens (incl. cache) and turns, checks the answer mentions the expected files; results in
  `target/bench-ab/*.jsonl`.

## Work packages

| # | Type | Scope | Depends on |
|---|---|---|---|
| H1 | backend | `src/code/` core: walk, store, extractors Rust/TS/SFC/tags+Go, resolvers, rank, render; `Cargo.toml` | – |
| H2 | backend | Python extractor + resolver | H1 |
| H4 | backend | switch, builds + status file, MCP tools/dispatch, CLI, features, README/CHANGELOG | H1 |
| H5 | general-purpose | fixtures, golden tests, `bench/` | H1, H2 |
| H3 | backend | speed-ups (a)–(d), each gated | H2, H4, H5 |
| H6 | parent | memory release 2.10.0 | H3 |
| S1 | general-purpose | skills retirement | ships after H6 |
| S2 | parent | skills release 0.5.0 | S1, H6 |
| D1 | backend | desktop business + src-tauri | H4 contract |
| D2 | frontend | desktop UI | D1 |
| D3 | parent | desktop release | D1, D2, S2 |

### Cheaper flow questions (added 2026-10-29)

- Call-site lines: each hop of `code_path` and the first 10 `used by` / 6 `uses` sites of `code_symbol` carry
  `path:line | source line`. The line is read at reply time (`walk::read_line`), only while the file's size and
  modification time are those in the graph (`File.mtime_ns/size`, loaded from `files`), through the same open-checks
  as `read_text`, one line of at most 2 KiB, literals blanked by `extract::elide_line`, whitespace squeezed, cut to 120
  characters, `display_escaped`. Lines that may be inside a string (backtick, Python triple quote, a Python `{` before
  the name) are left out. Limitation: a file touched without a content change (same hash) moves its stamp in the
  database but not the graph in memory, so its lines are left out until the graph is loaded again.
- Ends (`graph/endpoint.rs`), after a plain symbol query (unchanged) finds nothing: a file path (as `focus` is read, or a
  path ending with a file name that at most 4 files have; all definitions, the first 400, production first; a start
  also takes the references made outside any definition, rendered as a `<module>  path` first line), then a string
  that names no definition (`Graph.loose`: where `str` references to undefined names are written, at most 50,000 places
  and 32 per name, kept for this; the definitions that write it are the end, at most 64). The reply puts a note under the
  head (`from: ...`, `to: ...`) whenever an end stands for more than one definition, also for a symbol with several.
- Instructions and descriptions: the code line says to call `code_path` once for A to B. The instructions stay under
  2000 characters with the English line on, which needed small cuts in the base text (the tags line, the English
  line, "repo" for "repository").
- Functions passed through objects (schema 11, `graph/props.rs`): an object literal property whose value is a function
  reference (`key: fn`, `{ fn }`, `key: a ?? fn`, `key: c ? a : b`, `key: fn.bind(x)`, `key: ns.fn`; not `this.m`) is
  stored as a `prop` reference (`name` = key, `qual` = the value as written, `recv` = the function the literal is passed
  to as an argument, `create({..})` / `new Runner({..})`). It is no edge and not in `Graph.refs`: at load each value is
  resolved like a function passed as a value (same file, import, or a member of a namespace import) into
  `Graph.props[key]`, at most 64 bindings per key. A method call (`mcall`) that `resolve_ref` left with no target
  resolves through `props[call name]` as a weak edge (`?`, `soft`): receiver not an import or a type of the repository
  (a receiver of a repository type only counts with direct flow), bindings in test files only for calls in test files,
  at most 4 distinct functions bound to the key (else no edge), and bindings whose literal is passed to a function that
  contains the call (that function resolved at load as a call, `Graph.contains` = same definition or line range) are
  preferred: weight 0.75 against 0.55 for the name alone; both are followed by `code_path` (>= 0.5). Reading `host.run`
  on a repository without the pattern changes nothing: only calls that had no target gain edges. Python keyword
  arguments (`Thread(target=work)` already is a `fnref`) and dictionaries are not bound: Python attribute calls are
  `call` references with a qualifier told apart late, and `self.f()` carries the class as qualifier, so they need their
  own step. Go has no function-value references yet. An identifier in an array literal is a `fnref` now (a table of
  steps, `[["Code graph", codeGraphStep]]`), which the measured skills chain needed.
- Words as ends (schema 12, `graph/words.rs`): a text that is no symbol, file or known string is read as words.
  The text is cut into words (letters, digits, `_`, `-`; at least 3 characters; filler dropped: the, and, for,
  from, with, when, how, what, which, that, this, into, then, its, are, has, have, does, ask(s), file(s),
  request(s), changed, user(s); at most 6), and each word is matched, without regard to case, against (a) the
  names of definitions (the whole name, `runDoctorSetup` = `run-doctor-setup`, then a part of it by camel and
  snake segments; at most 64 definitions of one name) and (b) word literals: a string literal inside a definition
  that is one lowercase word or a short command (`^[a-z][a-z-]{2,31}$` or `^/?[a-z][a-z-]{1,30}( [a-z][a-z-]{1,30})?$`,
  at most 32 bytes: lowercase letters and `-`, so never a credential) is stored as a `word` reference (TS/JS, SFC
  scripts, Python, Go, Rust; JSX attributes, module-level strings and import sources are left out; at most
  `MAX_WORDS_PER_FILE` = 2,000 per file, left out without a count). At load they are no references: `Graph.words`
  maps the literal, and each of its words for a command, to the definitions that write it (32 per key, 100,000
  places), and the parts of a `str` reference (`"git_file_lines"` gives git, file, lines) are added the same way.
  Ranking: test code last, functions and components before constants and structs, then the number of words matched,
  then the sum of (tier x rarity), tier 3 the name, 2 a literal the definition writes, 1 a part of the name; rarity
  falls by 2 for each doubling of the definitions that hold the word; then the shorter name. The eight best are
  the end and the note under the head says how the words were read (`from: words `doctor` -> doctor command-core.js:245, ...
  (no match: `x`)`). `code_path` searches from each start, best ranked first, and takes the first whose chain is at
  most one hop longer than the nearest (so the command a question names is the start, not a helper it calls); a
  definition that stands for both ends is no start for the other end. Measured on the benchmark's own questions:
  `/useful-skills doctor` -> `harness-memory features` finds doctor -> runDoctorSetup -> codeGraphStep ->
  probeCodeGraph (3 hops, the skills repository), and `diff view lines` -> `validate_paths` finds DiffPanel ->
  git_file_lines (by name) -> file_lines -> git_file_lines -> validate_paths (desktop). `diff view full lines` ->
  `git` finds no chain: `git` is a function of 5 other places and nothing on the way from DiffPanel reaches them.
  A text that matches no word of a definition, nor a similar name, is answered `No symbol, file or command matches
  `x`. Grep once for that text to find the function that handles it, then call code_path from that function.`
  (with similar names suggested as before when there are some). The instruction line and the `code_path`
  description now say the ends may be plain words and that a grep for a starting point should become a `code_path`
  call from it (instructions 1,995 characters with the English line on, description at most 220).
- `code hint` (harness-memory 2.10): `features` gains `code-hint`. `src/code/hint.rs` opens `code.sqlite3` read-only
  (no write, no folder, no worker, no graph) and asks `symbols(repo_id, name)` once: see the README.
