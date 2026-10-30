#!/bin/sh
# The agent A/B of the code index: the same questions asked of `claude -p` (Sonnet) without (arm a) and with
# (arm b) the code_* tools of harness-memory, recording tokens, turns, cost and whether the answer names the
# expected files. It costs money: nothing runs without you starting it, and --dry-run only prints the commands.
#
# Usage: bench/ab/run.sh [--arms a,b,c] [--tasks id,id] [--repeat N] [--model M] [--binary PATH]
#                        [--timeout SECONDS] [--dry-run]
#
#   arm a   HARNESS_MEMORY_CODE_INDEX=0: Read, Grep, Glob only
#   arm b   HARNESS_MEMORY_CODE_INDEX=1: plus mcp__harness-memory__code_map/symbol/impact/path
#
# Tasks come from bench/ab/tasks.toml (public repositories, fetch them with bench/fetch.sh) and, when present,
# bench/local/ab-tasks.toml (private ones named in bench/local.toml). Build the binary first:
#   cargo build --release
# The index lives in target/bench-ab/data (HARNESS_MEMORY_DIR), so the first arm-b run of a repository pays for
# the index build and later runs are warm. Results: target/bench-ab/<timestamp>.jsonl.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/../.." && pwd)

exec python3 -I - "$root" "$@" <<'PY'
import argparse
import json
import re
import os
import shlex
import subprocess
import sys
import time
import tomllib
from pathlib import Path

root = Path(sys.argv[1])

parser = argparse.ArgumentParser(prog="bench/ab/run.sh")
parser.add_argument("--arms", default="a,b", help="comma separated: a (no code tools), b (code tools), "
                    "c (code tools + the Harness Desktop family hook text 'Use the code graph first')")
parser.add_argument("--tasks", default="", help="comma separated task ids (default: all)")
parser.add_argument("--repeat", type=int, default=1, help="runs per task and arm")
parser.add_argument("--model", default="sonnet")
parser.add_argument("--binary", default=str(root / "target/release/harness-memory"))
parser.add_argument("--timeout", type=int, default=900, help="seconds per run")
parser.add_argument("--dry-run", action="store_true", help="print the commands, run nothing")
args = parser.parse_args(sys.argv[2:])

arms = [a.strip() for a in args.arms.split(",") if a.strip()]
if not arms or any(a not in ("a", "b", "c") for a in arms):
    sys.exit("--arms takes a, b and/or c")

# What the Harness Desktop family hook "Use the code graph first" (HookSeed::CodeGraphFirst) adds to every prompt.
FAMILY_HOOK_TEXT = (
    "Before your first tool call, decide which Harness Memory code tool answers the question and call it first: "
    "code_map for an overview of the repository or an area, code_symbol for where a name is defined and used, "
    "code_impact for what a change affects, code_path for how A reaches B. Pass project_path, the repository root. "
    "grep, find and file reads come after, for literal text and for details the graph does not show. A small "
    "repository is no reason to skip them: one code_map call costs less than listing and reading files. A question "
    "about a flow (how a request, an event or data gets from one part to another) is answered by code_path from the "
    "entry point to the part it ends in, after code_map named both: do not reconstruct the flow by reading files one "
    "by one. If the code tools are listed as deferred tools, load them with ToolSearch first. If they are not "
    "available in this chat, say so once and go on with the usual tools."
)
if args.repeat < 1:
    sys.exit("--repeat must be at least 1")

work = root / "target/bench-ab"
data_dir = work / "data"
CODE_TOOLS = ",".join(f"mcp__harness-memory__code_{t}" for t in ("map", "symbol", "impact", "path"))
BASE_TOOLS = "Read,Grep,Glob"


def load(path):
    with open(path, "rb") as handle:
        return tomllib.load(handle)


# ---- tasks and repositories ------------------------------------------------------------------------------

tasks = []
for file in (root / "bench/ab/tasks.toml", root / "bench/local/ab-tasks.toml"):
    if file.is_file():
        tasks += load(file).get("task", [])

