#!/usr/bin/env bash
# Wait for every exact platform version pinned by the staged npm launcher.
# One ten-minute deadline covers all packages, registry requests and sleeps.
# Run on the Linux publish runner after stage-npm-packages.sh.
set -euo pipefail

WAIT_SECONDS=${NPM_VISIBILITY_TIMEOUT_SECONDS:-600}
if [[ ! "$WAIT_SECONDS" =~ ^[1-9][0-9]*$ ]]; then
    echo "error: NPM_VISIBILITY_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 1
fi

PACKAGES=$(jq -er '.optionalDependencies | to_entries[] | [.key, .value] | @tsv' "${1:-npm/package.json}")
DEADLINE=$((SECONDS + WAIT_SECONDS))

while IFS=$'\t' read -r NAME VERSION; do
    while true; do
        REMAINING=$((DEADLINE - SECONDS))
        if (( REMAINING <= 0 )); then
            echo "::error::$NAME@$VERSION not visible within ${WAIT_SECONDS}s — not publishing the launcher against a missing platform package. Re-run failed jobs to retry."
            exit 1
        fi

        # Revalidate cached metadata on every poll. Bound npm's network work as
        # well as the whole command so a stalled request cannot defeat the deadline.
        if VISIBLE=$(timeout "$REMAINING" npm view "$NAME@$VERSION" version \
            --prefer-online --fetch-retries=0 --fetch-timeout=10000 2>/dev/null) \
            && [[ "$VISIBLE" == "$VERSION" ]]; then
            echo "$NAME@$VERSION visible."
            break
        fi

        REMAINING=$((DEADLINE - SECONDS))
        if (( REMAINING > 0 )); then
            echo "waiting for $NAME@$VERSION to appear (${REMAINING}s left)…"
            DELAY=5
            (( REMAINING < DELAY )) && DELAY=$REMAINING
            sleep "$DELAY"
        fi
    done
done <<< "$PACKAGES"
