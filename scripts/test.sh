#!/usr/bin/env bash
# tabit's test runner — the green gate with honest exit codes and a
# quiet report, so nobody hand-rolls `cargo test | grep` pipelines
# again (a piped grep can mask a failing suite; this makes that
# mistake impossible).
#
# Usage:
#   scripts/test.sh              cargo test --workspace --no-fail-fast
#   scripts/test.sh <args...>    extra cargo-test args pass through
#                                (e.g. `-p tabit-session name_filter`,
#                                or `--target-dir target-test` when the
#                                GUI holds a lock on target\debug)
#   scripts/test.sh --gate       fmt --check + clippy + test — the full
#                                green gate, same quiet reporting
#
# Report: totals; failing suites and test names; panic blocks; compile
# and clippy errors. The exit code is always the underlying cargo exit
# code (0 only when everything passed; --gate is 0 only when all three
# legs passed).

set -u -o pipefail

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

# The interesting parts of a cargo-test log, nothing else.
summarize() {
    awk '/^test result:/ { p+=$4; f+=$6; i+=$8; n++ }
         END { if (n) printf "%d passed, %d failed, %d ignored — %d suites\n", p, f, i, n }' "$1"
    awk '/^ *Running / { line=$0; sub(/^ *Running /, "", line); sub(/ *\(.*$/, "", line); suite=line }
         /^Doc-tests / { suite=$2 }
         /^test .* \.\.\. FAILED/ { printf "FAIL [%s] %s\n", suite, $2 }' "$1"
    sed -n '/^---- /,/^note: run with/p' "$1" | grep -v '^$'
    grep -A 8 '^error\[E\|^error: could not compile' "$1"
    grep '^error:' "$1" | grep -v 'could not compile'
    true
}

# Run one cargo command, report quietly, propagate its exit code.
run() {
    echo "== $* =="
    local start=$SECONDS
    "$@" >"$LOG" 2>&1
    local status=$?
    summarize "$LOG"
    printf '(%ds, exit %d)\n\n' "$((SECONDS - start))" "$status"
    return "$status"
}

# The full green gate: fmt --check, clippy (warnings shown even on
# success — they are the interesting part), then the test leg.
gate() {
    local failed=0 status

    echo "== cargo fmt --check =="
    if cargo fmt --check >"$LOG" 2>&1; then
        echo ok
    else
        failed=1
        cat "$LOG"
    fi
    echo

    echo "== cargo clippy --workspace --all-targets =="
    cargo clippy --workspace --all-targets >"$LOG" 2>&1
    status=$?
    grep -E -A 8 '^(warning|error)' "$LOG"
    echo "(exit $status)"
    if [ "$status" -ne 0 ]; then failed=1; fi
    echo

    run cargo test --workspace --no-fail-fast "$@" || failed=1
    return "$failed"
}

if [ "${1:-}" = "--gate" ]; then
    shift
    gate "$@"
else
    run cargo test --workspace --no-fail-fast "$@"
fi
