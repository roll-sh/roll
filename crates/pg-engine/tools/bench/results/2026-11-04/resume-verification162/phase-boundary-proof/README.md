# Phase boundary regression proof

Eight no-service controls compare the exact saved original conditional phase and repaired subshell phase. The helper is `../prove-phase-boundary.py`; it extracts both saved implementations, runs each case in its own Bash process, and rejects an unexpected exit or marker outcome.

Re-run from the artifact directory using the existing shared heavy wrapper (no services, benchmark, cargo or source suite):

```sh
/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc/crates/pg-engine/tools/heavy.sh python3 /home/meszmate/.cache/pg-engine-resume-20261104/verification/prove-phase-boundary.py
```

For a relocated evidence bundle, invoke its copy of `prove-phase-boundary.py`; the saved phase implementations are resolved relative to that helper. Each case preserves `commands.sh`, `stdout.log`, `stderr.log`, `fixture.log`, `parity-status.tsv` and `exit.json`. `proof.json` records all eight outcomes and SHA256 identities for the phase function bytes.

Observed: original conditional phase falsely returns 0 and creates the success marker after command failure23, pipeline failure29 and nested-function failure31. Repaired phase returns the exact injected failure and never creates the marker. Both positive controls return0 and create the marker. Wrapper status alone was not used to accept these results.

The active parity runner and pinned source were not edited for this proof.
