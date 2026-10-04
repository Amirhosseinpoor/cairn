#!/usr/bin/env bash
# Document lints (SPEC §15.3 "Docs" → `scripts/lint-docs.sh (links, code
# fences)` and §16.1's mechanizable self-audit items):
#
#   1. REQ → test coverage      §14.2 maps every REQ-… to ≥ 1 T-…
#   2. error-code registry      §14.3.2b vs crates/cairn-core/src/error.rs
#   3. relative links           every `[..](path)` outside a code fence resolves
#   4. code fences              ``` fences balanced in every Markdown file
#   5. spec mirror              docs/spec.md is byte-identical to SPEC.md
#
# Usage:
#   scripts/lint-docs.sh                       # run every check
#   scripts/lint-docs.sh --check-req-coverage
#   scripts/lint-docs.sh --check-codes
#   scripts/lint-docs.sh --check-links
#   scripts/lint-docs.sh --check-fences
#   scripts/lint-docs.sh --check-spec-copy
#
# Exit status: 0 clean, 1 any check failed.

set -euo pipefail

cd "$(dirname "$0")/.."
SPEC="${SPEC:-SPEC.md}"
ERR="${ERR:-crates/cairn-core/src/error.rs}"

failures=0
note() { printf '%s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; failures=$((failures + 1)); }

# ---------------------------------------------------------------- extraction

# Everything before §16 — the audit section quotes ids it is talking *about*
# (e.g. the renumbered `REQ-ARCH-016`), which are not live requirements.
spec_body() {
    awk '/^## 16\. Final Self-Audit/{exit} {print}' "$SPEC"
}

# Every REQ-<AREA>-<NNN> defined anywhere in §0..§15.
reqs_in_spec() {
    spec_body | grep -oE 'REQ-[A-Z]+-[0-9]+' | sort -u
}

# REQ ids that the §14.2 traceability matrix maps to at least one test id.
# Matrix rows look like:  | REQ-CLI-001 | T-CLI-001, T-CLI-002 |
reqs_with_tests() {
    grep -E '^\| *REQ-[A-Z]+-[0-9]+ *\|' "$SPEC" \
        | sed -E 's/^\| *(REQ-[A-Z]+-[0-9]+) *\|.*/\1/' \
        | sort -u
}

# T-ids the matrix assigns to a requirement.
tids_in_matrix() {
    grep -E '^\| *REQ-[A-Z]+-[0-9]+ *\|' "$SPEC" \
        | grep -oE 'T-[A-Z0-9]+-[0-9]+' | sort -u
}

# T-ids mentioned anywhere outside the matrix — a referenced id must have a
# home (a §14.3 row, a range like `T-TUI-010..019`, or the prose that fixes
# what the test does).
tids_elsewhere() {
    grep -vE '^\| *REQ-[A-Z]+-[0-9]+ *\|' "$SPEC" \
        | grep -oE 'T-[A-Z0-9]+-[0-9]+' | sort -u
}

# E-/W- codes documented in the spec body (§0..§15 — §16 quotes codes it is
# auditing). `E-XXX-YYY` in D-15 shows the *shape* of a code and is
# deliberately not a code; so is a bare area stub such as `E-CTX`, which the
# requirement of at least one `-<NAME>` segment already excludes.
codes_in_spec() {
    spec_body | grep -oE '`[EW]-[A-Z0-9]+(-[A-Z0-9_]+)+`' \
        | tr -d '`' | grep -v '^E-XXX-YYY$' | sort -u
}

codes_registered() {
    grep -oE '"[EW]-[A-Z0-9]+-[A-Z0-9_]+"' "$ERR" | tr -d '"' | sort -u
}

# ------------------------------------------------------------------- checks

check_req_coverage() {
    local missing undef
    missing=$(comm -23 <(reqs_in_spec) <(reqs_with_tests))
    if [[ -n "$missing" ]]; then
        fail "requirements without a row in the §14.2 traceability matrix:"
        printf '  %s\n' $missing >&2
    else
        note "ok  req coverage: $(reqs_in_spec | wc -l) requirements, all mapped to tests"
    fi

    undef=$(comm -23 <(tids_in_matrix) <(tids_elsewhere))
    if [[ -n "$undef" ]]; then
        fail "test ids referenced by §14.2 but defined nowhere else:"
        printf '  %s\n' $undef >&2
    else
        note "ok  test ids: $(tids_in_matrix | wc -l) referenced, all defined"
    fi
}

check_codes() {
    local missing extra
    missing=$(comm -23 <(codes_in_spec) <(codes_registered))
    extra=$(comm -13 <(codes_in_spec) <(codes_registered))
    if [[ -n "$missing" ]]; then
        fail "codes documented in $SPEC but missing from $ERR:"
        printf '  %s\n' $missing >&2
    fi
    if [[ -n "$extra" ]]; then
        fail "codes registered in $ERR but absent from $SPEC:"
        printf '  %s\n' $extra >&2
    fi
    if [[ -z "$missing" && -z "$extra" ]]; then
        note "ok  codes: $(codes_in_spec | wc -l) documented, registry is 1:1"
    fi
}

# ------------------------------------------------------------------- markdown

# Every tracked Markdown file (code fences and links are checked per file, so
# the list is the single place that decides what "the docs" are).
#
# `docs/spec.md` is deliberately absent: it is a byte-identical mirror of
# SPEC.md (§15.6), so its fences are already checked under `SPEC.md`, and its
# relative links are written for the root copy — resolving them from inside
# `docs/` would report a false break.
md_files() {
    local f
    for f in SPEC.md README.md CHANGELOG.md PROGRESS.md docs/*.md docs/adr/*.md; do
        [ "$f" = "docs/spec.md" ] && continue
        [ -f "$f" ] && printf '%s\n' "$f"
    done
    return 0
}

# `file<TAB>target` for every Markdown link in `files`, skipping anything
# inside a fenced code block — examples in the spec must not be linted as if
# they were real links.
link_occurrences() {
    # shellcheck disable=SC2046
    for f in $(md_files); do
        awk -v F="$f" '
            /^[[:space:]]*```/ { fenced = !fenced; next }
            !fenced {
                line = $0
                while (match(line, /\]\([^)]*\)/)) {
                    t = substr(line, RSTART + 2, RLENGTH - 3)
                    split(t, parts, " ")
                    print F "\t" parts[1]
                    line = substr(line, RSTART + RLENGTH)
                }
            }' "$f"
    done
}

check_links() {
    local file target path total=0 before=$failures
    while IFS=$'\t' read -r file target; do
        case "$target" in
            '' | '#'* | http://* | https://* | mailto:* | data:*) continue ;;
        esac
        target="${target%%#*}"
        [ -z "$target" ] && continue
        case "$target" in
            /*) path="$target" ;;
            *) path="$(dirname "$file")/$target" ;;
        esac
        total=$((total + 1))
        if [ ! -e "$path" ]; then
            fail "broken relative link in $file: $target"
        fi
    done < <(link_occurrences)
    if [[ $failures -eq $before ]]; then
        note "ok  links: $total relative links all resolve"
    fi
}

check_fences() {
    local f n before=$failures
    while IFS= read -r f; do
        n=$(grep -c '^[[:space:]]*```' "$f" || true)
        if ((n % 2 != 0)); then
            fail "unbalanced code fences in $f ($n fence lines)"
        fi
    done < <(md_files)
    if [[ $failures -eq $before ]]; then
        note "ok  fences: $(md_files | wc -l) markdown files, all balanced"
    fi
}

# §15.6: `docs/spec.md` *is* this document. Both paths stay byte-identical so a
# reader who follows either one gets the normative text; this check is what
# makes that safe to claim.
check_spec_copy() {
    if [ ! -f docs/spec.md ]; then
        fail "docs/spec.md is missing (§15.6)"
    elif ! cmp -s "$SPEC" docs/spec.md; then
        fail "docs/spec.md differs from $SPEC — copy it over (cp $SPEC docs/spec.md)"
    else
        note "ok  spec mirror: docs/spec.md is identical to $SPEC"
    fi
}

case "${1:-all}" in
    --check-req-coverage) check_req_coverage ;;
    --check-codes) check_codes ;;
    --check-links) check_links ;;
    --check-fences) check_fences ;;
    --check-spec-copy) check_spec_copy ;;
    all)
        check_req_coverage
        check_codes
        check_links
        check_fences
        check_spec_copy
        ;;
    *)
        printf 'usage: %s [--check-req-coverage|--check-codes|--check-links|--check-fences|--check-spec-copy]\n' "$0" >&2
        exit 2
        ;;
esac

if [[ $failures -gt 0 ]]; then
    printf '\n%d check(s) failed\n' "$failures" >&2
    exit 1
fi
printf '\nall checks passed\n'
