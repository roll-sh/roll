#!/bin/sh
# Back up a checkout's work in progress (commits, uncommitted and untracked files; .gitignore
# respected) to origin's wip/<name>, without touching the index, HEAD or the shared branch.
# Each snapshot's parents are the previous snapshot and HEAD, so every push fast-forwards: no
# force. Run it after every meaningful edit or step; it does nothing when nothing changed.
#   crates/pg-engine/tools/wip-push.sh a148
# It refuses (exit 3, nothing pushed) when an untracked file is build output or large: anything
# under a directory named target* or .test-build (a CARGO_TARGET_DIR outside the ignored /target,
# e.g. target-a249/: 220 MB and 434 MB of objects went to wip branches that way), anything the
# ignore rules match, or a file over WIP_MAX_MB (default 5). Move or delete it, or put its path
# in .git/info/exclude.
set -eu
name=${1:?usage: wip-push.sh <name, e.g. a148>}
ref=refs/heads/wip/$name
root=$(git rev-parse --show-toplevel)
bad=$(cd "$root" && git ls-files -z --others --exclude-standard | python3 -I -c '
import os, re, subprocess, sys
limit = float(sys.argv[1]) * 2**20
paths = [p for p in sys.stdin.buffer.read().decode("utf-8", "surrogateescape").split("\0") if p]
ignored = set(subprocess.run(["git", "check-ignore", "--no-index", "-z", "--stdin"], input="\0".join(paths).encode("utf-8", "surrogateescape"),
                             capture_output=True).stdout.decode("utf-8", "surrogateescape").split("\0")) - {""}
bad = []
for p in paths:
    try:
        size = os.lstat(p).st_size
    except OSError:
        continue
    if any(re.fullmatch(r"target.*|\.test-build", c) for c in p.split("/")[:-1]):
        bad.append((p, size, "build output (a target* or .test-build directory)"))
    elif p in ignored:
        bad.append((p, size, "matches the ignore rules"))
    elif size > limit:
        bad.append((p, size, f"over {sys.argv[1]} MB"))
for p, size, why in bad[:10]:
    print(f"  {p} ({size / 2**20:.1f} MB): {why}")
if bad:
    dirs = sorted({p.split("/")[0] for p, _, _ in bad})
    print(f"  {len(bad)} file(s), {sum(s for _, s, _ in bad) / 2**20:.0f} MB, under {", ".join(dirs[:5])}")
' "${WIP_MAX_MB:-5}")
if [ -n "$bad" ]; then
  echo "wip-push.sh: refusing to back up build output or large untracked files:" >&2
  echo "$bad" >&2
  echo "Move or delete them (a CARGO_TARGET_DIR belongs under <checkout>/.test-build/ or /target), or list them in .git/info/exclude; WIP_MAX_MB raises the size limit." >&2
  exit 3
fi
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
