#!/bin/sh
# Fetches the public benchmark repositories of bench/repos.toml at their pinned commits into
# target/bench-repos/<name>. Idempotent: a repository already at its commit is left alone.
# Usage: bench/fetch.sh [name ...]
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(dirname -- "$here")
dest="$root/target/bench-repos"
only="$*"

mkdir -p "$dest"

# Prints "name url commit" per [section] of repos.toml.
list() {
    awk '
        /^\[/ { if (name != "") print name, url, commit; name = $0; gsub(/[][]/, "", name); url = ""; commit = ""; next }
        /^url *=/ { v = $0; sub(/^[^"]*"/, "", v); sub(/".*$/, "", v); url = v }
        /^commit *=/ { v = $0; sub(/^[^"]*"/, "", v); sub(/".*$/, "", v); commit = v }
        END { if (name != "") print name, url, commit }
    ' "$here/repos.toml"
}

list | while read -r name url commit; do
    if [ -n "$only" ]; then
        case " $only " in
            *" $name "*) ;;
            *) continue ;;
        esac
    fi

    case "$name" in
        *[!A-Za-z0-9._-]* | "") echo "bench/fetch.sh: bad repository name '$name'" >&2; exit 1 ;;
    esac
    case "$commit" in
        *[!0-9a-f]* | "") echo "bench/fetch.sh: bad commit for $name" >&2; exit 1 ;;
    esac

    dir="$dest/$name"
    short=$(printf '%s' "$commit" | cut -c1-8)

    if [ -d "$dir/.git" ] && [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)" = "$commit" ]; then
        echo "$name: already at $short"
        continue
    fi

    echo "$name: fetching $short"
    mkdir -p -- "$dir"
    [ -d "$dir/.git" ] || git -C "$dir" init --quiet
    git -C "$dir" remote remove origin 2>/dev/null || true
    git -C "$dir" remote add origin "$url"
    git -C "$dir" fetch --quiet --depth 1 origin "$commit"
    git -C "$dir" -c advice.detachedHead=false checkout --quiet --force FETCH_HEAD
    echo "$name: ready"
done
