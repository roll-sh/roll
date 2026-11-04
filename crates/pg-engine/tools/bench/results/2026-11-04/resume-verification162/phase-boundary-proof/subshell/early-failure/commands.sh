set -euo pipefail
V=/home/meszmate/.cache/pg-engine-resume-20261104/verification/phase-boundary-proof/subshell/early-failure
marker=/home/meszmate/.cache/pg-engine-resume-20261104/verification/phase-boundary-proof/subshell/early-failure/success-marker
rm -f -- "$marker"
phase() {
 local name=$1; shift
 printf '%s\tSTART\t%s\n' "$name" "$(date -u +%FT%TZ)" >> "$V/parity-status.tsv"
 local rc
 set +e
 ( set -euo pipefail; "$@" ) > "$V/$name.log" 2>&1
 rc=$?
 set -e
 printf '%s\t%s\t%s\n' "$name" "$rc" "$(date -u +%FT%TZ)" >> "$V/parity-status.tsv"
 if ((rc != 0)); then tail -35 "$V/$name.log"; exit "$rc"; fi
}
fixture() { sh -c 'exit 23'; touch "$marker"; }
phase fixture fixture
