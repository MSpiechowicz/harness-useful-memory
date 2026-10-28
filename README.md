<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/lockup-dark.svg">
    <img src="docs/brand/lockup-light.svg" alt="harness memory" width="430">
  </picture>
</p>

# Harness Memory

**Project memory for your AI coding agents, in one local binary.** `harness-memory` is a stdio
[MCP](https://modelcontextprotocol.io) server that Claude Code, Codex, OMP, PI, OpenCode and any other MCP client can
start. Agents search it at the start of a task and save what they learn (fixes, decisions, where things live) before they
finish, so the next session in the same repository starts better informed. Notes and a small entity and relation graph
live in a local SQLite database with full-text search (FTS5), keyed per git repository. Search is stemmed English keyword
search everywhere. Optional vector search adds notes close in meaning, through one local embedder per computer (see
[Search](#search)). The server opens no network connection unless you configure a team database, and the embedder
downloads its model once when you turn vector search on. Notes are treated as untrusted data written by agents, and secrets are redacted before anything is
stored. A team can point the server at one shared database instead (see [Team memory](#team-memory)).

## Install

**macOS and Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/MSpiechowicz/harness-useful-memory/main/install.sh | sh
```

This installs `~/.local/bin/harness-memory` and, if that folder is not on your `PATH`, adds it to your shell profile.
Linux is x86_64 only; macOS is Apple Silicon or Intel.

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/MSpiechowicz/harness-useful-memory/main/install.ps1 | iex"
```

This installs `%LOCALAPPDATA%\Programs\harness-memory\harness-memory.exe` and adds that folder to your user `PATH`.

Both installers download the binary and `checksums.txt` from the GitHub release and install nothing unless the SHA-256
matches. They never stop a running server. Options (environment variables):

| Variable | Effect |
| --- | --- |
| `HARNESS_MEMORY_VERSION=v0.1.0` | Install this release instead of the latest |
| `HARNESS_MEMORY_NO_MODIFY_PATH=1` | Leave your shell profile (or user `PATH`) alone |

You can also download a binary from the [releases page](https://github.com/MSpiechowicz/harness-useful-memory/releases)
and verify it against `checksums.txt`, or with `gh attestation verify <file> --repo MSpiechowicz/harness-useful-memory`.

```sh
harness-memory --version
```

### Updating

Run the install command again. A harness that is using the server keeps the old copy until it starts the server next. On
Windows the old file is renamed to `harness-memory.exe.old` and removed by the next update once nothing runs it.

### Uninstalling

First remove the server from each harness (see [below](#register-with-your-harness)), then delete the binary and,
optionally, the notes (a team database is not touched, remove it on its server):

```sh
rm ~/.local/bin/harness-memory                                     # Linux and macOS
rm -r "${XDG_DATA_HOME:-$HOME/.local/share}/harness-memory"        # Linux: the notes
rm -r ~/Library/Application\ Support/harness-memory                # macOS: the notes
```

On Windows delete `%LOCALAPPDATA%\Programs\harness-memory`, remove that folder from your user `PATH` (Settings, System,
About, Advanced system settings, Environment Variables), and delete `%LOCALAPPDATA%\harness-memory` to remove the notes.
The installer's `PATH` line in your shell profile is marked `# Added by the Harness Memory installer`.

## Register with your harness

`harness-memory mcp` (or just `harness-memory`) serves MCP over stdio, and the harness starts it. Each harness keeps its
MCP servers in its own settings, so add it once per harness you use. The entry is called `harness-memory` below, which is
the name harnesses then list. Removing the entry from a harness is all there is to undo; your notes stay.

**Harness Desktop** registers it for you in every detected harness while **Project memory** is on in its Settings (the
default), so the steps below are for harnesses you run outside the app.

**Claude Code**

```sh
claude mcp add --scope user harness-memory -- harness-memory mcp   # add
claude mcp remove --scope user harness-memory                      # remove
```

**Codex**

```sh
codex mcp add harness-memory -- harness-memory mcp   # add
codex mcp remove harness-memory                      # remove
```

Or by hand in `~/.codex/config.toml` (remove the table to take it out again):

```toml
[mcp_servers.harness-memory]
command = "harness-memory"
args = ["mcp"]
```

**omp (oh-my-pi)**: in `~/.omp/agent/mcp.json` (or `.omp/mcp.json` in a project), or with `/mcp add` inside omp:

```json
{
  "mcpServers": {
    "harness-memory": { "command": "harness-memory", "args": ["mcp"] }
  }
}
```

To remove it, delete the entry and run `/mcp reload`, or turn it off with `/mcp disable`.

**pi**: recent versions read `~/.pi/agent/mcp.json` (or `.pi/mcp.json` in a project), with the same `mcpServers` entry as
omp above. Older ones need the adapter first: `pi install npm:pi-mcp-adapter`. To remove it, delete the entry.

**OpenCode** (`~/.config/opencode/opencode.json`, or `opencode.json` in a project):

```json
{
  "mcp": {
    "harness-memory": { "type": "local", "command": ["harness-memory", "mcp"] }
  }
}
```

To remove it, delete the entry, or set `"enabled": false` to keep it but turn it off.

If a harness cannot find the command (an app started without your shell's `PATH`), use the absolute path of the binary
as the command instead.

## Tools

| Tool | What it does |
| --- | --- |
| `note_search` | Finds notes by a few keywords, repository notes before global ones. Without a query it lists the most recent notes. With a query it also lists up to 5 saved scripts whose name or description matches one of its words, in a `<saved-scripts>` block after the notes. Agents call it first, at the start of a task |
| `note_save` | Saves a note: title, body, kind (`outcome`, `problem`, `fix`, `optimisation`, `location`, `decision`, `note`), tags and the entities it is about. A note with the same title in the scope is updated, and similar notes are listed so outdated ones get fixed |
| `note_update` | Changes a note by id: any of title, body, kind and tags, or appends text to the body |
| `note_get` | Reads whole notes by id (up to 5) |
| `note_feedback` | Tells which notes of a `note_search` helped with the next step: their ids (up to 20, or an empty list when none did) and, optionally, a few words why (only `ids` is required). Call it only after a search that returned notes. Each note named counts as found useful ([usefulness counters](#usefulness-counters)). It is not a save, so it works while saving is off, paused or reviewed. The optional reason is not stored by harness-memory (it stays in the agent's own transcript and tool-call history). Ids that are not visible to the caller (another repository's notes, team notes hidden on this computer) are reported as `not found` and not counted, and ids that could not be checked (the team database or the hidden list could not be read) are reported as `not checked` and not counted |
| `note_delete` | Deletes a note by id |
| `entity_link` | Relates two entities, `from relation to`, for example `ConfigLoader located_in src/config.rs`. Missing entities are created |
| `entity_neighbors` | Shows the relations around an entity (one or two hops) and the notes linked to them |
| `entity_delete` | Deletes an entity with its relations, or one relation |
| `script_save` | Saves a deterministic script (`sh`, `bash`, `python`, `node` or `bun`) by name, so an action agents repeat, such as a `curl` or `gh` fetch, runs without being worked out again. A script with the same name in the scope is replaced |
| `script_list` | Lists the saved scripts, repository scripts first, optionally filtered by a word of the name or description |
| `script_get` | Reads a script with its body, by id or name |
| `script_run` | Runs a script by id or name with arguments and standard input, and returns its exit code, stdout and stderr. A script from a teammate runs only after you approve it |
| `script_delete` | Deletes a script by id or name |

Every tool takes an optional `scope` (`repo` or `global`, and `both` for reads) and a `project_path`, the absolute path
of the repository, for when the server cannot tell it from its working directory. The server also sends instructions that
tell agents when to search, when to save, and to update a note rather than add a duplicate.

Per-call limits keep the notes short: a title is at most 120 characters, a body 4000, a note has at most 8 tags and 10
entities, and search and read replies are capped at a few KiB. The number of notes is not capped.

The script tools also return `structuredContent`: `script_save` `{"script", "created"}`, `script_list` `{"scripts"}`,
`script_get` `{"script"}` (with `body`), `script_delete` `{"deleted", "id"}` and `script_run` `{"id", "sha256",
"exitCode", "timedOut", "durationMs", "stdout", "stderr", "stdoutTruncated", "stderrTruncated"}`. A script object has
`id`, `name`, `description`, `language`, `scope`, `repo` (`host/owner/repo`, or `null` for a global script), `sha256`,
`timeoutSecs`, `place` (`team` or `local`), `author` (`source`, `root`: on one line without control or invisible
characters, at most 64 and 512 characters), `createdAt`, `updatedAt`, `runCount`,
`lastRunAt`, `lastExitCode` (RFC 3339 UTC times), `trusted` and `approval` (`local`: saved on this computer,
`approved`: approved by you here, `team`: a team script with `trust_team_scripts` on, `needed`: waits for your approval,
see [Approving scripts](#approving-scripts)). A refused run returns `{"untrusted": true, "id", "name", "pending"}`,
where `pending` is `false` (not asked), `"declined"` or `"timeout"`.

### Scripts

A script is a file the server writes to a private temporary folder and runs with its interpreter, found by name on
`PATH` (`python3`, then `python`, for Python), as `<interpreter> <file> <args…>`: arguments are passed as they are,
never through a shell. The folder (`0700`, removed after the run) is in `run` next to the local database
(`<data folder>/harness-memory/run`, or `run` in `HARNESS_MEMORY_DIR`), never in the shared system temporary folder,
because Node and Bun look for modules and settings in every parent folder of a script. It runs in the repository root
(the server's working directory outside one) with the user's environment, so `gh` and `curl` see `GH_TOKEN` and the
rest, but not the team database settings (`HARNESS_MEMORY_DATABASE_URL`, `HARNESS_MEMORY_DATABASE_TOKEN`,
`HARNESS_MEMORY_CONFIG` and the routing variables).

Since the file is not in the repository, imports relative to the script do not find repository files: `import helper`
in Python and `require('./helper')` or `import './helper.ts'` in Node and Bun look next to the temporary file. Use
paths relative to the working directory instead (`path.resolve('helper.js')`, `sys.path.insert(0, os.getcwd())`), absolute
paths, or run the repository's own tools from the working directory (`./scripts/release.sh`, `npx eslint`).

A run ends when the script exits. Processes it left in the background that still hold its output get 1 s, then they
are killed (Linux and macOS). A run is killed after the script's timeout (60 s unless it names one, at most 600 s),
with everything it started on Linux and macOS; on Windows only the interpreter is killed. A run is killed the same
way, and its temporary file removed, when the client cancels the request (`notifications/cancelled`, which then gets no
answer), closes the server's input, or (Linux and macOS) sends the server SIGTERM. The reply keeps 32 KiB of
stdout and 8 KiB of stderr, masked before the cut so a secret crossing it is masked whole: credential formats, secret
assignments and URL secrets (random-looking words are kept, so JSON output stays intact).

A body holding a credential is refused: scripts read secrets from environment variables. So is a body with control
characters (other than newlines, tabs and a carriage return right before a newline) or invisible format characters
such as bidirectional overrides, zero-width spaces and variation selectors, which could make a reviewed body run
differently from how it reads (a lone carriage return hides the rest of a line, and `ok` with a variation selector is
another JavaScript name than `ok`). A Python body that declares an encoding other than UTF-8 on its first two lines
(`# -*- coding: utf-7 -*-`, PEP 263; `utf-8`, `utf8` and `utf_8` are fine) is refused too, since Python would run
other code than the text you read, and a Python body without a declaration is written after a UTF-8 byte-order mark,
so Python reads it as UTF-8 whatever it says. A script with one of these in the team database is not run or approved
either. Emoji
keep what they need: the emoji joiner (U+200D) next to an emoji, and U+FE0E or U+FE0F right after an emoji symbol
(U+FE0F also after a keycap digit, `#` or `*`). A body is at most 64 KiB, a run takes at most 32 arguments of 4 KiB
and 64 KiB of standard input.

#### Approving scripts

An agent cannot approve a script through the MCP tools. A machine runs only the script versions it trusts: the ones
saved on it with `script_save`, and the ones you approved there. Trust belongs to one script, where it is kept (this
computer, or the team database, named by a hash of its kind, hosts and database name without credentials, the
scheme's default port and the order of MongoDB hosts left out, so every way of writing one database URL names the
same database) and its id there, and to one version of it, named by its `sha256`: the SHA-256 of its language and body
(`harness-memory-script-v1`, a NUL byte, the language, a NUL byte, the body), so the same body run by another
interpreter is another version. Approving a script never approves another script with the same code. A version is
trusted as the script it was then: a trusted script renamed, or moved to another scope or repository, in the team
database needs your approval again (the request shows its old name and scope). The description and timeout are not
part of it. Trust is kept in the local database, also when scripts are in the team database, with the last 5 trusted
versions of each script.

Code an agent saves is trusted because the agent wrote it, by design: the approval protects you from code a teammate
or anyone with access to the team database wrote running without you, not from your own agent. `script_save` does not
trust a body that is the code of a visible script waiting for approval (in either store, in any language, also when it
replaces that script): the copy waits for approval too. A copy with any edit is new code and is trusted like any code
the agent writes, so read what your agent saves. Neither is the code of a script reported here as waiting for
approval before (by `script_list`, `script_get`, `script_run`, `script_delete` or `scripts list`): this computer keeps
the hashes of those versions, so deleting the original and saving its code again does not trust it. `script_save`
trusts the body and language the agent sent, and only when the store gives them back unchanged (a database trigger
could change the row as it is written), so a changed save waits for approval too.

A script a teammate saved in the team database, or one whose body or language changed since, needs your approval:

- **In Harness Desktop.** While Harness Desktop runs, `script_run` asks you there and waits up to 45 s for your answer:
  the server writes a request to `requests/<id>.json` next to the local database (`0600` in a `0700` folder, the id
  128 random bits) with the body, the arguments, the folder it runs in (`cwd`), its standard input (`stdin`: the
  first 2048 characters without control or invisible characters, as `preview`, and its size in `bytes`, or `null`) and
  the version trusted before (`previous`: `body`, `language`, `name`, `scope`, `trustedAt` and `origin`, `saved` or
  `approved`, or `null`), so the desktop can show what changed, and Harness Desktop answers with
  `requests/<id>.decision`. Run
  approves the version and runs it, Don't run refuses it. Both files are removed however it ends, and requests older
  than 2 minutes are removed before the next one. On Linux and macOS nothing is asked unless the data folder and the
  requests folder are folders (not links) of your user that no one else may write to. The desktop shows it is running
  by touching `desktop.alive` in the same folder. Scripts that need approval are also listed under Harness Desktop →
  Memory, where you can approve them before an agent asks.

  45 s is longer than some clients wait for a tool. If the client stopped waiting, Run still runs the script, but the
  agent does not see the result: it should call `script_run` again once you approved (which runs the script again).
  Without your answer nothing runs, also when the client hung up. A cancel of the request, the end of the server's
  input or SIGTERM ends the wait at once, and nothing runs.
- **In a terminal.** Without Harness Desktop the run is refused at once, and the agent is told to ask you. Review and
  approve from a terminal (`--project` picks the repository whose scripts are shown, the working directory's by
  default):

  ```sh
  harness-memory scripts list --json --untrusted    # the first 50 scripts that need approval, as script_list returns them
  harness-memory trust 12=<sha256>                   # approve script 12 if it still has the sha256 you reviewed
  harness-memory trust 12 14                         # approve scripts 12 and 14 as they are now
  harness-memory trust --all                         # approve every visible script that needs it, as it is now
  harness-memory scripts get 12 --json              # script 12 with its body: {"script": {...}}
  harness-memory scripts delete 12                   # delete script 12 as script_delete does
  harness-memory scripts delete --all --yes --json   # delete every script scripts list shows: {"deleted": N, "team": M}
  ```

  `<id>=<sha256>` takes the `sha256` of `scripts list`: a script changed since then is not approved
  (`harness-memory: script 12 open-prs changed since you reviewed it`). It works from any program (Harness Desktop
  uses it). A plain id and `--all` approve the versions there are now, so read them in the same terminal first: they
  run only with a terminal as standard input, and exit with 2 otherwise (`harness-memory: run this in your own
  terminal: …`), so an agent's shell tool cannot approve what no one reviewed. A script whose body holds a hidden
  character or declares another encoding is never approved, since it cannot run. `trust` prints one line per approved
  script on stdout and exits with 1 when an id is not found, a script changed since its review or a body cannot run
  (the others are still approved). When it fails, the last line on stderr says why (`harness-memory: …`, after any
  warning, and `harness-memory: <n> scripts were not trusted` when several were not), so a program calling it can show
  that line.

  `scripts list --json` prints one page, `{"total", "groups": {"needsReview", "repo", "global"}, "scopes": {"repo",
  "global"}, "review": {"needed", "trusted"}, "places": {"local", "team"}, "scripts"}`, and takes `--group
  needsReview|repo|global` (`--untrusted` is `--group needsReview`), `--scope repo|global`, `--place local|team` (kept
  on this computer or in the team database), `--review needed|trusted`, `--query TEXT` (name or description,
  ignoring case), `--sort name|language|scope|place|runs|lastRun|updated`, `--order asc|desc`, `--offset N` and
  `--limit N` (1 to 200, 50 by default). Each script has `place` (`team` or `local`: where it is kept), `readOnly` (a
  team script while `scripts = "remote+local"`) and `hidden` (hidden on this computer). With a team database the object
  also has `hiddenCount` (the team scripts of the view hidden on this computer) and `localCounts` (`{"repo", "global"}`:
  the scripts in the local file per scope, every repository's). `--hidden` lists only the hidden team scripts instead,
  with the same filters, order and paging (`total` counts them); a hidden script the team database no longer has
  leaves the hidden list.
  The groups do not overlap: `needsReview` holds every script that needs approval, `repo` and `global` the other
  scripts of that scope. `--scope` keeps the scripts of one scope, `--review needed` the ones that need approval (the
  `needsReview` group) and `--review trusted` every other one. `--scope` and `--review` combine with each other but not
  with `--group` (that is a usage error). `--place` combines with every filter. The group counts follow `--query` and
  `--place`, the `scopes` counts follow `--query`, `--review` and `--place`, the `review` counts follow `--query`,
  `--scope` and `--place`, the `places` counts follow every filter but `--place`, and `total` follows every filter. By name (A to Z,
  repository scripts first on a tie) unless `--sort updated` (newest first), `language`, `scope` (global scripts
  first), `place` (this computer's first, then by id), `runs` (the run count) or `lastRun` (a script
  never run comes last whichever the order), all ascending. `--order` turns any round, and ties go by id. Approval is
  decided on this computer, so every visible script is read and the page is cut there. Usage errors exit with 2.

  `scripts delete <id>` deletes a visible script as `script_delete` does (a version waiting for approval is
  remembered first, so saving its code again trusts nothing) and prints `deleted script <id> <name>`, or exits with 1
  and `harness-memory: script <id> not found`. `scripts delete --all --yes` does that for every script `scripts list`
  shows without filters (every group and page) in one write, and prints `deleted N scripts`, or with `--json`
  `{"deleted": N, "team": M}`, where `team` counts the ones that were in the team database. With `scripts =
  "remote+local"` it deletes only this computer's scripts and counts the team's that stay (`{"deleted": N, "team": 0,
  "kept": K}`). Without `--yes` it deletes nothing and exits with 2. A team script that is read-only here is not
  deleted by `scripts delete <id>` either: it exits with 1 and names `harness-memory scripts hide <id>`.

  `scripts get <id> --json [--project PATH] [--hidden]` prints `{"script": {...}}`, the list's object with its
  `body`; a hidden team script is not found unless `--hidden` is given.

To skip the approval for the team's scripts, set `trust_team_scripts = true` in the [config file](#configure-each-teammate) (or
`HARNESS_MEMORY_TRUST_TEAM_SCRIPTS=1`): every script read from the team database then runs as if you had approved it,
and `harness-memory check` adds `team scripts: trusted automatically` (only when scripts are read from the team
database). Anyone who can write to the team database can
then run code on your computer. Scripts saved on this computer stay trusted either way.

A row changed by hand in a team database does not break the list: its timeout is read as 1 to 600 s, and a script with
a language this server cannot run, an invalid name, or a description or body over its limit is left out (with a line
on the server's stderr), as if it did not exist.

Scripts need team database schema 2 (PostgreSQL, MongoDB) or 3 (libSQL), [team hooks](#sharing-hooks) schema 3
(PostgreSQL, MongoDB) or 4 (libSQL), [team routines](#sharing-routines) schema 4 (PostgreSQL, MongoDB) or 5 (libSQL). A release that raises the schema migrates the team database on first use (it only
adds tables and collections), and from then on every tool of an older `harness-memory` fails on it, notes included
(`check` says `update it`): when you upgrade, the whole team upgrades together.

The English search objects, the `lang` of a note, the [English notes](#english-notes) setting and the
[usefulness counters](#usefulness-counters) of a note are different. They are additive extensions that every backend
adds on open to what is missing, and they do not raise the schema version. An older `harness-memory` keeps reading,
searching and writing an upgraded database. What it misses: it searches with the old, unstemmed index, it does not set
`lang` (a note it writes or edits is looked at again by the next newer server), it does not know the English notes
setting, and it counts no usefulness. A database that gives this server no right to change its schema is used with
what it has, with one line on stderr: English stemming, the setting and the counters are then off where they are
missing (a note then lists with `null` counters).

### Browsing notes

Harness Desktop shows and deletes notes with these commands (they are for people and programs acting for them, not
for agents, which use the tools). Each sees the notes `note_search` would with `project_path` set to `--project`:
that repository's notes and the global ones, only the global ones without `--project`, from wherever the server keeps
them (the local file, the team database, or both).

```sh
harness-memory notes list --json --project ~/src/widget --query cache --sort updated --order desc --offset 0 --limit 50
harness-memory notes get 12 --json --project ~/src/widget
harness-memory notes delete 12 --project ~/src/widget
harness-memory notes delete --all --yes --json --project ~/src/widget
harness-memory notes list --json --all-repos
harness-memory notes list --json --repo git:github.com/acme/widget
harness-memory notes repos --json
```

`notes list`, `get`, `hide`, `unhide` and `delete <id>` (and the same `scripts` commands) take one of `--project PATH`,
`--repo KEY` (a repository key, `git:github.com/acme/widget`, wherever it is checked out) or `--all-repos` (every
repository's notes and the global ones; `delete --all` refuses it). Each note has `repoKey` next to its `repo` label.
`notes repos --json` and `scripts repos --json` print `{"repos": [{"key", "label", "count"}], "global": N}`: the
repositories that have entries, by label, and how many global ones there are.

- `notes list --json` prints one page, `{"total", "kinds": [{"kind", "count"}], "scopes": {"repo", "global"},
  "places": {"local", "team"}, "notes"}`. `--query TEXT` keeps the notes whose title, body or tags contain it,
  ignoring case (on SQLite and libSQL only ASCII letters are folded), `--kind KIND` one kind, `--scope repo|global`
  one scope, `--place local|team` the notes kept on this computer or in the team database, and they combine. `kinds`
  counts every kind under the other filters, `scopes` both scopes under the other filters, `places` both places
  under the other filters, and `total` follows all four. Newest
  first unless `--sort title` (A to Z), `kind` (by kind name), `scope` (global notes first) or `place` (notes kept on
  this computer first), all ascending, or `--sort useful` (the notes found useful most often first) and `lastUseful`
  (the notes found useful most recently first, never useful ones last), both descending. `--useful ever|never` keeps
  the notes found useful at least once, or those shown at least 3 times and never found useful (`any` keeps every
  note), and narrows the counts like `--lang` does. `--order asc|desc` turns any round and ties go by id, `--offset N` and
  `--limit N` (1 to 200, 50 by default) pick the page. The database filters, sorts and cuts the page, and leaves the bodies there. A note
  object has `id`, `kind`, `title`, `scope` (`repo` or `global`), `repo` (`host/owner/repo`, a `path:` key whole, or
  `null` for a global note), `place` (`team` or `local`, where it is kept), `tags`, `entities`, `author` (`source` and
  `root`, shown like a script's), `createdAt`, `updatedAt` (RFC 3339 UTC), `revision`, `hits`, and the
  [usefulness counters](#usefulness-counters) `shown`, `useful` and `lastUsefulAt` (RFC 3339 UTC, `null` until the
  note was found useful). All three are `null` when the database has no counters.
- `notes get <id> --json` prints `{"note", "links"}`: the note object with its `body`, and up to 50 linked notes
  `{"id", "title", "kind", "relation", "direction"}`, the notes linked to the entity at the other end of a relation
  that touches one of this note's entities (`direction` is `out` when this note's entity is the relation's source,
  `in` when it is the target). Showing a note counts no hit (`note_get` does count one).
- `notes delete <id>` deletes the note as `note_delete` does and prints `deleted note <id> <title>`.
- `notes delete --all --yes` deletes every note `notes list` shows without filters (every page), in one write per
  store, as `note_delete` deletes one: relations stay without their note, entities no relation uses go. It prints
  `deleted N notes`, or with `--json` `{"deleted": N, "team": M}`, where `team` counts the ones that were in the team
  database. Without `--yes` it deletes nothing and exits with 2.

A note that is not visible exits with 1 and the last line on stderr is `harness-memory: note <id> not found`. Usage
errors exit with 2, other failures with 1 and the reason on the last line of stderr.

### Save guards

Harness Desktop lets you decide what agents may save, per chat, per project and everywhere, and can ask you before a
save. Reading, recall and `script_run` are never guarded. The desktop writes `guards/global.json` (notes and scripts:
`paused`, `offRepos`) and `guards/chats/<chatKey>.json` (`paused`, notes and scripts `on` and `ask`, and `repoRoot`, the
chat's project root or `null`) next to the local database, and starts each chat's harness with
`HARNESS_MEMORY_SESSION=<chatKey>`. Before `note_save`, `note_update`, `note_delete`, `entity_link`, `entity_delete`,
`script_save` or `script_delete` runs, the server checks the global pause of that kind, then the repository (the git
root of `project_path`, that of its working folder and the chat's `repoRoot`: any one turned off refuses, global-scope
writes included). A repository counts as turned off when its canonical path is an `offRepos` entry or its repository
key (`git:host/owner/repo`, see [Which repository](#which-repository)) is that of an entry, so other clones and git
worktrees of it are covered. Then the chat: paused or off refuses. With `HARNESS_MEMORY_SESSION` set, a missing chat
file refuses too (the desktop writes it before it starts the chat). With Ask on, `note_save`, `note_update` and
`script_save` are held in `held/<chatKey>/<heldId>.json` (one private folder per chat) for your review while Harness
Desktop runs (refused when it does not), at most 50 items and 4 MiB per chat, kept 30 days. Files of the older flat
layout `held/<heldId>.json` are ignored. A held item keeps only the fields the tool knows, in normal form (scope, kind
and language as the tool reads them, note text, tags, entities and script descriptions masked), and a `revision` that
changes whenever the agent holds it again. Nothing held is saved or searchable. The agent gets a normal result saying
it was held or refused, with `structuredContent` `{"held": true, "heldId"}` or `{"saved": false, "reason"}`. Reads
(`note_search`, `note_get`, `entity_neighbors` for notes, `script_list`, `script_get` for scripts) end with one line when
the same checks would not plainly let that kind save, such as "Saving notes is off for this project (the user's
setting). Reading works. Do not try to save notes.", and script reads add `structuredContent.saving` (`{"allowed":
false, "reason"}` or `{"ask": true}`). A guard
file that cannot be read, or a `HARNESS_MEMORY_SESSION` that is not a lowercase UUID, refuses. While any guard applies
to a call (a pause, a kind off or on Ask in its chat, a repository opt-out or a global pause of either kind), an
agent's `script_save` trusts nothing and holds the version back on this computer (in a table of the local database,
never in a team database, never pruned), so `script_run` asks you first with the code, also with `trust_team_scripts`
on. Only your approval (`harness-memory trust`, or Allow in Harness Desktop) releases it. Scripts never see
`HARNESS_MEMORY_SESSION`. The guards stop agents that use these tools. They are no sandbox: an agent with a shell can
change files in your home.

Your own saves go past every guard, through commands no agent reaches through MCP:

```sh
harness-memory held list --json [--chat KEY]                 # {"held": [...]}, newest first
harness-memory held save <heldId> --json [--expect REVISION] [< edited.json]   # {"saved": {"kind": "note", "id": 12}}
harness-memory held skip <heldId> --json [--expect REVISION] # {"skipped": "<heldId>"}
harness-memory notes save --json --project ~/src/widget < note.json          # {"id", "title", "scope"}
harness-memory notes update 12 --json --project ~/src/widget < patch.json    # {"id", "title"}
harness-memory scripts save --json --project ~/src/widget < script.json      # {"name", "scope", "id", "created", "approval"}
```

`held save` and `held skip` claim the item first, so two saves of one item save it once (the other gets `notFound`), and
an item the agent holds again meanwhile waits as a new one. With `--expect` they save or skip only the revision you saw.
A held `note_update` is refused with `changed` when the note's title, body, kind or tags are no longer those the item
shows as `current`. The item then keeps its id but shows the note as it is now as `current`, with a new `revision`, so
you review it again against that note and can save it. A held script is saved untrusted and held back: it needs your approval before it runs, also with
`trust_team_scripts` on.

`notes save`, `notes update` and `scripts save` take `--client NAME` to name the author (Harness Desktop passes
`harness-desktop`): 1 to 64 visible characters on one line. Without it the author is unknown.

A refusal prints `{"error": "notFound" | "gone" | "changed" | "invalid" | "refused", "message"}` and exits with 1.
`gone`: the note a held `note_update` changes was deleted since. `changed`: the held item is not of the `--expect`
revision, or the note a held `note_update` changes was changed since (the item now shows that note). A refused `held save` or `held skip` keeps the
item.

### Which repository

The server asks git (`git rev-parse --show-toplevel` and `git config --get remote.origin.url`) in its working directory,
or in `project_path`. The repository key is the normalized `origin` remote (`host/owner/repo`, credentials dropped), so
every clone of a repository shares its notes, and the repository path when there is no remote (so a path key matches only on one machine). A home or filesystem-root
directory is no project. The `global` scope is shared by all repositories.

### Search

`note_search` ranks with stemmed English keyword search on every backend (SQLite, libSQL, PostgreSQL, MongoDB,
DocumentDB). Stopwords are dropped from the query, and a query that is only stopwords finds no notes. A note has to match
at least half (rounded up) of the remaining query stems and pass a per-backend score floor. Repository and global notes
are ranked together, with a boost for the repository's notes and for notes whose entities or path segments equal a word
of the query. `similar` (the list `note_save` returns) is unchanged.

With vector search on, the keyword list is fused with a list of notes close in meaning (reciprocal rank fusion, k = 60).
The vectors come from the [embedder](#the-embedder), and a note has to reach a cosine floor to count. Vector search is
off until `harness-memory embedder enable` (Harness Desktop turns it on by default, with a switch in Settings). While
it is on and the embedder does not answer, the search returns the keyword results with the first line `(vector search
unavailable: keyword results only)`, once per session. While it is off, search is silent and the same as keyword-only.

The vectors of the notes live only in a cache on this computer (`vectors.sqlite3`). They are never written to a team
database. A note saved or changed through this server is sent to the embedder at once, and the embedder also finds what
was missed, so a note saved by a teammate gets its vector here within minutes.

### The embedder

One `harness-memory embedder` runs per computer and every `harness-memory mcp` asks it over a local socket. It holds the
model, embeds the notes of the stores this computer is set up for (the local file and the team database, as the
[routing](#which-notes-are-shared) says) and answers searches. Harness Desktop starts it, and you can run it by hand:

```sh
harness-memory embedder enable          # {"enabled": true}, writes embedder.toml (missing = disabled)
harness-memory embedder run             # serve in the foreground (the default of `embedder`)
harness-memory embedder status --json   # {"enabled", "running", "active_model", "pending_model", "model_present", "vectors"}
harness-memory embedder disable         # {"enabled": false}
harness-memory embedder remove-model    # delete the model files and the vectors, refused while it runs
```

`enabled` only decides whether `harness-memory mcp` asks the embedder. `disable` does not stop a running embedder.

- **One instance.** A second `embedder run` finds `embedder.lock` held, prints `{"event":"busy"}` and exits 0. The
  embedder exits when its standard input closes (the program that started it is gone) and on SIGTERM.
- **Events.** It prints one JSON object per line: `download` (`received`, `total`), `verifying`, `loading`, `ready`
  (`model`, `dim`), `backfill` and `reembed` (`store`, `done`, `total`), `model-update` (`from`, `to`), `switched`,
  `cleanup-done`, `idle`, `busy` and `error` (`message`).
- **Socket.** On Linux and macOS it listens on `embedder.sock` in the data folder, mode 0600. When that path is longer
  than 100 bytes it uses `harness-memory-<hash>.sock` in the temporary folder instead. A client refuses a socket that is
  not a socket or not yours. On Windows it is a named pipe, `harness-memory-embedder-<hash>`. Requests are one line of
  JSON per connection (`ping`, `search`, `upsert`, `forget`). A search waits at most 2 s and an update 200 ms.
- **Memory.** The model and tokenizer are unloaded after 10 idle minutes (`idle`) and loaded again by the next search.
  The vectors stay loaded.
- **Vector cache.** `vectors.sqlite3` keeps one vector per note and model, with a hash of the note text, plus the position
  it has read to in each store. The cache is local, and a store is named in it by a key that holds no secret (a hash of
  the file path, or of the team URL without its credentials and the database name).
- **The model.** `paraphrase-multilingual-MiniLM-L12-v2`, int8, 384 dimensions, read with tract. On the first start the
  embedder downloads two files, `model_quantized.onnx` (about 118 MB) and `tokenizer.json` (about 17 MB), from the
  harness-useful-memory GitHub release named in the build. Each file's SHA-256 and size are compiled in, and a file that
  does not match is not used. They are kept in `models/<id>/` in the data folder. Nothing else is downloaded, and it
  never reads from Hugging Face.
- **Mirror.** `HARNESS_MEMORY_MODEL_BASE_URL` replaces the folder URL the files are fetched from (`<base>/<file name>`),
  for an offline mirror or a test. It has to be `https://`, or `http://` to `127.0.0.1` or `localhost`. The hashes stay
  pinned, so a mirror cannot change the model.
- **Model updates.** A release that names a new model id downloads and verifies it in the background while the old one
  keeps answering. It then embeds every note again under the new id (`reembed`), switches in one step (`switched`) and
  deletes the old files and vectors (`cleanup-done`). If a step fails, the old model keeps serving, and the update is
  tried again at the next start and every hour.

### English notes

A store can ask agents to write notes in English, which the search and the stemming work best with. The setting is kept
in the store, so on a team database it is team-wide, and it is on when nothing is stored.

```sh
harness-memory settings get --json                    # {"english_notes": true}
harness-memory settings set english_notes=false --json
```

`set` writes every store a note scope saves to. A database this server could not extend (no right to change the schema)
has no settings storage and the command exits 1 with the reason.

With the setting on, an agent's `note_save` or `note_update` whose title and body are clearly not English is refused
before the save guards run, with "Rewrite the title and body in English and call the tool again. Nothing was saved.".
Nothing is held for review. Detection is conservative: it needs a reliable verdict over at least 40 letters of prose,
and code, paths and identifiers do not count, so short and technical notes pass. Your own saves (`notes save`, `held
save`) are never refused and only print a warning. `push` is not checked. A note already stored in another language gets
`lang` = `other`, and `note_get` adds a line asking the agent to rewrite it with `note_update`.

`notes list --json` gives each note a `lang` (`en`, `other` or `null` when it is not known), and `--lang en|other|unknown`
keeps one of them.

### Usefulness counters

Every note counts how often an agent was shown it (`shown`), how often an agent said it helped (`useful`) and when it
last did (`lastUsefulAt`). The counters start at 0 on every note, also those written before they existed. They live in
the same database as the note (the local file or the team database, also for a team note that is read-only here), and
counting changes nothing else of a note: not its update time, revision, hits, search index or language. A database an
older `harness-memory` made gets the columns on the first open of this version (`shown`, `useful`, `last_useful_at` on
SQLite, libSQL and PostgreSQL, the same fields on MongoDB and DocumentDB, where a missing field counts as 0), so no
schema version changes and the older version keeps working.

`note_search` counts each note its reply really shows as shown (a note the reply cap cuts is not counted), except for
the searches of Harness Desktop itself, which runs its own when saving or importing global notes (the MCP client named
`harness-desktop`). Reading a note with `note_get`, recall blocks and `notes list` count nothing. Agents are told to
call `note_feedback` after a search, which counts a note as found useful. The counters are advisory, a rough signal of
which notes get used and not a measure to trust:

- A note counts at most once as shown and once as useful per MCP session (one server process), however often it is
  found or named again, so one chatty agent cannot inflate it.
- Counting is best effort and never fails a call. The counter is written after the reply has gone out, in a single
  attempt that is dropped instead of waited for, and it is bounded so the next call of the session is held up for
  about a second at most. A dropped count is not retried later in the same session. A counter that cannot be written
  (a read-only database, a team database that is down) is dropped the same way, and with a team database split from
  this computer each side is counted on its own. The counting never opens the team database, nor copies notes saved
  offline: the call that asked for it did. A request the client cancels counts nothing, and its notes count with the
  next request that shows them. The bound per backend, while the server answers at all:
  - SQLite: no wait for the write lock, so there is no delay.
  - libSQL: 0.75 s per write. A counter is one self-closing request (the schema check, the counts, the commit and the
    close of the stream together), so a counter that gives up leaves nothing open on the server to hold the write
    lock. The server finishes such a request when it can, so a dropped count may still land a little later.
  - PostgreSQL: per statement, each of the four waits for a lock or runs for 0.25 s at most, about 1 s in all, and a
    closed connection is not opened again for a count. A server that drops packets silently is not bounded by that:
    every PostgreSQL connection gives up on data nobody acknowledges after 15 s (`tcp_user_timeout`) and probes an idle
    one with TCP keepalives (after 15 s, every 5 s, 3 times), so such a connection ends in about 20 to 30 s instead of
    the system's two hours. This never cuts a healthy connection, also not one waiting for a long statement. A URL
    parameter of the same name (`tcp_user_timeout`, `keepalives_idle`, `keepalives_interval`, `keepalives_retries`)
    sets its own value.
  - MongoDB and DocumentDB: 0.75 s per write, after which the write is left running on its own thread and the call
    goes on. A count is dropped while such a late write still runs.
- Shown counting reads the list of team notes hidden on this computer too: when it cannot be read, the team notes of
  that search are not counted as shown (this computer's own notes still are), and a hidden note is never counted.
  `note_feedback` does the same: team notes are then `not checked` and not counted.
- The skip of the `harness-desktop` client is noise reduction, not a trust check: the client name is whatever the
  client says it is, and any client can still call `note_feedback`.

`notes list --json` shows them on every note, sorts by them (`--sort useful|lastUseful`) and filters by them
(`--useful ever|never|any`). Never useful needs a note shown at least 3 times first, so a note nobody saw yet is not
flagged.

## Where data lives

One SQLite file, `memory.sqlite3`, in the `harness-memory` folder of your user data directory:

| OS | Folder |
| --- | --- |
| Linux | `$XDG_DATA_HOME/harness-memory/` (default `~/.local/share/harness-memory/`) |
| macOS | `~/Library/Application Support/harness-memory/` |
| Windows | `%LOCALAPPDATA%\harness-memory\` |

Set `HARNESS_MEMORY_DIR` to use another folder (the file is then `$HARNESS_MEMORY_DIR/memory.sqlite3`). It has to be set
in the environment the harness starts the server with, and it has to be absolute (a leading `~` is fine): a relative
value is ignored, because the server runs inside your project. Point it at a folder only you can write to: the scripts
run from `run` inside it, and the script approvals pass through `requests` there.

**Coming from Harness Desktop.** Harness Desktop used to keep the same database at
`<data dir>/com.mspiechowicz.harness-useful-desktop/memory/memory.sqlite3`. On first run, when there is no database in
the new place yet, the server copies that file over. The old one is left untouched. Harness Desktop versions that still
bundle their own server keep writing the old file, so notes saved through them after the copy stay there.

The embedder adds these files to the same folder: `embedder.toml` (`enabled = true|false`), `embedder.lock` (held while
it runs), `embedder.sock` (its socket, or in the temporary folder when the path is long), `vectors.sqlite3` (the note
vectors, for this computer only) and `models/<id>/` (the model files).

## Team memory

By default each person's notes live in their own SQLite file. **Team memory** points every teammate's `harness-memory`
at one shared database instead, so the whole team fills one project memory: a fix one person's agent saves is found by
the next teammate's agent in the same repository. Use it for a team that works on the same repositories. For one person,
or several machines of one person, the local file is simpler and needs nothing.

With a team database the whole store is shared by default, repository notes **and** global notes. There is no local
copy: if the database cannot be reached the server reports an error and does not fall back to an empty local one. To
keep one of the two on your own computer, see [Which notes are shared](#which-notes-are-shared).

**Choosing a backend.** The server needs one shared database it can reach over the network. Pick what you already run.

| Backend | URL scheme | Pick it when |
| --- | --- | --- |
| PostgreSQL | `postgres://`, `postgresql://` | You have a Postgres server or a managed one (RDS, Neon, Supabase, Cloud SQL) |
| libSQL | `libsql://`, `https://`, `http://` (localhost only) | You want the lightest option: one `sqld` container, or a hosted [Turso](https://turso.tech) database |
| MongoDB | `mongodb://`, `mongodb+srv://` | You already run MongoDB or Atlas. A replica set (Atlas always is one) gives transactions; a standalone server works too |
| Amazon DocumentDB | `mongodb://`, `mongodb+srv://` | Your company runs DocumentDB in AWS. See [Amazon DocumentDB](#amazon-documentdb) |

Every remote host uses TLS with certificate checks, unless the host is this machine (`localhost` or a loopback address):
for PostgreSQL a missing, `allow` or `prefer` `sslmode` becomes `require` and `disable` is refused; for libSQL plain
`http://` is refused; for MongoDB TLS is switched on when the URL does not say, and `tls=false`, `tlsInsecure` and
`tlsAllowInvalidCertificates`/`Hostnames` are refused. A MongoDB URL for this machine is only accepted without TLS when
it sets `directConnection=true` (or `loadBalanced=true`), so the driver cannot follow the replica set to other hosts in
the clear. A token or password for libSQL never goes in the URL. Try them on one machine first with
[`docs/team-memory/docker-compose.yml`](docs/team-memory/docker-compose.yml) (`docker compose -f
docs/team-memory/docker-compose.yml up -d --wait postgres`, `libsql` or `mongo`; the URLs are in the file's header).

A PostgreSQL connection to a network that goes silent ends in about 20 to 30 s (`tcp_user_timeout` 15 s, TCP keepalives
after 15 s, every 5 s, 3 times) instead of hanging; a healthy connection, also one waiting for a long statement, is
never cut. The URL parameters of the same names set your own values.

### Set up the database

Give each team its own database and credentials: **whoever holds the credentials can read and write all of the team's
notes**, and the repository key is not an access boundary.

**PostgreSQL** (version 15 or newer). [`docs/team-memory/postgres-setup.sql`](docs/team-memory/postgres-setup.sql)
creates a `harness_memory` database and a role that may only connect to it and create its own tables (no superuser,
no other databases). Run it once as an admin; running it again only resets the password:

```sh
psql "postgres://postgres@db.example.com/postgres?sslmode=require" \
  -v memory_password='a-long-random-password' -f docs/team-memory/postgres-setup.sql
# URL for the team: postgres://harness_memory:a-long-random-password@db.example.com:5432/harness_memory?sslmode=require
```

URL-encode special characters in the password (`@` is `%40`, `/` is `%2F`).

Behind a connection pooler (Neon, Supabase, PgBouncer and the like) use the direct connection or a session-mode pooler,
not transaction-mode pooling: the server sets session settings and uses named prepared statements on its connection,
which a transaction-mode pooler hands to other clients between transactions.

**libSQL.** Either create a [Turso](https://turso.tech) database and a token (`turso db create harness-memory`, `turso
db show harness-memory --url`, `turso db tokens create harness-memory`), or run `sqld` yourself behind a TLS reverse
proxy (Caddy, nginx, a cloud load balancer):

```sh
docker run -d --name sqld -p 127.0.0.1:8080:8080 -v sqld-data:/var/lib/sqld \
  -e SQLD_NODE=primary -e SQLD_HTTP_LISTEN_ADDR=0.0.0.0:8080 -e SQLD_DB_PATH=/var/lib/sqld/data.sqld \
  -e SQLD_AUTH_JWT_KEY="$(cat jwt-public-key.b64)" \
  ghcr.io/tursodatabase/libsql-server:latest
```

`sqld` without `SQLD_AUTH_JWT_KEY` accepts anyone who can reach it, so never expose that one. With a key, the database
token is a JWT signed by the matching private key (`sqld --help` describes the key format). The URL is
`libsql://sqld.example.com` (or `https://...`), plus the token. The libSQL client ignores `HTTP_PROXY`, `HTTPS_PROXY`
and `ALL_PROXY`: it connects to the host in the URL directly.

**MongoDB.** [`docs/team-memory/mongo-setup.js`](docs/team-memory/mongo-setup.js) creates a `harness_memory` user that has
`readWrite` on the `harness_memory` database and nothing else:

```sh
HARNESS_MEMORY_DB_PASSWORD='a-long-random-password' \
  mongosh "mongodb://admin@db.example.com/admin?tls=true" --file docs/team-memory/mongo-setup.js
# URL for the team: mongodb://harness_memory:a-long-random-password@db.example.com/harness_memory?tls=true&replicaSet=rs0
```

The database is the URL path (`harness_memory` when there is none), and unless the URL sets `authSource` the user is
looked up in that database too. With a replica set or a sharded cluster every write runs in a transaction, so a note,
its entities and its relations change together. A standalone server works as well: no update is lost (a note is only
replaced at the revision it was read at), but a write that touches several documents is not atomic as a whole. Search
ranks by MongoDB's text score, which ignores how rare a word is, so close results can order differently than with
SQLite or PostgreSQL. For a private CA add `tlsCAFile=/path/to/ca.pem`. `tlsCAFile` and `tlsCertificateKeyFile` must be
absolute paths, for a server on this machine too: the server runs inside your project, so a relative path is refused
rather than read from there. `tlsCAFile=global-bundle.pem` (or `rds-combined-ca-bundle.pem`), relative or at a path
that does not exist, means the Amazon RDS bundle built into harness-memory (see [Amazon DocumentDB](#amazon-documentdb)).

### Amazon DocumentDB

[Amazon DocumentDB](https://aws.amazon.com/documentdb/) speaks the MongoDB protocol, so it takes a `mongodb://` URL and
works like MongoDB above. It is built for instance-based clusters on engine 4.0 or newer, checked against the AWS compatibility lists and a MongoDB server in DocumentDB mode but not yet against a real cluster. Elastic clusters are detected
by their host name too, but they have no transactions, so writes run without them (as on a standalone MongoDB server),
and they are untested. The server says so on stderr when it connects to one.

DocumentDB keeps every user in the `admin` database. Create the team's user there, with `readWrite` on the notes
database only (in `mongosh`, connected as the cluster's admin user):

```js
db.getSiblingDB("admin").createUser({
  user: "harness_memory",
  pwd: "a-long-random-password",
  roles: [{ role: "readWrite", db: "harness_memory" }],
})
```

The connection string the AWS console shows works as it is, with this user and password in it. Without a database in
the path the notes go to `harness_memory`. A user and password log in against `admin`, where DocumentDB keeps its
users, unless the URL sets `authSource`.

- **Which kind.** A URL whose host ends in `.docdb.amazonaws.com` (or `.docdb-elastic.amazonaws.com`, and the
  `.amazonaws.com.cn` variants) is taken for DocumentDB. For any other host name, set
  `database_kind = "documentdb"` in the config file, or `HARNESS_MEMORY_DATABASE_KIND=documentdb` next to
  `HARNESS_MEMORY_DATABASE_URL`. The kind is read from the same place as the URL: the variable only goes with the
  variable's URL, the key only with the file's. `database_kind = "mongodb"` turns the detection off. Any other value,
  or a kind with a URL that is not `mongodb://` or `mongodb+srv://`, is an error. Switching the kind of one URL keeps it
  the same team database (script approvals included).
- **Certificates.** DocumentDB's certificates are signed by the Amazon RDS certificate authorities, which are not in
  your system's trusted roots. harness-memory has [their bundle](https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem)
  built in and hands it to each new connection in a private temporary file, removed once the connection has read it,
  so you need no file of your own. The connection string the AWS console shows works as it is: its
  `tlsCAFile=global-bundle.pem` (or `rds-combined-ca-bundle.pem`) means the built-in bundle. Any other `tlsCAFile`
  must be an absolute path to an existing file. TLS stays required, with certificate checks.
- **Retryable writes** are off unless the URL sets `retryWrites` (DocumentDB before engine 8.0.2 refuses them).
- **Reads from the primary.** Every read goes to the cluster's primary instance, whatever `readPreference` the URL
  sets (the AWS console string's `secondaryPreferred` included). Writes run in transactions, which only the primary
  runs, and a replica can lag behind it, so a read there could miss a note saved a moment earlier.
- **VPN.** A cluster only accepts connections from inside its AWS network (its VPC). Connect to your company VPN
  first. When the cluster cannot be reached, at startup or later in a session, the error says to connect to the VPN.
  When the cluster answers but its certificate cannot be verified, the error says that instead.
- **Search.** DocumentDB cannot build MongoDB's text index (none before engine 5.0, and none on the tags array or
  without a language after it), so on DocumentDB the database gets no text index and search matches the words each
  note was saved with. Notes rank with the same title, body and tag weights and the same formula as MongoDB's text
  score, computed by harness-memory, but from the 100 most recently updated matching notes rather than the 100 best,
  which only matters when more than 100 notes match.

```toml
database_url = "mongodb://harness_memory:...@docs.cluster-abc123.eu-central-1.docdb.amazonaws.com:27017/?tls=true&tlsCAFile=global-bundle.pem&replicaSet=rs0&readPreference=secondaryPreferred&retryWrites=false"
# database_kind = "documentdb"   # only needed when the host is not a docdb.amazonaws.com name
```

If `harness-memory check` against DocumentDB says the notes text index could not be created, the URL was taken for
MongoDB: set `database_kind = "documentdb"`.

### Configure each teammate

Give every teammate the URL (and, for libSQL, the token) through one of these. The environment variable wins when both are
set, and an empty value counts as unset.

**Config file** (recommended: the secret stays out of every harness's settings). Copy
[`docs/team-memory/config.example.toml`](docs/team-memory/config.example.toml) to the file below, fill it in and make it
private:

| OS | File |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME/harness-memory/config.toml` (default `~/.config/harness-memory/config.toml`) |
| macOS | `~/Library/Application Support/harness-memory/config.toml` |
| Windows | `%APPDATA%\harness-memory\config.toml` |

```toml
database_url = "postgres://harness_memory:...@db.example.com:5432/harness_memory?sslmode=require"
# database_token = "..."   # libSQL only
```

```sh
chmod 600 ~/.config/harness-memory/config.toml     # Linux and macOS: only you can read it
```

`HARNESS_MEMORY_CONFIG` points at a config file somewhere else (an absolute path, `~` allowed; the file must exist). An
unknown key in the file is an error, so a typo stops the server instead of quietly using the local database. On Linux
and macOS the server also refuses a config file (the default one or `HARNESS_MEMORY_CONFIG`'s) that the group or other
users can change, or that another user owns, since whoever can change it can point your agents at their database; it
warns when others can read it.

**Environment variables**: `HARNESS_MEMORY_DATABASE_URL` and, for libSQL, `HARNESS_MEMORY_DATABASE_TOKEN` (and, for
[Amazon DocumentDB](#amazon-documentdb) on another host name, `HARNESS_MEMORY_DATABASE_KIND`). Like
`HARNESS_MEMORY_DIR`, they must be in the environment the harness starts the server with, and an app started from a
launcher does not see your shell's variables. Hand them to the server in the harness's own entry, which stores the
secret in that harness's settings file:

```sh
claude mcp add --scope user harness-memory -e HARNESS_MEMORY_DATABASE_URL='postgres://...' -- harness-memory mcp
codex mcp add harness-memory --env HARNESS_MEMORY_DATABASE_URL='postgres://...' -- harness-memory mcp
```

In `~/.codex/config.toml` that is a `[mcp_servers.harness-memory.env]` table with the variable in it. omp and pi take an
`"env": { "HARNESS_MEMORY_DATABASE_URL": "..." }` key next to `command` in `mcp.json`, and OpenCode an
`"environment": { ... }` key in its `local` entry.

To check it, run `harness-memory check`. It reads the settings the server would read, opens the database the same way
and prints one line, without the credentials, then where scripts are kept (`scripts: team database`,
`scripts: this computer` or `scripts: team database and this computer`, followed by `team scripts: trusted
automatically` when `trust_team_scripts` is on and scripts are read from the team database):

```text
ok: PostgreSQL at postgres://harness_memory@db.example.com:5432/harness_memory?… (schema 4)
scripts: team database
```

It exits with 0 when the database opens (the first line then ends with `update it` when the database was set up by a newer
`harness-memory`), and with 1 and the reason on stderr when it does not. Without a team database it names the local file
(`ok: local database at …`, with `(not created yet)` before the first note); that file is only looked at, never
created. With both kinds of notes kept on this computer and scripts in the team database, it opens the team database
for the scripts and fails the same way when it cannot. Run it with the same variables as the harness's entry when the settings live there.

`harness-memory kinds` prints the team database kinds this `harness-memory` supports, one per line (`postgres`,
`libsql`, `mongodb`, `documentdb`), and exits with 0. Older versions do not know the command.

The server checks the same at startup: a wrong URL, a refused plain connection or an unreachable database stops it with
the reason on stderr, so the harness shows `harness-memory` as failed to start (its MCP log or status view has the
message). A connection lost later in a session comes back as a tool error. The credentials are never printed.

### Which notes are shared

By default both kinds of notes go to the team database. Two optional keys keep one kind on this computer, in the local
SQLite file, while the other is shared, or read the team's notes of a kind while saving your own on this computer:

| Key | Environment variable | Values | Default |
| --- | --- | --- | --- |
| `repository_notes` | `HARNESS_MEMORY_REPOSITORY_NOTES` | `"remote"` (team database), `"local"` (this computer) or `"remote+local"` (read both, save here) | `"remote"` |
| `global_notes` | `HARNESS_MEMORY_GLOBAL_NOTES` | `"remote"` (team database), `"local"` (this computer) or `"remote+local"` (read both, save here) | `"remote"` |

```toml
database_url = "postgres://..."
global_notes = "local"   # share repository notes with the team, keep personal global notes here
# repository_notes = "remote+local"   # see the team's repository notes, save new ones on this computer only
```

The environment variable wins over the key, and any other value is an error that stops the server (`hooks` and
`routines` take `"remote"` or `"local"` only). With both set to `"local"` the team database is not used at all (the
server says so on stderr). The keys only matter with a team database.

With one kind kept locally, the tools still see both: a search lists the repository's notes first and fills up with
global ones, and `note_get`, `note_update` and `note_delete` find a note by its id in either place. Local notes
are numbered from 1000000001 (local note 7 is note 1000000007), so their ids never clash with the team's, and team
notes keep the ids teammates see. Relations stay in the scope they are linked in, so a relation's note must be in the
same place, and similar notes are only looked for next to the note just saved. Notes left in the other place from
before you changed the keys are not shown; `push` copies local notes of a shared kind into the team database.
`harness-memory check` adds where each kind goes (`; repository notes: team database, global notes: this computer;
local database at …`).

With a kind set to `"remote+local"` the tools read the team's notes of that kind and this computer's, and every note
they save goes to this computer. The team's notes of that kind are read-only there: `note_update`, `note_delete` and
`entity_link` with a team note's id are refused, and similar notes listed after `note_save` are marked `(team,
read-only)`. Searches take turns between the two places, and similar notes are looked for in both. `notes delete --all`
deletes only this computer's notes of that kind and counts the team's that stay (`kept`).

### Offline fallback

With a team database a scope set to `"remote"` normally fails while the team database cannot be reached (VPN off, server
down). By default (`offline_fallback = true`, or `HARNESS_MEMORY_OFFLINE_FALLBACK=1`) harness-memory serves that scope
from this computer meanwhile: a scope set to `"remote"` or `"remote+local"` is read and saved in the local SQLite file,
the team's notes and scripts are not shown, and every reply of an MCP tool, and `notes list` / `scripts list` with
`"offline": true`, say so. The server prints the reason on stderr and does not try the team database again for a
minute. The notes and scripts saved this way for a `"remote"` scope are listed in `pending-sync.json` next to the
local database (by id, scope, title or name and the team database they were meant for; the file is private to you);
when that team database opens again (at the next call, or when a later server starts) they are copied to it as
`harness-memory push` copies them (a note with the title of a teammate's newer note stays as it is) and leave the list;
a copy that fails is tried again after a minute. Nothing is deleted from this computer, and notes you keep on this
computer on purpose, or whose scope you have since set to `"local"`, are never copied. Only new notes and scripts are
copied: a change you make offline to a note or script that already has a team copy, and relations between entities
made without a note, stay on this computer. The ids differ too: a note saved offline gets its team id when it is
copied. Hooks and routines have no local copy: `hooks list` and `routines list` print
`{"error": "failed", "message": ...}` while the team database is down.

Set `offline_fallback = false` (or `HARNESS_MEMORY_OFFLINE_FALLBACK=0`) to turn it off, for example when notes must
never be kept on this computer: the calls that need the team database then fail with the reason and how to change the
settings. The fallback decides when the team database is opened; a connection that breaks after it was opened is not
covered, and its calls fail with the database's error.

A team note you do not want to see can be hidden on this computer, with `harness-memory notes hide <id>` (or Hide on
this computer in Harness Desktop), whatever the kind is set to: the team database is not changed, but the note is left
out of every search, list, `note_get`, similar notes and `entity_neighbors` here, also for servers that are already
running. `notes list --hidden` lists the hidden ones and `notes unhide <id>` shows one again. The list is kept per team
database in the local SQLite file (at most 500 notes).

### Where scripts are kept

Scripts go to the team database too, unless `scripts` (or `HARNESS_MEMORY_SCRIPTS`) says otherwise: `"remote"` (the
default), `"local"` (this computer) or `"remote+local"` (read the team's scripts and this computer's, save new ones
here). Like the note keys it only matters with a team database.

```toml
scripts = "remote+local"   # run the team's scripts, keep the ones your agents save on this computer
```

With `"remote+local"` both are listed (this computer's first on the same name), and a name is looked up on this
computer first: its repository scripts, the team's, its global ones, the team's. Local scripts are numbered from
1000000001, like local notes. The team's scripts are read-only there (`script_delete` with a team script is refused,
naming `harness-memory scripts hide`), and `scripts delete --all` deletes only this computer's. Each script keeps its
own approval: a team script is approved under the team database (and `trust_team_scripts` trusts the team's only), a
local one on this computer, by the id it has there.

A team script you do not want to see can be hidden on this computer, with `harness-memory scripts hide <id>` (or Hide
on this computer in Harness Desktop), with `"remote"` and `"remote+local"` alike: it is left out of `script_list`,
`script_get`, `script_run`, the scripts `note_search` lists and `scripts list`, also for running servers, and saving a
script under its name with `"remote"` is refused. `scripts list --hidden` lists the hidden ones and
`harness-memory scripts unhide <id>` shows one again (`--json` prints `{"hidden": <id>}` or `{"unhidden": <id>}`); a
script saved on this computer cannot be hidden (delete it instead). The list is kept per team database in the local
SQLite file (at most 500 scripts). When it cannot be read, nothing is hidden, but nothing in the team database is
changed either.

With `"remote"` the scripts left in the local file are not read. `harness-memory scripts push` copies them into the
team database (see [Seed it from existing notes](#seed-it-from-existing-notes)), and `harness-memory scripts
discard-local (--all | ID[,ID...] | -) [--scope repo|global] --yes [--json]` deletes them by their local ids (`localId`
in `scripts push --json`, `-` reads them from stdin), only while `scripts = "remote"`.

### Sharing hooks

[Harness Desktop](https://github.com/MSpiechowicz/harness-useful-desktop) hooks (small JSON files that add context to a
prompt, send a follow-up or run a command in a desktop chat) can be shared through the team database. Harness Desktop
does it for you: its Settings turn sharing on, its Hooks view shares a hook and shows the team's. It reads and writes
them with the commands below, never by connecting to the database itself. This server never runs a hook.

There is no MCP tool for hooks, but the commands below are not limited to Harness Desktop: any process running as you,
an agent with a shell tool included, can run `harness-memory hooks put` and share a hook with everyone on the team
database.

| Key | Environment variable | Values | Default |
| --- | --- | --- | --- |
| `hooks` | `HARNESS_MEMORY_HOOKS` | `"remote"` (share through the team database) or `"local"` (share none) | `"local"` |
| `trust_team_hooks` | `HARNESS_MEMORY_TRUST_TEAM_HOOKS` | `true` (`1`) or `false` (`0`) | `false` |

```toml
database_url = "postgres://..."
hooks = "remote"
# trust_team_hooks = true   # team hooks run without your approval in Harness Desktop
```

`hooks = "remote"` only counts with a team database (without one it is ignored, like `scripts = "remote"`).
`trust_team_hooks` is only passed on to Harness Desktop (and only while hooks are shared): with it on, a team hook runs
there without your approval, otherwise it waits until you approve that exact version. With it on, anyone who can write
to the team database, and any agent of a teammate that can run `harness-memory`, can run commands on your computer.
Leave it off unless you trust everyone with access to the team database.

The team database keeps one row per hook in `hooks` (a collection in MongoDB): its `id` (the 32 hex id of the desktop
file it was shared from, unique), `repo` (the repository key `git:host/owner/repo` of a project hook, at most 512 bytes, `null` for a
global one), `body` (the hook file bytes as written, at most 16384), `sha256` (of the body), `author` (a free-text label, at most
80 characters), `created_at` and `updated_at`. A scope (the global one, and each repository) holds at most 64 hooks.
The body is a version 1 hook file of Harness Desktop (`docs/hooks-format.md` there) with two team rules: no `seed`
(Harness family hooks are never shared, every computer adds its own), and a project hook's `project` holds the
repository key instead of a local path. So only the repository identity leaves your computer: a project hook in a
repository without an `origin` remote (a `path:` key) cannot be shared. A body that holds a credential is refused too.

```sh
harness-memory hooks config --json
harness-memory hooks list --json --project ~/src/widget --offset 0 --limit 50
harness-memory hooks get 1a2b3c4d5e6f70818293a4b5c6d7000b --json
harness-memory hooks put --json --project ~/src/widget < hook.json
harness-memory hooks put --json --project ~/src/widget --expect-sha <sha256> --author "Ada" < hook.json
harness-memory hooks delete 1a2b3c4d5e6f70818293a4b5c6d7000b --expect-sha <sha256> --json
```

- `hooks config --json` prints `{"hooks": "remote"|"local", "trustTeamHooks": bool}` whenever the settings load
  (`"local"` without a team database; `trustTeamHooks` is `false` unless hooks are shared), so Harness Desktop can tell
  whether this `harness-memory` has these commands.
- `hooks list --json` prints one page, `{"total", "nextOffset", "project", "hooks"}`: the global hooks and those of
  `--project`'s repository, global first, then by `createdAt`, then by id. `project` is `{"key", "shareable"}`, the
  repository key of `--project` and whether it may be shared (a `git:` key; a `path:` key is never sent to the
  database), `null` without `--project`. `--offset N` and `--limit N` (1 to 200, 50 by default) pick the page.
  `nextOffset` is the `--offset` of the next page, `null` after the last one: page with it rather than by counting the
  hooks, since rows left out (see below) make a page shorter but still move the offset. Without sharing it lists none.
- `hooks get <id> --json` prints one hook.
- `hooks put --json` reads a hook file from standard input. Without `--expect-sha` it adds a new hook (an id that is
  taken is a `conflict`), with it it replaces the hook with that id while its `sha256` is still the one given (a missing
  id is `notFound`, another `sha256` a `conflict`). A project hook needs `--project`, whose repository key must equal
  the file's `project`. The author is `--author`, else your user name (`USER`, `USERNAME`). It prints the stored hook.
- `hooks delete <id> --json` prints `{"deleted": bool}` (`false` when no hook has the id); with `--expect-sha` only
  while the hook's `sha256` is still the one given.

A hook is `{"id", "repo", "sha256", "author", "createdAt", "updatedAt", "body"}` (RFC 3339 UTC times). A refusal exits
with 1 and prints `{"error", "message"}`, the message also the last line of stderr: `conflict`, `notFound`, `invalid`
(the body breaks the hook file format, or a bad author), `refused` (a Harness family hook, a repository without an
`origin` remote, a `project` that is not the repository of `--project`), `limit` (the scope holds 64 hooks) or `local`
(hooks are not shared). Other failures (the settings, the database) exit with 1 and only the stderr line, usage errors
with 2. A row changed by hand that this server never writes is left out of the list and of `get`, with a line on stderr:
one `put` would refuse (an id that is no hook id, a repository that is no `git:` key, a bad author, a body that breaks
the rules above or names another id or scope than its row). `total` still counts it.

### Sharing routines

Harness Desktop routines (JSON files that run an agent prompt, a command or a saved script on a schedule) are shared the
same way as [hooks](#sharing-hooks): Harness Desktop's Settings turn sharing on, its Routines view shares a routine and
lists the team's as a catalogue, each routine arriving turned off until you turn it on on your computer. It reads and
writes them with the commands below. This server never runs a routine.

| Key | Environment variable | Values | Default |
| --- | --- | --- | --- |
| `routines` | `HARNESS_MEMORY_ROUTINES` | `"remote"` (share through the team database) or `"local"` (share none) | `"local"` |
| `trust_team_routines` | `HARNESS_MEMORY_TRUST_TEAM_ROUTINES` | `true` (`1`) or `false` (`0`) | `false` |

```toml
database_url = "postgres://..."
routines = "remote"
# trust_team_routines = true   # team routines you turned on run without your approval in Harness Desktop
```

`routines = "remote"` only counts with a team database, and `trust_team_routines` only while routines are shared. With
it on, every team routine you turned on runs in Harness Desktop without your approval of that version, commands and
agent routines that run tools without asking included. Anyone who can write to the team database, and any agent of a teammate
that can run `harness-memory`, can then run commands and agents on your computer once you turned their routine on.

The team database keeps one row per routine in `routines` (a collection in MongoDB), with the columns of `hooks` and the
same limits: a 32 hex `id`, `repo` (a `git:` repository key, `null` for a global routine), `body` (at most 16384
bytes), `sha256`, `author`, `created_at` and `updated_at`, at most 64 routines per scope. The body is a version 1
routine file of Harness Desktop (`docs/routines-format.md` there): its schedule (`when`, a cron expression included,
which must be able to run), its action, and the shared rules of every file. `seed` and `requires` are never set, a
project routine's `project` holds the repository key instead of a local path, and a body that holds a credential is
refused.

```sh
harness-memory routines config --json
harness-memory routines list --json --project ~/src/widget --offset 0 --limit 50
harness-memory routines get 1a2b3c4d5e6f70818293a4b5c6d7000b --json
harness-memory routines put --json --project ~/src/widget < routine.json
harness-memory routines put --json --project ~/src/widget --expect-sha <sha256> --author "Ada" < routine.json
harness-memory routines delete 1a2b3c4d5e6f70818293a4b5c6d7000b --expect-sha <sha256> --json
harness-memory routines delete --mine --json
```

The commands, their replies and refusals are those of `hooks`, with `routines` for `hooks`:
`routines config --json` prints `{"routines": "remote"|"local", "trustTeamRoutines": bool}`, `routines list --json`
prints `{"total", "nextOffset", "project", "routines"}`, and a refusal for a body that breaks the routine file format is
`invalid`. One command is added: `routines delete --mine [--author NAME] --json` deletes every team routine whose author
is `--author`, else the user name `put` records for you (`USER`, else `USERNAME`), in every scope, and prints
`{"deleted": N, "ids": [...]}` (`local` when routines are not shared). Without `--author` and without a usable user
name it is `refused`: it never deletes the routines `put` recorded as `unknown`, which may be anyone's. The author is a label anyone can write, so this
is a way to clean up after yourself, not a permission.

### Seed it from existing notes

Notes people already have in their local SQLite file can be uploaded once. `push` copies the local notes and relations into
the configured team database, and it is safe to repeat: a note that is already there is updated or skipped, never
duplicated, and a relation that is already there is left alone.

```sh
harness-memory push --dry-run              # list what would be shared, per repository, and change nothing
harness-memory push                        # push every repository's notes and the global ones
harness-memory push --repo . --no-global   # only this checkout's repository, no global notes
```

It reads the local file (`HARNESS_MEMORY_DIR` applies) and writes to the database in your configuration. Each teammate can
push their own notes. By default `push` takes everything: every repository's notes and the global ones. A kind kept on
this computer (`repository_notes` or `global_notes` set to `"local"`) is left out, and `push` says so. A kind set to
`"remote+local"` is left out too unless `--scope repo` or `--scope global` names it: its new notes are meant to stay on
this computer. `--scope repo|global` pushes only that kind.

- **Review first.** Everything you push becomes visible to the whole team. `--dry-run` lists, under each repository key
  (and `global`), the titles of the notes it would add or change. Repositories without a remote are keyed by their
  local path (`path:/home/you/notes`) and may be private; `push` prints a note when it includes any.
- **Narrow it.** `--repo <key>` pushes one repository's notes and relations: a key as `--dry-run` lists it
  (`git:github.com/acme/widget`, `path:/home/you/notes`), a remote (`github.com/acme/widget` or its clone URL), or `.`
  for the repository of the folder you run it in (found the way the server finds it). `--no-global` leaves the global
  notes out.
- **It can replace a teammate's text.** A note with the same title in the same repository (or in global) is skipped
  when it is identical or the team's copy is as new or newer, and replaced when your local copy was changed later. So
  `push` can overwrite a teammate's text of a same-titled note when your copy is newer: a conflict. By default it does,
  names every such note (`"Title" (key): overwrote the team's version`) and counts them as overwritten, apart from
  updates that only changed the kind or tags. `--overwrite none` keeps the team's text of every conflict, `--overwrite
  all` replaces it, and `--overwrite 12,40` (local note ids, or `-` to read them from stdin) replaces only those.
- **Move instead of copy.** `--delete-copied` deletes from this computer the notes the team database now holds as they
  are here (added, updated, overwritten or identical) once the push is done. Conflicts it kept, notes the team changed
  later and notes that failed to push stay. So does a note the team already had (updated, overwritten or identical)
  when the team's note lacks some of its linked entities, since only an added note takes its entities along. When a
  relation fails to push, nothing is deleted and the run reports it (the relation of a note that failed stays here
  with its note, and the rest is deleted). It does not go with `--dry-run`.
- **Hidden notes stay as they are.** A team note you hid on this computer is never written over. A local note with
  its title (in the same repository, or in global) that would be copied there fails instead, names `harness-memory
  notes unhide <id>`, and stays on this computer. `--dry-run` reports it the same way. If the list of hidden notes
  cannot be read, nothing is pushed.

It prints how many notes were added, updated, overwritten, kept and skipped, and how many relations were added or
already there. If the team database was set up by a newer `harness-memory`, `push` stops before writing anything:
update this one first.

`--json` prints one object instead, the same for a dry run and a real one, exit 0 when it worked or there was nothing to
push, 1 when a note or relation failed:

```json
{"dryRun": true, "nothing": null,
 "notes": {"added": 1, "updated": 0, "overwritten": 0, "kept": 1, "identical": 0, "teamNewer": 0, "failed": 0},
 "relations": {"added": 0, "unchanged": 0}, "entitiesCut": 0, "deleted": null,
 "changes": [{"localId": 12, "scope": "repo", "repo": "github.com/acme/widget", "title": "Deploy steps",
   "action": "keep", "conflict": true, "teamId": 34, "localUpdatedAt": "2026-10-19T08:00:00Z",
   "teamUpdatedAt": "2026-10-18T17:30:00Z", "deleted": false, "changedSince": false, "failed": false}],
 "failed": []}
```

`nothing` says why there was nothing to push (otherwise `null`). Each change has an `action` of `add`, `update`,
`overwrite`, `keep`, `identical` or `teamNewer`, and `conflict` marks a newer local note whose text differs from the
team's, whichever way `--overwrite` decided it. A change whose note could not be written has `failed: true`, keeps
its `action`, and is not counted in `notes` (`notes.failed` counts it). `deleted` counts what `--delete-copied` deleted
(`null` without it), and `changedSince: true` marks a copied note it left because the note changed on this computer
after the push read it. A relation whose note failed is not pushed either and is listed in `failed` with that note's
`localId`. The relations of a note whose team note you hid and the push left as it is are not linked to it and count as
`unchanged`.
`harness-memory scripts push` does the same for the scripts saved on this computer, with the same options: a script
matches the team's of the same scope, repository and name, and is `identical` (same code, description and timeout),
`teamNewer` (the team's changed at or after yours), `update` (same code, another description or timeout), or a
conflict (`overwrite` or `keep`, as `--overwrite` says, with local script ids). It pushes while `scripts = "remote"`,
with `"remote+local"` only when `--scope` names the scope, never with `"local"`. A script this computer trusts stays
trusted once copied (one waiting for approval never becomes trusted), a team script hidden on this computer is never
written over (it fails, in a dry run too), and `--delete-copied` deletes a local script only when it did not change since it was read. Its `--json`
object has `scripts` totals instead of `notes`, no `relations` or `entitiesCut`, and each change a `name` and
`trusted` (true when this computer's trust went along):

```json
{"dryRun": false, "nothing": null,
 "scripts": {"added": 1, "updated": 0, "overwritten": 0, "kept": 0, "identical": 0, "teamNewer": 0, "failed": 0},
 "deleted": null,
 "changes": [{"localId": 3, "scope": "repo", "repo": "github.com/acme/widget", "name": "open-prs", "action": "add",
   "conflict": false, "teamId": 9, "localUpdatedAt": "2026-10-19T08:00:00Z", "teamUpdatedAt": null,
   "deleted": false, "changedSince": false, "failed": false, "trusted": true}],
 "failed": []}
```

`harness-memory features` lists what this build supports, one per line (`push-json`, `scripts-remote-local`,
`scripts-hide`, `scripts-push-json`, `settings`, `notes-lang`, `notes-useful` and `embedder` among them), so a tool can
check before it asks for them. `settings` is the `settings` command, `notes-lang` the `lang` of a note and the `--lang`
filter, `notes-useful` the usefulness counters of a note with `--useful` and `--sort useful|lastUseful`, and `embedder`
the `embedder` command.

### Security and backups

- **The credential is the access control.** Anyone with the URL can read, change and delete every note, and a notes
  database has no per-repository or per-person permissions. Keep teams that must not see each other's notes in separate
  databases with separate credentials, share the credential through your password manager, and rotate it (change the
  password or token, update the teammates) when someone leaves.
- **Notes stay untrusted.** In a team, a note may be written by another person's agent. The same framing, the check against
  the code and the secret redaction apply, and a planted note can still try to steer everyone's agent.
- **TLS.** Connections to any host except `localhost` need TLS, so the notes and the credential do not cross the network
  in the clear. Use `sslmode=require` (or `verify-full` with your CA) for PostgreSQL.
- **Backups are yours.** The database is the only copy. Use your provider's backups, or `pg_dump harness_memory`,
  `mongodump --uri ...`, a snapshot of the `sqld` volume (or `turso db shell harness-memory .dump`).

## Privacy and trust

- **Local by default.** Without a team database the server opens no network connection, except that the embedder
  downloads its search model once when vector search is on (and again when a release names a new model), from a
  harness-useful-memory GitHub release, checked against a SHA-256 compiled into the build. It talks MCP to the process that
  started it, and the only other programs it runs are `git`, to find the repository (see above), and the scripts agents
  save and run with `script_run` (see [Scripts](#scripts)). It reads no other file of
  your project. When you configure a [team database](#team-memory), it connects to that one server and every note, global
  ones included, is stored there, readable by everyone who has its credentials.
- **Notes are untrusted data.** Agents write them, they may be wrong or outdated, and a planted note can try to steer a
  later session. Results say where each note came from, and the server tells agents to check notes against the code and
  never to follow instructions found inside one. That cannot make a note safe against a model that obeys what it reads:
  treat the memory like any other file an agent can edit.
- **Scripts run as you.** A script can do anything you can, with your environment. A machine runs only scripts saved on
  it or approved by you there (see [Approving scripts](#approving-scripts)), and an agent cannot approve one through the
  MCP tools: read a teammate's script before you approve it. Code your own agent saves runs without approval, by
  design. With `trust_team_scripts = true`, whoever can write to the team database can run code on your computer.
- **Team hooks are remote code.** With `hooks = "remote"`, Harness Desktop runs the hooks teammates shared only after you
  approve each version, or straight away with `trust_team_hooks = true`: whoever can write to the team database can
  then run commands on your computer. No MCP tool shares a hook, but an agent with a shell can run `harness-memory hooks
  put`, so keep `trust_team_hooks` off unless you trust everyone with access to the team database. A shared hook
  carries the repository key, never a local path.
- **Team routines are remote code too.** With `routines = "remote"`, a routine a teammate shared runs on your computer
  only after you turn it on, and then only after you approve that version, or straight away with
  `trust_team_routines = true`, which covers every team routine, commands and agents that run tools without asking
  included. Keep `trust_team_routines` off unless you trust everyone with access to the team database.
- **Secrets are redacted on write.** Titles, bodies, tags, entity names and relation labels pass a secret filter before
  they are stored. It is a heuristic and does not catch everything, so do not rely on it. Agents are also told never to
  store secrets or personal data.
- **No access control.** The repository key is only a name: any agent that can start the server can read and write any
  repository's notes. With a team database, who may do that is decided by who has its credentials. A same-title note written from another checkout, or by an agent that names another repository by
  path, is reported instead of replaced.
- **Not encrypted.** The local database is a plain file in your user data directory, outside any repository, and a team
  database is as protected as its server and TLS make it. What an agent reads
  from it goes to its model provider like the rest of its context.

## Development

Prerequisites: Rust via [rustup](https://rustup.rs) (`rust-toolchain.toml` selects the pinned toolchain, with clippy and
rustfmt) and [bun](https://bun.sh) 1.4+ for the release scripts.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked     # target/release/harness-memory
cargo deny check                   # supply-chain policy (deny.toml), needs cargo-deny
bun install && bun test            # the release version script
shellcheck install.sh
```

Source builds report `<version>-dev` (for example `harness-memory 1.0.1-dev`), only the release pipeline
(`HARNESS_MEMORY_RELEASE=1`) prints the plain version.

Try the server by hand with a throwaway database:

```sh
HARNESS_MEMORY_DIR=$(mktemp -d) target/release/harness-memory mcp
```

CI runs the Rust checks on Linux, macOS and Windows.

## Releasing

Releases are automatic. After CI passes on `main`, [semantic-release](https://semantic-release.gitbook.io) reads the
[Conventional Commits](https://www.conventionalcommits.org) since the last tag: `fix:` makes a patch release, `feat:` a
minor one, and a `BREAKING CHANGE:` footer or `feat!:` a major one. Other types do not release. A release bumps
`package.json`, `Cargo.toml` and `Cargo.lock`, updates `CHANGELOG.md`, tags `vX.Y.Z` and creates a draft GitHub release.
The binaries for Linux x86_64, macOS (Apple Silicon and Intel) and Windows x64 are then built from the tag and attached,
`checksums.txt` and signed build provenance are added, and the release is published.

## Harness Useful family

Harness Memory is one of a few small, local tools for coding-agent harnesses. They share one logo and one look, and each
works on its own.

- [**Harness Sidebar**](https://github.com/MSpiechowicz/harness-useful-sidebar): live token charts and session
  history for Oh My Pi in a tmux sidebar and for Claude Code in a native pane
- [**Harness Skills**](https://github.com/MSpiechowicz/harness-useful-skills): skills for Oh My Pi and Claude Code that
  take a request from a plain-language ask to reviewed, verified work, with focused audits, delivery and project memory
- [**Harness Desktop**](https://github.com/MSpiechowicz/harness-useful-desktop): your coding-agent CLI in a native
  desktop window, with chat, sessions, changes to review and its configuration; it registers this server for you
- [**Harness Dashboard**](https://github.com/MSpiechowicz/harness-useful-dashboard): token usage and cost across your
  AI coding tools, in one local app
- [**Harness Memory**](https://github.com/MSpiechowicz/harness-useful-memory) (this repository): project memory for
  your coding agents, as one local MCP server

## License

[GPL-3.0-or-later](LICENSE). The logo font is Inter, under the SIL Open Font License (`docs/brand/OFL.txt`).

### Search model notice

The embedder's model is `paraphrase-multilingual-MiniLM-L12-v2` from
[sentence-transformers](https://huggingface.co/sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2), under the
Apache License 2.0. The int8 ONNX export is by [Xenova](https://huggingface.co/Xenova/paraphrase-multilingual-MiniLM-L12-v2).
It is not part of the binary. It is downloaded as release assets when vector search is turned on.
