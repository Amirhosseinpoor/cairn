#!/usr/bin/env bash
# Schema drift check (SPEC §15.3 "Schemas", §14.7 lint job).
#
# Regenerates every committed schema from the Rust types and diffs it against
# the copy in the tree. Stale, missing and unexpected files all fail, because a
# schema that no longer matches its source is worse than no schema at all
# (REQ-ARCH-009 for the event envelope, §11.4.3 for the config schema).
#
# Usage:
#   scripts/check-schemas.sh            # report drift (exit 1 on any)
#   scripts/check-schemas.sh --write    # accept the regenerated output
#
# Exit status: 0 clean, 1 drift, 2 usage/generation error.

set -euo pipefail

cd "$(dirname "$0")/.."

WRITE=0
case "${1:-}" in
    "") ;;
    --write) WRITE=1 ;;
    -h | --help)
        sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        printf 'check-schemas.sh: unknown argument %s (try --help)\n' "$1" >&2
        exit 2
        ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# ----------------------------------------------------------------- generate

mkdir -p "$tmp/config"
cargo run -q -p cairn-config --example dump_schema -- "$tmp/config/config.schema.json" >/dev/null
cargo run -q -p cairn-core --example dump_event_schemas -- "$tmp/events" >/dev/null
cargo run -q -p cairn-tools --example dump_tool_schemas -- "$tmp/tools" >/dev/null

# ------------------------------------------------------------------- accept

if [ "$WRITE" -eq 1 ]; then
    mkdir -p schemas/events
    cp "$tmp/config/config.schema.json" schemas/config.schema.json
    cp "$tmp"/events/*.schema.json schemas/events/
    mkdir -p schemas/tools
    find schemas/tools -name '*.schema.json' -delete
    cp "$tmp"/tools/*.schema.json schemas/tools/
    printf 'regenerated schemas/config.schema.json, %d event and %d tool schema(s)\n' \
        "$(find "$tmp/events" -name '*.schema.json' | wc -l)" \
        "$(find "$tmp/tools" -name '*.schema.json' | wc -l)"
fi

# --------------------------------------------------------------------- diff

failures=0
diff_against() {
    # $1 = generated path, $2 = committed path, $3 = label
    if [ ! -e "$2" ]; then
        printf 'FAIL: %s: %s is missing from the tree\n' "$3" "$2" >&2
        failures=$((failures + 1))
        return
    fi
    if ! diff -u "$2" "$1" >"$tmp/diff.txt" 2>&1; then
        printf 'FAIL: %s: %s is out of date\n' "$3" "$2" >&2
        sed -n '1,40p' "$tmp/diff.txt" >&2
        failures=$((failures + 1))
        return
    fi
    printf 'ok   %s (%s)\n' "$3" "$2"
}

diff_against "$tmp/config/config.schema.json" "schemas/config.schema.json" "config"

generated_count=0
for generated in "$tmp"/events/*.schema.json; do
    [ -e "$generated" ] || continue
    generated_count=$((generated_count + 1))
    name=$(basename "$generated")
    diff_against "$generated" "schemas/events/$name" "event $name"
done

for committed in schemas/events/*.schema.json; do
    [ -e "$committed" ] || continue
    if [ ! -e "$tmp/events/$(basename "$committed")" ]; then
        printf 'FAIL: event %s: %s has no generator (stale file?)\n' \
            "$(basename "$committed")" "$committed" >&2
        failures=$((failures + 1))
    fi
done

tool_count=0
for generated in "$tmp"/tools/*.schema.json; do
    [ -e "$generated" ] || continue
    tool_count=$((tool_count + 1))
    name=$(basename "$generated")
    diff_against "$generated" "schemas/tools/$name" "tool $name"
done

for committed in schemas/tools/*.schema.json; do
    [ -e "$committed" ] || continue
    if [ ! -e "$tmp/tools/$(basename "$committed")" ]; then
        printf 'FAIL: tool %s: %s has no generator (stale file?)\n' \
            "$(basename "$committed")" "$committed" >&2
        failures=$((failures + 1))
    fi
done

# ------------------------------------------------------------------- result

if [ "$failures" -ne 0 ]; then
    printf '\n%d schema file(s) drifted — run `scripts/check-schemas.sh --write`\n' \
        "$failures" >&2
    exit 1
fi

printf 'schemas: clean (%d event schema(s), %d tool schema(s) + config)\n' "$generated_count" "$tool_count"