wanted = {t for t in args.tasks.split(",") if t}
if wanted:
    unknown = wanted - {t["id"] for t in tasks}
    if unknown:
        sys.exit("unknown task id(s): " + ", ".join(sorted(unknown)))
    tasks = [t for t in tasks if t["id"] in wanted]

local = load(root / "bench/local.toml") if (root / "bench/local.toml").is_file() else {}
public = load(root / "bench/repos.toml")


def repo_dir(name):
    if name in public:
        return root / "target/bench-repos" / name
    if name in local:
        return Path(local[name])
    return None


# ---- files the answer must name ---------------------------------------------------------------------------

_tracked = {}


def tracked_files(directory):
    """Paths git tracks in `directory` (empty when it is not a git checkout)."""
    if directory not in _tracked:
        done = subprocess.run(["git", "-C", str(directory), "ls-files"], capture_output=True, text=True)
        _tracked[directory] = done.stdout.splitlines() if done.returncode == 0 else []
    return _tracked[directory]


def shortest_unique_suffix(rel, tracked):
    """The fewest trailing path components that name `rel` alone among the tracked files."""
    parts = rel.split("/")
    for k in range(1, len(parts) + 1):
        suffix = "/".join(parts[-k:])
        if sum(1 for f in tracked if f == suffix or f.endswith("/" + suffix)) <= 1:
            return suffix
    return rel


CODE_FILE = re.compile(r"\.(rs|go|py|ts|tsx|js|jsx|mjs|cjs|svelte|vue|astro)$")


def stem(path):
    return path.rsplit("/", 1)[-1].split(".", 1)[0]


def missing_files(answer, expect, directory):
    """The expected files the answer does not name: by the shortest suffix that is unique in the repository, or by the
    bare file name (`TagsView` for `…/TagsView.tsx`, how components are usually named) when no other file shares it."""
    tracked = tracked_files(directory)
    gone = []
    for rel in expect:
        needle = shortest_unique_suffix(rel, tracked) if tracked else "/".join(rel.split("/")[-2:])
        name = stem(rel)
        # test files (`TagsView.test.tsx`, `__tests__/…`) share the name but are not what an answer means by it
        # (and stylesheets or assets with the same name are not code an answer refers to)
        rivals = [
            f for f in tracked
            if stem(f) == name and CODE_FILE.search(f) and not re.search(r"(^|/)(__tests__|tests?)/|\.(test|spec)\.", f)
        ]
        unique_name = bool(tracked) and len(rivals) == 1
        by_name = unique_name and re.search(rf"(?<![\w/.-]){re.escape(name)}(?![\w-])", answer) is not None
        if needle not in answer and rel not in answer and not by_name and not names_module(answer, rel, tracked):
            gone.append(rel)
    return gone


# The file that is a folder's module (`src/mcp/mod.rs` is the `mcp` module, likewise `index.ts`, `__init__.py`).
MODULE_ENTRY = re.compile(r"(^|/)(mod\.rs|index\.(ts|tsx|js|jsx|mjs)|__init__\.py)$")


def names_module(answer, rel, tracked):
    """Whether the answer names the module `rel` is the entry of, by its folder: the folder path (`src/mcp/`) or, when
    no other folder has that name, the bare folder name in code (`mcp`, `mcp/`, `mcp::`)."""
    if not MODULE_ENTRY.search(rel) or "/" not in rel:
        return False
    folder = rel.rsplit("/", 1)[0]
    if folder + "/" in answer or re.search(rf"(?<![\w/.-]){re.escape(folder)}(?![\w.-])", answer):
        return True
    last = folder.rsplit("/", 1)[-1]
    folders = {f.rsplit("/", 1)[0] for f in tracked if "/" in f}
    if sum(1 for d in folders if d.rsplit("/", 1)[-1] == last) != 1:
        return False
    return re.search(rf"`{re.escape(last)}(/|::)?`|(?<![\w/.-]){re.escape(last)}(/|::)", answer) is not None


# ---- one run ------------------------------------------------------------------------------------------------

