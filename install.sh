#!/bin/sh
# Harness Memory installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/MSpiechowicz/harness-useful-memory/main/install.sh | sh
#
# Installs the harness-memory command in ~/.local/bin: a single binary that is the project memory MCP server of your
# coding agents. Register it with each harness afterwards (the steps are printed at the end, and are in the README).
# Running servers are never stopped: the new file replaces the old one atomically, and a running server keeps the old
# copy until its harness starts it again.
#
# Environment overrides:
#   HARNESS_MEMORY_VERSION         release tag to install (default: latest), e.g. v0.1.0
#   HARNESS_MEMORY_NO_MODIFY_PATH  set to 1 to leave your shell profile alone
set -eu

REPO="MSpiechowicz/harness-useful-memory"
BIN="harness-memory"
INSTALL_DIR="$HOME/.local/bin"
VERSION="${HARNESS_MEMORY_VERSION:-latest}"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
fail() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || fail "'$1' is required but not installed"; }

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1"
  else
    fail "curl or wget is required"
  fi
}

# A download URL must point into this repository's releases. The version comes from the environment, so a value that
# could walk out of that prefix (dot segments, also percent-encoded, a query) is refused. The version is also
# validated on its own (see below), which keeps slashes out.
require_release_url() {
  case "$1" in
    *..* | *%2[eE]* | *[?#]*) fail "unexpected download URL: $1" ;;
  esac

  case "$1" in
    "https://github.com/$REPO/releases/download/"* | "https://github.com/$REPO/releases/latest/download/"*) ;;
    *) fail "unexpected download URL: $1" ;;
  esac
}

# Nothing is installed without a checksum to compare against: verify the file $1 against the entry of $2 in checksums.txt.
# The name is compared literally, never as a pattern.
verify() {
  EXPECTED="$(awk -v name="$2" 'substr($0, length($1) + 3) == name { print $1; exit }' "$TMP/checksums.txt")"
  [ -n "$EXPECTED" ] || fail "checksums.txt has no entry for $2"
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL="$(sha256sum "$1" | awk '{print $1}')"
  else
    need shasum
    ACTUAL="$(shasum -a 256 "$1" | awk '{print $1}')"
  fi
  [ "$EXPECTED" = "$ACTUAL" ] || fail "checksum mismatch for $2"
}

# Puts the verified binary $1 at ~/.local/bin/harness-memory. A symlink there is the user's own arrangement, so it is
# kept and nothing is installed. The file lands beside its destination first, then replaces it with one rename: that is
# safe while a server runs from the old file (the running process keeps its inode).
install_bin() {
  if [ -L "$INSTALL_DIR/$BIN" ]; then
    fail "$INSTALL_DIR/$BIN is a symlink, so it was left alone. Nothing was installed"
  fi

  # mktemp creates a fresh file (never through an existing symlink) next to the destination. The cleanup trap removes
  # it if anything fails before the rename.
  STAGED=$(mktemp "$INSTALL_DIR/.harness-memory.XXXXXX")
  cp "$1" "$STAGED"
  chmod 755 "$STAGED"
  mv -f "$STAGED" "$INSTALL_DIR/$BIN"
  STAGED=""
  info "Installed $BIN ${TAG:-} to $INSTALL_DIR/$BIN"
}

# True when the value $1 holds a character that would change the meaning of a double-quoted shell profile line (or its
# path): a double quote, a dollar sign, a backtick, a backslash or a newline.
unsafe_for_profile() {
  case "$1" in
    *'"'* | *'$'* | *'`'* | *\\* | *'
'*) return 0 ;;
  esac
  return 1
}

case "$(uname -s)" in
  Darwin) OS="darwin" ;;
  Linux) OS="linux" ;;
  *) fail "unsupported OS: $(uname -s). On Windows use install.ps1." ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) ARCH="x86_64" ;;
  arm64 | aarch64) ARCH="aarch64" ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac

# Rosetta: prefer the native arm64 build on Apple Silicon.
if [ "$OS" = "darwin" ] && [ "$ARCH" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
  ARCH="aarch64"
fi

if [ "$OS" = "linux" ]; then
  [ "$ARCH" = "x86_64" ] || fail "the Linux binary is x86_64 only. On arm64, build it from source with cargo (see the README)"
  TRIPLE="x86_64-unknown-linux-gnu"
else
  TRIPLE="$ARCH-apple-darwin"
fi
ASSET="$BIN-$TRIPLE"

# Release tags are vMAJOR.MINOR.PATCH[-prerelease][+build]: accept the number with or without the v. The version comes
# from the environment, so anything else (a slash, a space, a newline, a query) is refused here, the same rule as
# install.ps1. require_release_url below is a second line of defence.
if [ "$VERSION" != "latest" ]; then
  case "$VERSION" in
    *'
'*) fail "unexpected version: $VERSION" ;;
  esac
  printf '%s\n' "$VERSION" | grep -Eq '^v?[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$' || fail "unexpected version: $VERSION"
  case "$VERSION" in
    v*) ;;
    *) VERSION="v$VERSION" ;;
  esac
