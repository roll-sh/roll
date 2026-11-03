#!/bin/sh
# Back up a checkout's work in progress (commits, uncommitted and untracked files; .gitignore
# respected) to origin's wip/<name>, without touching the index, HEAD or the shared branch.
# Each snapshot's parents are the previous snapshot and HEAD, so every push fast-forwards: no
# force. Run it after every meaningful edit or step; it does nothing when nothing changed.
#   crates/pg-engine/tools/wip-push.sh a148
set -eu
name=${1:?usage: wip-push.sh <name, e.g. a148>}
ref=refs/heads/wip/$name
root=$(git rev-parse --show-toplevel)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
export GIT_INDEX_FILE="$tmp/index"
git read-tree HEAD
(cd "$root" && git add -A)
tree=$(git write-tree)
unset GIT_INDEX_FILE
head=$(git rev-parse HEAD)
prev=$(git ls-remote origin "$ref" | cut -f1)
if [ -n "$prev" ]; then
  git fetch -q origin "$ref"
  if [ "$(git rev-parse "$prev^{tree}")" = "$tree" ] && git merge-base --is-ancestor "$head" "$prev"; then
    echo "unchanged: $prev"; exit 0
  fi
  set -- -p "$prev" -p "$head"
else
  set -- -p "$head"
fi
commit=$(echo "wip snapshot of $name, $(date -u +%Y-%m-%dT%H:%M:%SZ)" | git commit-tree "$tree" "$@")
git push -q origin "$commit:$ref"
echo "pushed $commit to wip/$name"