def mcp_config(arm):
    config = {
        "mcpServers": {
            "harness-memory": {
                "command": str(Path(args.binary).resolve()),
                "args": ["mcp"],
                "env": {
                    "HARNESS_MEMORY_DIR": str(data_dir.resolve()),
                    "HARNESS_MEMORY_CODE_INDEX": "0" if arm == "a" else "1",
                },
            }
        }
    }
    path = work / f"mcp-{arm}.json"
    return path, config


def command(task, arm, config_path):
    allowed = BASE_TOOLS + ("" if arm == "a" else "," + CODE_TOOLS)
    hook = ["--append-system-prompt", FAMILY_HOOK_TEXT] if arm == "c" else []
    return [
        "claude", "-p", task["question"], *hook,
        "--model", args.model,
        "--output-format", "stream-json", "--verbose",
        "--no-session-persistence",
        "--setting-sources", "project",
        "--strict-mcp-config",
        "--mcp-config", str(config_path),
        "--tools", BASE_TOOLS,
        "--allowedTools", allowed,
        "--disable-slash-commands",
    ]


def summarize(task, arm, repeat, completed, seconds, directory):
    record = {
        "task": task["id"], "arm": arm, "repeat": repeat, "repo": task["repo"],
        "model": args.model, "wall_ms": int(seconds * 1000),
        "exit": completed.returncode,
    }
    # stream-json: one event per line; the last `result` event carries usage and the answer, the `assistant`
    # events carry the tool calls (which tools the agent chose is what the code index is meant to change)
    # `sequence` is the order of the calls; `code` keeps each code tool call with its reply, to see what the agent
    # did after a reply (`user` events carry the tool results)
    reply, tools, sequence, code, pending = None, {}, [], [], {}
    for line in completed.stdout.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("type") == "result":
            reply = event
        elif event.get("type") == "assistant":
            for block in (event.get("message") or {}).get("content") or []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    name = str(block.get("name", "")).replace("mcp__harness-memory__", "")
                    tools[name] = tools.get(name, 0) + 1
                    sequence.append(name)
                    if name.startswith("code_"):
                        call = {"tool": name, "at": len(sequence) - 1, "input": block.get("input"), "reply": None}
                        code.append(call)
                        pending[block.get("id")] = call
        elif event.get("type") == "user":
            for block in (event.get("message") or {}).get("content") or []:
                if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("tool_use_id") in pending:
                    content = block.get("content")
                    if isinstance(content, list):
                        content = "\n".join(str(part.get("text", "")) for part in content if isinstance(part, dict))
                    pending.pop(block.get("tool_use_id"))["reply"] = str(content or "")[:6000]
    if reply is None:
        record.update(error="no result event on stdout", stderr=completed.stderr[-500:])
        return record
    usage = reply.get("usage") or {}
    answer = reply.get("result") or ""
    expect = task["expect_files"]
    gone = missing_files(answer, expect, directory)
    # Open-ended questions ("the 8 most central modules") name `min_expect` of the files, not all of them.
    enough = len(expect) - len(gone) >= task.get("min_expect", len(expect))
    record.update(
        input_tokens=(usage.get("input_tokens") or 0)
        + (usage.get("cache_read_input_tokens") or 0)
        + (usage.get("cache_creation_input_tokens") or 0),
        output_tokens=usage.get("output_tokens") or 0,
        tools=tools,
        sequence=sequence,
        code=code,
        code_calls=sum(count for name, count in tools.items() if name.startswith("code_")),
        grep_calls=tools.get("Grep", 0),
        turns=reply.get("num_turns") or 0,
        cost_usd=reply.get("total_cost_usd") or 0.0,
        duration_ms=reply.get("duration_ms") or 0,
        is_error=bool(reply.get("is_error")),
        correct=enough and not reply.get("is_error"),
        missing=gone,
        answer=answer,
    )
    return record