fi

if [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi
require_release_url "$BASE/$ASSET"

TAG=""
[ "$VERSION" = "latest" ] || TAG="$VERSION"

TMP="$(mktemp -d)"
STAGED=""
cleanup() {
  rm -rf "$TMP"
  if [ -n "$STAGED" ]; then rm -f "$STAGED"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

info "Downloading $ASSET (${TAG:-latest})"
download "$BASE/$ASSET" "$TMP/$ASSET" || fail "download failed: $BASE/$ASSET"

download "$BASE/checksums.txt" "$TMP/checksums.txt" 2>/dev/null || fail "this release has no checksums.txt, so $ASSET can't be verified. Nothing was installed"
verify "$TMP/$ASSET" "$ASSET"
info "Checksum verified"

mkdir -p "$INSTALL_DIR"
install_bin "$TMP/$ASSET"
xattr -d com.apple.quarantine "$INSTALL_DIR/$BIN" 2>/dev/null || true

# A fresh binary that cannot run (a wrong platform, a blocked file) is better reported now than by the first harness.
if INSTALLED_VERSION="$("$INSTALL_DIR/$BIN" --version 2>/dev/null)"; then
  info "$INSTALLED_VERSION"
else
  warn "$INSTALL_DIR/$BIN did not run. Try it yourself: $INSTALL_DIR/$BIN --version"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    # Like install.ps1 on Windows: put the install dir on PATH for new terminals by appending to the shell's profile.
    if [ "${HARNESS_MEMORY_NO_MODIFY_PATH:-0}" = "1" ]; then
      warn "$INSTALL_DIR is not on your PATH. Add this to your shell profile:"
      printf '    export PATH="%s:%s"\n' "$INSTALL_DIR" "\$PATH"
    elif unsafe_for_profile "$INSTALL_DIR"; then
      # The profile line embeds the directory inside double quotes: with a quote, $, backtick, backslash or newline in
      # it that line would not mean what we intend, so nothing is written.
      warn "$INSTALL_DIR is not on your PATH, and its name has characters that are unsafe to write into a shell profile, so your profile was left alone."
      warn "Add this directory to your PATH yourself, in the way your shell expects:"
      printf '    %s\n' "$INSTALL_DIR"
    else
      case "$(basename "${SHELL:-sh}")" in
        zsh) PROFILE="${ZDOTDIR:-$HOME}/.zshrc"; LINE="export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
        bash)
          # macOS Terminal opens login shells, which read ~/.bash_profile rather than ~/.bashrc.
          if [ "$OS" = "darwin" ]; then PROFILE="$HOME/.bash_profile"; else PROFILE="$HOME/.bashrc"; fi
          LINE="export PATH=\"$INSTALL_DIR:\$PATH\""
          ;;
        fish) PROFILE="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/$BIN.fish"; LINE="fish_add_path \"$INSTALL_DIR\"" ;;
        *) PROFILE="$HOME/.profile"; LINE="export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
      esac
      if unsafe_for_profile "$PROFILE"; then
        warn "the path of $PROFILE has characters that are unsafe to write to, so your profile was left alone."
        warn "Add this directory to your PATH yourself, in the way your shell expects:"
        printf '    %s\n' "$INSTALL_DIR"
      else
        if [ -f "$PROFILE" ] && grep -qsF "$LINE" "$PROFILE"; then
          :
        elif mkdir -p "$(dirname "$PROFILE")" && printf '\n# Added by the Harness Memory installer\n%s\n' "$LINE" >>"$PROFILE"; then
          info "Added $INSTALL_DIR to your PATH in $PROFILE"
        else
          warn "couldn't update $PROFILE. Add $INSTALL_DIR to your PATH yourself"
        fi
        warn "open a new terminal (or run: export PATH=\"$INSTALL_DIR:\$PATH\") to use $BIN in this one"
      fi
    fi
    ;;
esac

printf '\n\033[1m%s\033[0m is installed. Register it with each coding agent you use (once per harness):\n\n' "$BIN"
printf '  Claude Code   claude mcp add --scope user %s -- %s mcp\n' "$BIN" "$BIN"
printf '  Codex         codex mcp add %s -- %s mcp\n' "$BIN" "$BIN"
printf '  OMP, PI, OpenCode: add the entry from the README, section "Register with your harness"\n'
printf '\nREADME: https://github.com/%s#register-with-your-harness\n' "$REPO"
printf 'Harness Desktop registers it for you. Re-run this installer to update.\n'
