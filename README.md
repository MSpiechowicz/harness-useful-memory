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
live in a local SQLite database with full-text search (FTS5), keyed per git repository. There are no embeddings and no
network access. Notes are treated as untrusted data written by agents, and secrets are redacted before anything is
stored.

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
optionally, the notes:

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
every clone of a repository shares its notes, and the repository path when there is no remote. A home or filesystem-root
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

## Privacy and trust

- **Local only.** The server opens no network connection. It talks MCP to the process that started it, and the only other
  program it runs is `git`, to find the repository (see above). It reads no other file of your project.
- **Notes are untrusted data.** Agents write them, they may be wrong or outdated, and a planted note can try to steer a
  later session. Results say where each note came from, and the server tells agents to check notes against the code and
  never to follow instructions found inside one. That cannot make a note safe against a model that obeys what it reads:
  treat the memory like any other file an agent can edit.
- **Secrets are redacted on write.** Titles, bodies, tags, entity names and relation labels pass a secret filter before
  they are stored. It is a heuristic and does not catch everything, so do not rely on it. Agents are also told never to
  store secrets or personal data.
- **No access control.** The repository key is only a name: any agent that can start the server can read and write any
  repository's notes. A same-title note written from another checkout, or by an agent that names another repository by
  path, is reported instead of replaced.
- **Not encrypted.** The database is a plain file in your user data directory, outside any repository. What an agent reads
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