def table(records):
    groups = {}
    for r in records:
        groups.setdefault((r["task"], r["arm"]), []).append(r)
    rows = []
    for (task, arm), rs in sorted(groups.items()):
        ok = [r for r in rs if "input_tokens" in r]
        n = len(ok) or 1
        rows.append((
            task, arm, f"{sum(r['correct'] for r in ok)}/{len(rs)}",
            round(sum(r["input_tokens"] for r in ok) / n), round(sum(r["output_tokens"] for r in ok) / n),
            round(sum(r["turns"] for r in ok) / n, 1), round(sum(r["cost_usd"] for r in ok) / n, 4),
            round(sum(r["duration_ms"] for r in ok) / n / 1000, 1),
            f"{sum(1 for r in ok if r.get('code_calls'))}/{len(ok)}",
            round(sum(r.get("grep_calls", 0) for r in ok) / n, 1),
        ))
    head = ("task", "arm", "correct", "input", "output", "turns", "cost$", "sec", "used code", "greps")
    widths = [max(len(str(x)) for x in col) for col in zip(head, *rows)]
    fmt = "  ".join("{:<%d}" % w for w in widths)
    print(fmt.format(*head))
    print(fmt.format(*("-" * w for w in widths)))
    for row in rows:
        print(fmt.format(*map(str, row)))
    print()
    for arm in sorted({r["arm"] for r in records}):
        ok = [r for r in records if r["arm"] == arm and "input_tokens" in r]
        print(
            f"arm {arm}: {sum(r['correct'] for r in ok)}/{len(ok)} correct, "
            f"input {sum(r['input_tokens'] for r in ok)}, output {sum(r['output_tokens'] for r in ok)}, "
            f"turns {sum(r['turns'] for r in ok)}, cost ${sum(r['cost_usd'] for r in ok):.4f}, "
            f"runs using a code tool {sum(1 for r in ok if r.get('code_calls'))}/{len(ok)}, "
            f"greps {sum(r.get('grep_calls', 0) for r in ok)}"
        )


# ---- main -----------------------------------------------------------------------------------------------------

if not tasks:
    sys.exit("no tasks selected")

if not args.dry_run:
    if not Path(args.binary).is_file():
        sys.exit(f"{args.binary} is missing: run `cargo build --release` first")
    if subprocess.run(["claude", "--version"], capture_output=True).returncode != 0:
        sys.exit("the claude CLI is not installed")
    data_dir.mkdir(parents=True, exist_ok=True)
    work.mkdir(parents=True, exist_ok=True)

stamp = time.strftime("%Y%m%d-%H%M%S")
out_path = work / f"{stamp}.jsonl"
records = []

for task in tasks:
    directory = repo_dir(task["repo"])
    if directory is None or not directory.is_dir():
        print(f"SKIPPED {task['id']}: repository {task['repo']!r} is not available "
              f"(bench/fetch.sh for public ones, bench/local.toml for private ones)", file=sys.stderr)
        continue
    for repeat in range(1, args.repeat + 1):
        for arm in arms:
            config_path, config = mcp_config(arm)
            cmd = command(task, arm, config_path)
            label = f"{task['id']} arm {arm} run {repeat}/{args.repeat}"
            if args.dry_run:
                print(f"# {label}")
                print(f"# {config_path.relative_to(root)} = {json.dumps(config)}")
                print(f"(cd {shlex.quote(str(directory))} && {shlex.join(cmd)} < /dev/null)")
                print()
                continue
            config_path.write_text(json.dumps(config))
            print(f"running {label} ...", file=sys.stderr, flush=True)
            started = time.monotonic()
            try:
                completed = subprocess.run(
                    cmd, cwd=directory, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                    timeout=args.timeout,
                )
            except subprocess.TimeoutExpired:
                record = {"task": task["id"], "arm": arm, "repeat": repeat, "repo": task["repo"],
                          "error": f"timed out after {args.timeout}s"}
                records.append(record)
                with open(out_path, "a") as handle:
                    handle.write(json.dumps(record) + "\n")
                continue
            record = summarize(task, arm, repeat, completed, time.monotonic() - started, directory)
            records.append(record)
            with open(out_path, "a") as handle:
                handle.write(json.dumps(record) + "\n")

if args.dry_run:
    print(f"# dry run: {len(tasks)} task(s), arms {','.join(arms)}, repeat {args.repeat}; nothing was executed")
else:
    print()
    table(records)
    print(f"\nrecords: {out_path}")
PY
