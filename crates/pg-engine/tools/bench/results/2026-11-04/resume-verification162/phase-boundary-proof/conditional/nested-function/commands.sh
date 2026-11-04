set -euo pipefail
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/phase-boundary-proof/conditional/nested-function
marker=/home/meszmate/.cache/pg-engine-resume-20261104/verification/phase-boundary-proof/conditional/nested-function/success-marker
rm -f -- "$marker"
phase() {
 local name=$1; shift
 printf '%s\tSTART\t%s\n' "$name" "$(date -u +%FT%TZ)" >> "$V/parity-status.tsv"
 local rc=0
 "$@" > "$V/$name.log" 2>&1 || rc=$?
 printf '%s\t%s\t%s\n' "$name" "$rc" "$(date -u +%FT%TZ)" >> "$V/parity-status.tsv"
 if ((rc != 0)); then tail -35 "$V/$name.log"; exit "$rc"; fi
}
inner() { sh -c 'exit 31'; }
fixture() { inner; touch "$marker"; }
phase fixture fixture
