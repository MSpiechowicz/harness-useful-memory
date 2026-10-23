# Hybrid search, English notes and additive migration

Approved 2026-10-23. A = this repository (harness-memory), B = harness-useful-desktop. A ships first (2.7.0); B gates
everything on `harness-memory features`.

## Goals

- Keyword search (always on, every backend): English stemming, stopwords dropped from the query, a stopword-only query
  answers "no matching notes", a note must match at least `ceil(n/2)` of the remaining query stems, a per-backend
  score floor, one ranking for repository and global notes (a repository boost, not `ORDER BY project_id IS NULL`
  first), a context boost for notes whose entities or path segments equal a query word. `similar` stays unchanged.
- Vector search (default on in Harness Desktop, opt-out): int8 `Xenova/paraphrase-multilingual-MiniLM-L12-v2`
  (384-d, mean pooling, L2-normalised), served by ONE `harness-memory embedder` per computer, which every
  `harness-memory mcp` asks over a local socket. Reciprocal rank fusion (k = 60) of keyword and vector lists plus a
  cosine floor. Embedder unreachable while enabled: keyword results and one notice line on the first search of a
  session. Disabled: silent, identical to keyword-only.
- Vectors live only in a local cache per computer, never in a team database.
- English notes setting, stored in the store (team-wide on a team database), default on (no stored value = on). When
  on, a clearly non-English agent `note_save`/`note_update` is refused before the guards gate ("rewrite this note in
  English … Nothing was saved."), nothing is held. User CLI saves only warn. `push` is not checked. Flagged notes
  (`lang = "other"`) get a rewrite hint in `note_get`; `notes list --json` returns `lang` and filters by `--lang`.
- Migration is additive: no `SCHEMA_VERSION` bump. An older harness-memory keeps reading, searching and writing an
  upgraded database, and its writes stay findable.

Non-goals: vectors in team databases, ANN indexes, translating notes, changing the scripts search, GPU.

## Interfaces

- `features` gains `embedder`, `settings`, `notes-lang`.
- `harness-memory embedder [run]`: single instance (`<data>/harness-memory/embedder.lock`, a second start prints
  `{"event":"busy"}` and exits 0), exits on stdin EOF and SIGTERM, prints JSON lines: `download {received,total}`,
  `verifying`, `loading`, `ready {model,dim}`, `backfill {store,done,total}`, `model-update {from,to}`,
  `reembed {store,done,total}`, `switched {model}`, `cleanup-done`, `idle`, `error {message}`.
- `embedder enable|disable` writes `<data>/harness-memory/embedder.toml` (`enabled = bool`, missing = disabled).
  `embedder status --json` → `{enabled,running,active_model,pending_model,model_present,vectors}`.
  `embedder remove-model` deletes model files and vectors (refused while running).
- Socket: Unix `<data>/harness-memory/embedder.sock` (0600; when the path exceeds 100 bytes,
  `$TMPDIR/harness-memory-<hash>.sock`), Windows named pipe `harness-memory-embedder-<user hash>`. Newline JSON, one
  request per connection, `"v":1`. Ops: `ping` → `{ok,model,ready,pending}`; `search {text,targets:[{store,projects}],
  limit,floor}` → `{ok,model,hits:[{store,id,cosine}]}`; `upsert {store,notes:[{id,project,title,body,tags}]}`;
  `forget {store,ids}`; `warm` → `{ok}` at once, then the embedder loads the model if it was unloaded (`mcp` sends it
  at `initialize` when embedding is on, so the first search does not pay the 0.7-1.7 s reload). Client timeouts cover
  the whole call and are kept by the client itself (a helper thread, since Windows named pipes have no socket
  timeouts): search 4 s, ping 500 ms, upsert/forget 200 ms, warm 250 ms. A model download honours `ALL_PROXY`,
  `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`, and removes stale `*.part` files first.
- Store key: `local:` + first 16 hex of sha256(canonical path); team: `<kind>:` + first 16 hex of sha256(normalised
  URL without userinfo + database name). No secrets in keys or logs.
- Vector cache `<data>/harness-memory/vectors.sqlite3`: `vectors(store, note_id, project_id, content_hash, model, dim,
  vec BLOB, PRIMARY KEY(store, note_id, model))`, `cursors(store, updated_at_max)`, `active(model)`.
- Model manifest compiled in (`src/embed/manifest.rs`): versioned `id`, release-asset `url`, `sha256`, `size`, `dims`,
  `pooling`, `normalize`. Downloads only from the manifest URL (a harness-useful-memory GitHub release asset), never
  Hugging Face. A new manifest id: download + verify in the background, keep serving with the old model, re-embed every
  note under the new id, switch atomically, then delete the old files and rows. Failed switch: keep the old model,
  retry at the next start and hourly.
- `harness-memory settings get --json` → `{"english_notes":true}`; `settings set english_notes=true|false --json`
  writes every writable store. Storage: SQLite/libSQL `settings(key TEXT PRIMARY KEY, value TEXT)`, Postgres
  `harness_memory_meta` row `english_notes`, Mongo/DocumentDB meta `{_id:"settings", english_notes}`.
- `notes list --json` rows gain `"lang": "en" | "other" | null`; `--lang en|other|unknown`.

## Additive extensions (A1)

`ensure_extensions()` runs after `migrate()` on every open, checks for each object, never reads the version:

- SQLite/libSQL: `notes_fts_en` (fts5, `content='notes'`, `tokenize='porter unicode61 remove_diacritics 2'`), three
  triggers, `rebuild`; `ALTER TABLE notes ADD COLUMN lang TEXT`; `settings` table; a trigger that nulls `lang` when an
  older client edits title or body.
- Postgres: `search_en` generated tsvector (`english`, same A/B/D weights) + GIN index, `lang`, meta row, lang reset.
- Mongo and DocumentDB: `terms_en` (Snowball English stems) + index, `lang`. The existing `notes_text` index is not
  touched.
- No DDL right: log once and run without the extensions.
- Backfill `lang` and `terms_en` in pages.

## Order

Spike (engine choice, gates A4/A5) → A1 → A2, A3, A4 in parallel → A5 → A6 docs → B1 → B2, B3 → skills guidance.

## Spike thresholds

Parity with the Python reference (cosine ≥ 0.99), RSS idle and in backfill, p50/p95 query latency, binary growth,
build on all four targets. RSS above 400 MB → idle unload. Binary growth above 40 MB → user decision. Neither
engine builds everywhere → stop.

## Spike result (2026-10-23)

Engine: **tract-onnx 0.23.9** (user decision). Linux x86_64: parity min cosine 0.992 vs the Python reference, query
p50 8.7 ms, backfill 12-14 notes/s, idle RSS 453 MB (tokenizer alone 286 MB), +24 MB binary. ort was exact and faster
but has no x86_64-apple-darwin prebuilt and downloads ONNX Runtime from a third-party CDN at build time. Tokenizer:
`tokenizers` 0.23.2 with `fancy-regex`, no onig. RSS is above 400 MB, so the embedder unloads the model and tokenizer
after 10 idle minutes and reloads on demand (vectors stay loaded). Dynamic int8 quantisation makes a vector depend on
batch composition (about 0.987 cosine alone vs in a batch): embed one note per inference call. Spike sources:
scratchpad `spike/` (RESULTS.md).

Model assets (two files, each SHA-256 pinned in the manifest; uploaded to a harness-useful-memory release by the main
session before release):

- `model_quantized.onnx`: 118308126 bytes, sha256 `66fc00f5f29afcaff34092e1bdd20008ca3918265a82fb9695a551e510cc4ebc`
- `tokenizer.json`: 17082913 bytes, sha256 `b60b6b43406a48bf3638526314f3d232d97058bc93472ff2de930d43686fa441`

## Known deviations

Accepted after review, not fixed:

- SQLite and libSQL stem English with FTS5's `porter`, PostgreSQL and MongoDB with Snowball, so inflections can differ
  between backends (for example `ties` does not match `tie` on SQLite). The vectors cover such misses.
- `set_english_notes` on a split store writes its two stores one after the other: if the second write fails the stores
  disagree until the setting is saved again.
- On Windows the embedder's server side has no per-connection read timeout either (named pipes), so a client that
  connects and never sends holds one of its 32 connection slots until it disconnects.
