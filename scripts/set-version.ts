// Release helper: set the Rust crate version to the semantic-release version.
//
//   bun scripts/set-version.ts <semver> [--root <dir>]
//
// package.json is bumped by @semantic-release/npm; this script keeps Cargo.toml
// ([package] version) and the crate's entry in Cargo.lock in step.
// Edits are line-precise so every other byte of both files is preserved, and cargo is never run.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

/** Crates whose Cargo.lock entry carries the release version. */
export const WORKSPACE_PACKAGES = ["harness_memory"] as const;

// Official semver.org regular expression (strict: no leading "v", no leading zeros).
const SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$/;

export function isSemver(value: string): boolean {
  return SEMVER.test(value);
}

export interface Change {
  file: string;
  target: string;
  from: string;
  to: string;
}

const VERSION_LINE = /^(version\s*=\s*)"([^"]*)"(.*)$/;

/** Replace the first `version = "..."` line inside a TOML section; returns the new text and old version. */
function replaceVersionInBlock(
  lines: string[],
  start: number,
  end: number,
  version: string,
  what: string,
): string {
  for (let index = start; index < end; index += 1) {
    const match = VERSION_LINE.exec(lines[index] ?? "");
    if (match) {
      const previous = match[2] ?? "";
      lines[index] = `${match[1]}"${version}"${match[3]}`;
      return previous;
    }
  }
  throw new Error(`no version line found for ${what}`);
}

// A table header such as `[package]` or `[[package]]`. Lines of a multi-line array (`["a", "b"],`) are no headers.
const TABLE_HEADER = /^\[\[?[A-Za-z0-9_."' -]+\]\]?\s*(#.*)?$/;

/** Index range [start, end) of the lines following a header line, up to the next table header. */
function blockEnd(lines: string[], from: number): number {
  let index = from;
  while (index < lines.length && !TABLE_HEADER.test((lines[index] ?? "").trim())) index += 1;
  return index;
}

/** A manifest's lines and the line ending it uses: a Windows checkout has CRLF, and it is kept. */
function splitLines(text: string): { lines: string[]; eol: string } {
  const eol = text.includes("\r\n") ? "\r\n" : "\n";
  return { lines: text.split(eol), eol };
}

export function updateCargoToml(text: string, version: string): { text: string; change: Change } {
  const { lines, eol } = splitLines(text);
  const header = lines.findIndex((line) => line.trim() === "[package]");
  if (header === -1) throw new Error("Cargo.toml has no [package] section");
  const previous = replaceVersionInBlock(
    lines,
    header + 1,
    blockEnd(lines, header + 1),
    version,
    "[package]",
  );
  return {
    text: lines.join(eol),
    change: { file: "Cargo.toml", target: "[package]", from: previous, to: version },
  };
}

export function updateCargoLock(text: string, version: string): { text: string; changes: Change[] } {
  const { lines, eol } = splitLines(text);
  const changes: Change[] = [];
  for (const name of WORKSPACE_PACKAGES) {
    const nameIndexes = lines.flatMap((line, index) =>
      line === `name = "${name}"` && lines[index - 1] === "[[package]]" ? [index] : [],
    );
    if (nameIndexes.length !== 1) {
      throw new Error(`Cargo.lock must contain exactly one [[package]] named ${name}, found ${nameIndexes.length}`);
    }
    const at = nameIndexes[0] as number;
    const previous = replaceVersionInBlock(lines, at + 1, blockEnd(lines, at + 1), version, name);
    changes.push({ file: "Cargo.lock", target: name, from: previous, to: version });
  }
  return { text: lines.join(eol), changes };
}

/** Apply the version to <root>/Cargo.toml and <root>/Cargo.lock. Writes only files whose content changes. */
export function setVersion(root: string, version: string): Change[] {
  if (!isSemver(version)) throw new Error(`"${version}" is not a strict semver version (expected MAJOR.MINOR.PATCH)`);
  const tomlPath = join(root, "Cargo.toml");
  const lockPath = join(root, "Cargo.lock");
  for (const path of [tomlPath, lockPath]) {
    if (!existsSync(path)) throw new Error(`missing ${path}`);
  }

  const toml = updateCargoToml(readFileSync(tomlPath, "utf8"), version);
  const lock = updateCargoLock(readFileSync(lockPath, "utf8"), version);
  // Compute everything before writing anything, so a failure leaves both files untouched.
  if (toml.text !== readFileSync(tomlPath, "utf8")) writeFileSync(tomlPath, toml.text);
  if (lock.text !== readFileSync(lockPath, "utf8")) writeFileSync(lockPath, lock.text);
  return [toml.change, ...lock.changes];
}

function main(argv: string[]): number {
  const args = argv.slice(2);
  let root = resolve(import.meta.dir, "..");
  const rootFlag = args.indexOf("--root");
  if (rootFlag !== -1) {
    root = resolve(args[rootFlag + 1] ?? "");
    args.splice(rootFlag, 2);
  }
  const [version, ...extra] = args;
  if (version === undefined || extra.length > 0) {
    console.error("usage: bun scripts/set-version.ts <semver> [--root <dir>]");
    return 2;
  }
  try {
    for (const change of setVersion(root, version)) {
      const verb = change.from === change.to ? "unchanged" : `${change.from} -> ${change.to}`;
      console.log(`${change.file} ${change.target}: ${verb}`);
    }
    return 0;
  } catch (error) {
    console.error(`set-version: ${error instanceof Error ? error.message : String(error)}`);
    return 1;
  }
}

if (import.meta.main) process.exit(main(process.argv));
