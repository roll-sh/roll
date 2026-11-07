#!/bin/sh
# Push HEAD to the shared branch only if it fast-forwards origin and does not take back what other
# commits added. A commit made from a stale working tree on top of a newer origin ref reverts other
# agents' work while still fast-forwarding (d0867471 did that to six commits); this catches it by
# checking that the lines each of origin's last 40 commits added (outside HEAD's own new commits)
# are still in HEAD's version of their files.
#   crates/pg-engine/tools/push.sh            # push HEAD
#   PUSH_DRY=1 crates/pg-engine/tools/push.sh # check only
set -eu
branch=docs/postgres-architecture-strategy
git fetch -q origin "$branch"
remote=$(git rev-parse "origin/$branch")
git merge-base --is-ancestor "$remote" HEAD || { echo "push.sh: HEAD does not contain origin/$branch; rebase first" >&2; exit 1; }
python3 -I - "$remote" <<'PY'
import subprocess, sys
remote = sys.argv[1]
def git(*a): return subprocess.run(['git', *a], capture_output=True, text=True).stdout
def show(rev, f):
    r = subprocess.run(['git', 'show', f'{rev}:{f}'], capture_output=True, text=True)
    return set(l.strip() for l in r.stdout.splitlines()) if r.returncode == 0 else set()
bad = []
for c in git('rev-list', '--max-count=40', '--no-merges', remote).split():
    files = [f for f in git('diff-tree', '--no-commit-id', '--name-only', '-r', c).split() if f]
    for f in files:
        added = [l[1:].strip() for l in git('diff', f'{c}^', c, '--', f).splitlines()
                 if l.startswith('+') and not l.startswith('+++') and len(l[1:].strip()) > 8]
        if len(added) < 3: continue
        at_remote, at_head = show(remote, f), show('HEAD', f)
        lost = [l for l in added if l in at_remote and l not in at_head]
        if len(lost) >= 3 and len(lost) * 3 >= len(added):
            bad.append(f'{c[:9]} {f}: {len(lost)}/{len(added)} of its added lines are gone in HEAD')
if bad:
    print('push.sh: HEAD takes back other commits\' work (stale tree?):', *bad, sep='\n  ', file=sys.stderr)
    sys.exit(1)
PY
echo "push.sh: $(git diff --stat "$remote" HEAD | tail -1)"
[ -n "${PUSH_DRY:-}" ] && exit 0
git push origin "HEAD:$branch"
