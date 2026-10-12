#!/usr/bin/env bash
# Close open Copilot cloud agent PRs whose change landed by hand on $BRANCH.
# A commit lands PR N when its message says "from Copilot cloud agent PR #N" or "The change is Copilot cloud
# agent PR #N" (the coordinator's two conventions; use the first in new commits).
# Env: REPO (owner/name), BRANCH, SCAN_DEPTH (commits to scan, default 60), GH_TOKEN.
# DRY_RUN=1 prints what would happen and changes nothing.
set -euo pipefail

: "${REPO:?}" "${BRANCH:?}"
depth="${SCAN_DEPTH:-60}"
dry="${DRY_RUN:-0}"
seen=" "

run() {
  if [ "$dry" = 1 ]; then echo "DRY: $*"; else "$@" >/dev/null; fi
}

for sha in $(git rev-list -n "$depth" HEAD); do
  nums=$(git log -1 --format=%B "$sha" | grep -oiE '(from|change is) Copilot cloud agent PR #[0-9]+' | grep -oE '[0-9]+$' | sort -un || true)
  for n in $nums; do
    case "$seen" in *" $n "*) continue ;; esac
    seen="$seen$n "
    info=$(gh api "repos/$REPO/pulls/$n" --jq '[.state, .user.login, .head.ref, .head.repo.full_name] | @tsv' 2>/dev/null || true)
    [ -n "$info" ] || { echo "PR #$n: not readable, skipped"; continue; }
    IFS=$'\t' read -r state login head headrepo <<<"$info"
    if [ "$state" != open ]; then echo "PR #$n: already $state"; continue; fi
    if [ "$login" != Copilot ] || [ "$headrepo" != "$REPO" ]; then
      echo "PR #$n: author $login from $headrepo, not a Copilot PR of this repo, left open"; continue
    fi
    short=$(git rev-parse --short "$sha")
    echo "PR #$n: landed as $short on $BRANCH, closing"
    run gh api -X POST "repos/$REPO/issues/$n/comments" \
      -f body="Landed on $BRANCH as $short (applied by hand from this PR, so GitHub does not show it as merged). Closed automatically."
    run gh api -X PATCH "repos/$REPO/pulls/$n" -f state=closed
    case "$head" in
      copilot/*) run gh api -X DELETE "repos/$REPO/git/refs/heads/$head" || echo "PR #$n: branch $head not deleted" ;;
      *) echo "PR #$n: head $head is not a copilot/* branch, kept" ;;
    esac
  done
done
