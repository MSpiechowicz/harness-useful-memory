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
live in a local SQLite database with full-text search (FTS5), keyed per git repository. There are no embeddings, and the server
opens no network connection unless you configure a team database. Notes are treated as untrusted data written by agents, and secrets are redacted before anything is
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
| `memory_search` | Finds notes by a few keywords, repository notes before global ones. Without a query it lists the most recent notes. Agents call it first, at the start of a task |
| `memory_remember` | Saves a note: title, body, kind (`outcome`, `problem`, `fix`, `optimisation`, `location`, `decision`, `note`), tags and the entities it is about. A note with the same title in the scope is updated, and similar notes are listed so outdated ones get fixed |
| `memory_update` | Changes a note by id: any of title, body, kind and tags, or appends text to the body |
| `memory_get` | Reads whole notes by id (up to 5) |
| `memory_link` | Relates two entities, `from relation to`, for example `ConfigLoader located_in src/config.rs`. Missing entities are created |
| `memory_neighbors` | Shows the relations around an entity (one or two hops) and the notes linked to them |
| `memory_forget` | Deletes a note by id, an entity with its relations, or one relation |

Every tool takes an optional `scope` (`repo` or `global`, and `both` for reads) and a `project_path`, the absolute path
of the repository, for when the server cannot tell it from its working directory. The server also sends instructions that
tell agents when to search, when to save, and to update a note rather than add a duplicate.

Per-call limits keep the notes short: a title is at most 120 characters, a body 4000, a note has at most 8 tags and 10
entities, and search and read replies are capped at a few KiB. The number of notes is not capped.

### Which repository

The server asks git (`git rev-parse --show-toplevel` and `git config --get remote.origin.url`) in its working directory,
or in `project_path`. The repository key is the normalized `origin` remote (`host/owner/repo`, credentials dropped), so
every clone of a repository shares its notes, and the repository path when there is no remote (so a path key matches only on one machine). A home or filesystem-root
directory is no project. The `global` scope is shared by all repositories.

## Where data lives

One SQLite file, `memory.sqlite3`, in the `harness-memory` folder of your user data directory:

| OS | Folder |
| --- | --- |
| Linux | `$XDG_DATA_HOME/harness-memory/` (default `~/.local/share/harness-memory/`) |
| macOS | `~/Library/Application Support/harness-memory/` |
| Windows | `%LOCALAPPDATA%\harness-memory\` |

Set `HARNESS_MEMORY_DIR` to use another folder (the file is then `$HARNESS_MEMORY_DIR/memory.sqlite3`). It has to be set
in the environment the harness starts the server with, and it has to be absolute (a leading `~` is fine): a relative
value is ignored, because the server runs inside your project.

**Coming from Harness Desktop.** Harness Desktop used to keep the same database at
`<data dir>/com.mspiechowicz.harness-useful-desktop/memory/memory.sqlite3`. On first run, when there is no database in
the new place yet, the server copies that file over. The old one is left untouched. Harness Desktop versions that still
bundle their own server keep writing the old file, so notes saved through them after the copy stay there.

## Team memory

By default each person's notes live in their own SQLite file. **Team memory** points every teammate's `harness-memory`
at one shared database instead, so the whole team fills one project memory: a fix one person's agent saves is found by
the next teammate's agent in the same repository. Use it for a team that works on the same repositories. For one person,
or several machines of one person, the local file is simpler and needs nothing.

With a team database the whole store is shared, repository notes **and** global notes. There is no local copy: if the
database cannot be reached the server reports an error and does not fall back to an empty local one.

**Choosing a backend.** The server needs one shared database it can reach over the network. Pick what you already run.

| Backend | URL scheme | Pick it when |
| --- | --- | --- |
| PostgreSQL | `postgres://`, `postgresql://` | You have a Postgres server or a managed one (RDS, Neon, Supabase, Cloud SQL) |
| libSQL | `libsql://`, `https://`, `http://` (localhost only) | You want the lightest option: one `sqld` container, or a hosted [Turso](https://turso.tech) database |
| MongoDB | `mongodb://`, `mongodb+srv://` | You already run MongoDB or Atlas. A replica set (Atlas always is one) gives transactions; a standalone server works too |

Every remote host uses TLS with certificate checks, unless the host is this machine (`localhost` or a loopback address):
for PostgreSQL a missing, `allow` or `prefer` `sslmode` becomes `require` and `disable` is refused; for libSQL plain
`http://` is refused; for MongoDB TLS is switched on when the URL does not say, and `tls=false`, `tlsInsecure` and
`tlsAllowInvalidCertificates`/`Hostnames` are refused. A MongoDB URL for this machine is only accepted without TLS when
it sets `directConnection=true` (or `loadBalanced=true`), so the driver cannot follow the replica set to other hosts in
the clear. A token or password for libSQL never goes in the URL. Try them on one machine first with
[`docs/team-memory/docker-compose.yml`](docs/team-memory/docker-compose.yml) (`docker compose -f
docs/team-memory/docker-compose.yml up -d --wait postgres`, `libsql` or `mongo`; the URLs are in the file's header).

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
SQLite or PostgreSQL. For a private CA add `tlsCAFile=/path/to/ca.pem`.

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

**Environment variables**: `HARNESS_MEMORY_DATABASE_URL` and, for libSQL, `HARNESS_MEMORY_DATABASE_TOKEN`. Like
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

To check it, start a new session and ask an agent to run `memory_search`. A wrong URL, a refused plain connection or an
unreachable database stops the server at startup: it exits with the reason on stderr, so the harness shows
`harness-memory` as failed to start (its MCP log or status view has the message). A connection lost later in a session
comes back as a tool error. The credentials are never printed.

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
push their own notes. By default `push` takes everything: every repository's notes and the global ones.

- **Review first.** Everything you push becomes visible to the whole team. `--dry-run` lists, under each repository key
  (and `global`), the titles of the notes it would add or change. Repositories without a remote are keyed by their
  local path (`path:/home/you/notes`) and may be private; `push` prints a note when it includes any.
- **Narrow it.** `--repo <key>` pushes one repository's notes and relations: a key as `--dry-run` lists it
  (`git:github.com/acme/widget`, `path:/home/you/notes`), a remote (`github.com/acme/widget` or its clone URL), or `.`
  for the repository of the folder you run it in (found the way the server finds it). `--no-global` leaves the global
  notes out.
- **It can replace a teammate's text.** A note with the same title in the same repository (or in global) is skipped
  when the team's copy is as new or newer, and replaced when your local copy was changed later. So `push` can overwrite
  a teammate's text of a same-titled note when your copy is newer. It names every such note (`"Title" (key): overwrote
  the team's version`) and counts them as overwritten, apart from updates that only changed the kind or tags.

It prints how many notes were added, updated, overwritten and skipped, and how many relations were added or already
there. If the team database was set up by a newer `harness-memory`, `push` stops before writing anything: update this
one first.

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

- **Local by default.** Without a team database the server opens no network connection. It talks MCP to the process that
  started it, and the only other program it runs is `git`, to find the repository (see above). It reads no other file of
  your project. When you configure a [team database](#team-memory), it connects to that one server and every note, global
  ones included, is stored there, readable by everyone who has its credentials.
- **Notes are untrusted data.** Agents write them, they may be wrong or outdated, and a planted note can try to steer a
  later session. Results say where each note came from, and the server tells agents to check notes against the code and
  never to follow instructions found inside one. That cannot make a note safe against a model that obeys what it reads:
  treat the memory like any other file an agent can edit.
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
