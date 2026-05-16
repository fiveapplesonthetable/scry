#!/usr/bin/env bash
# Master e2e runner. Builds (or reuses) a scry index of the scry
# repo, then runs each editor's e2e suite against it. Each suite
# is self-contained; this script is mostly a tee.

set -eu

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

INDEX="${INDEX:-/mnt/agent/tmp/scry-self-idx}"
SCRY="${SCRY:-$root/target/release/scry}"
export INDEX SCRY

# Build the binary if missing.
if [ ! -x "$SCRY" ]; then
    echo "[run_all] building scry release..."
    (cd "$root" && . ./env.sh && cargo build --release > /dev/null)
fi

# Build the shared index once.
if [ ! -d "$INDEX" ]; then
    echo "[run_all] building scry index of scry repo at $INDEX..."
    "$SCRY" index "$root" -o "$INDEX" --workers 4 > /dev/null
fi

PASS=0
FAIL=0
FAILED_SUITES=""

for suite in emacs vim vscode; do
    echo
    echo "================================================================"
    echo " editor e2e: $suite"
    echo "================================================================"
    if "$here/e2e_$suite.sh"; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        FAILED_SUITES="$FAILED_SUITES $suite"
    fi
done

echo
echo "================================================================"
echo " editor e2e summary: $PASS suite(s) green / $FAIL red"
echo "================================================================"
if [ $FAIL -gt 0 ]; then
    echo "failed:$FAILED_SUITES"
    exit 1
fi
