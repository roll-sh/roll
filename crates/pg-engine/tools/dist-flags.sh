# Sourced by tools/dist.sh and tools/dist-pgo.sh: DIST_RUSTFLAGS, the code-generation flags of
# the shipped roll-engine builds on top of the `dist` profile (HANDOFF "Build flags", agent 236).
#
# x86-64: LLVM pads branches so that none crosses or ends on a 32-byte boundary. Intel cores from
# Skylake to Cascade Lake (erratum SKX102, the "JCC erratum") don't cache such a branch's code in
# the decoded-uop cache after the microcode fix, so a hot loop runs 5-20% slower or faster
# depending on where unrelated changes put it. Measured on every benchmarked class (BENCH.md
# "Build flags"): TPC-H -5 to -7% user cycles (PGO), range reads -1 to -2%, point reads, short
# transactions and the bulk script within their noise, for +2.5% code and +2-4% instructions.
# No target-cpu (x86-64 baseline): v2 measured no faster, v3 about 1% on TPC-H only and not
# every production CPU is guaranteed to have AVX2.
DIST_RUSTFLAGS=
case $(rustc -vV | sed -n 's/^host: //p') in
  x86_64-*) DIST_RUSTFLAGS="-Cllvm-args=-x86-branches-within-32B-boundaries" ;;
esac
