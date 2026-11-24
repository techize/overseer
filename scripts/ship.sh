#!/bin/sh
# Merge pull requests in order, then optionally test and install main.
#
# Usage: scripts/ship.sh [--install] [--repo owner/name] <pr>...
#
# Each PR is merged at the commit its checks ran on (--match-head-commit),
# and its branch is deleted. The run stops at the first PR that is not
# open, has checks failing or still running, or conflicts with the base,
# and names the PRs left to ship. A PR already merged is skipped. After a
# merge GitHub takes a few seconds to say whether the next PR still merges;
# that answer is waited for, checks never are.
#
# --install then puts this checkout on the base branch, runs the workspace
# tests and, when they pass, runs scripts/install.sh. It needs a clean
# checkout, and switches only a linked worktree, never the main checkout.
#
# Environment:
#   OVERSEER_GH          the gh command (default gh)
#   OVERSEER_SHIP_BASE   the branch PRs merge into (default main)
#   OVERSEER_SHIP_WAIT   seconds between mergeable checks (default 5)
#   YES, NO_RESTART      passed on to install.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
gh=${OVERSEER_GH:-gh}
base=${OVERSEER_SHIP_BASE:-main}
wait=${OVERSEER_SHIP_WAIT:-5}

usage() {
    echo "usage: scripts/ship.sh [--install] [--repo owner/name] <pr>..."
}

die() {
    echo "ship: $*" >&2
    exit 1
}

install=0
repo=
while [ $# -gt 0 ]; do
    case $1 in
    --install) install=1; shift ;;
    --repo) [ $# -ge 2 ] || die "--repo needs owner/name"; repo=$2; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    -*) usage >&2; exit 2 ;;
    *) break ;;
    esac
done
[ $# -gt 0 ] || { usage >&2; exit 2; }
for pr in "$@"; do
    case $pr in
    *[!0-9]*) die "not a PR number: $pr" ;;
    esac
done
[ -n "$repo" ] || repo=$("$gh" repo view --json nameWithOwner --jq .nameWithOwner)

# One line per PR: state, branch, head commit, mergeable, and the checks
# that are not green (PENDING for one still running), comma separated.
pr_line() {
    "$gh" pr view "$1" --repo "$repo" \
        --json state,headRefName,headRefOid,mergeable,statusCheckRollup \
        --jq '[.state, .headRefName, .headRefOid, .mergeable,
               ([.statusCheckRollup[]?
                 | if (.status // "COMPLETED") != "COMPLETED" then "PENDING"
                   else (.conclusion // .state // "") end
                 | select(. != "SUCCESS" and . != "SKIPPED" and . != "NEUTRAL")]
                | unique | join(","))] | join(" ")'
}

left="$*"
for pr in "$@"; do
    left=${left#"$pr"}
    left=${left# }
    rest=${left:+" Left to ship: $left."}
    tries=0
    while :; do
        line=$(pr_line "$pr")
        # shellcheck disable=SC2086 # split the five fields
        set -- $line
        state=$1 branch=$2 oid=$3 mergeable=$4 checks=${5:-}
        [ "$mergeable" = UNKNOWN ] && [ "$state" = OPEN ] && [ $tries -lt 12 ] || break
        tries=$((tries + 1))
        sleep "$wait"
    done
    case $state in
    MERGED) echo "#$pr is already merged"; continue ;;
    OPEN) ;;
    *) die "#$pr is $(echo "$state" | tr 'A-Z' 'a-z').$rest" ;;
    esac
    case $checks in
    "") ;;
    *PENDING*) die "#$pr still has checks running; run this again when they finish.$rest" ;;
    *) die "#$pr has checks that did not pass ($checks).$rest" ;;
    esac
    case $mergeable in
    MERGEABLE) ;;
    CONFLICTING) die "#$pr conflicts with $base: merge $base into $branch, push, and run this again.$rest" ;;
    *) die "GitHub cannot say yet whether #$pr merges; run this again.$rest" ;;
    esac
    "$gh" pr merge "$pr" --repo "$repo" --merge --match-head-commit "$oid"
    # Gone already when the repository deletes merged branches itself.
    "$gh" api -X DELETE "repos/$repo/git/refs/heads/$branch" >/dev/null 2>&1 || true
    echo "merged #$pr ($branch)"
done

[ $install = 1 ] || exit 0

cd "$root"
[ -z "$(git status --porcelain)" ] || die "$root has uncommitted changes; not switching it to $base"
git fetch -q origin
if [ "$(git rev-parse HEAD)" != "$(git rev-parse "origin/$base")" ]; then
    git_dir=$(cd "$(git rev-parse --git-dir)" && pwd -P)
    common=$(cd "$(git rev-parse --git-common-dir)" && pwd -P)
    [ "$git_dir" != "$common" ] ||
        die "$root is the main checkout; check out origin/$base there, or run --install from a worktree"
    git switch -q --detach "origin/$base"
fi
echo "testing $(git log -1 --format='%h %s')"
cargo test --workspace -q >/dev/null 2>&1 || die "tests fail on $base; not installing (run cargo test --workspace to see them)"
sh "$root/scripts/install.sh"
