#!/usr/bin/env python3
"""How much of a PGO profile the build that used it could not apply (a stale profile).

    tools/pgo-stale.py <.profdata> <build log> [<warn percent>]

<build log> is the optimised build's output with LLVM's profile warnings left on
(tools/dist-pgo.sh builds with `-Cllvm-args=-pgo-warn-missing-function` and without
`-no-pgo-warn-mismatch`, and writes the log to <out>/build-use.log). A function whose code changed
since the profile was made has another control-flow hash: LLVM drops its counts ("function control
flow change detected (hash mismatch) <symbol> ... up to <n> count discarded") and compiles it as if
it never ran. A function the profile does not know ("no profile data available for function") is
new since then, or the profile names it differently: Rust symbols carry each crate's
disambiguator (`Cs<hash>_`, from cargo's -C metadata), which changes with the crate's or its
dependencies' features (a test build's dev-dependencies can do that: crates/pg-engine/Cargo.toml's
build-dependencies note), and then no function of that crate matches. The share of the hot counts
lost that way alone is printed in parentheses after "under another crate id". So a hot function counts as lost when it mismatched, or when the build has no data
for a function of the same name up to crate disambiguators.

The profile's hot functions are those with a counter at or above LLVM's hot cutoff (the count at
which the counters, largest first, make 99% of the total: the ProfileSummary's 990000, as
tools/pgo-hotcov.py). Prints one line: how many of them, and what share of their counts, the build
dropped, and how many functions were new; with "WARNING" when that share of counts is past
<warn percent> (default 1). Exit status 0 either way: the caller decides.
"""
import importlib.util
import os
import re
import sys

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('hotcov', os.path.join(os.path.dirname(os.path.abspath(__file__)), 'pgo-hotcov.py'))
hotcov = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hotcov)

DISAMBIG = re.compile(r'Cs[0-9A-Za-z]*_(?=[0-9])')
MISMATCH = re.compile(r'function control flow change detected \(hash mismatch\) (\S+)')
MISSING = re.compile(r'no profile data available for function (\S+)')


def main():
    prof, log = sys.argv[1], sys.argv[2]
    warn = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0
    counters = hotcov.counters(prof)
    funcs = {}
    for key, vals in counters.items():
        # "<codegen unit>;<symbol>#<hash>" for local symbols, "<symbol>#<hash>" for others.
        sym = key.rsplit('#', 1)[0].rsplit(';', 1)[-1]
        f = funcs.setdefault(sym, [0, 0])
        f[0] = max(f[0], max(vals, default=0))
        f[1] += sum(vals)
    total = sum(t for _, t in funcs.values())
    ran = sum(1 for m, _ in funcs.values() if m > 0)
    # A function is hot when its hottest block is.
    cut = cutoff(counters)
    hot = {s: t for s, (m, t) in funcs.items() if m >= cut and m > 0}
    hot_total = sum(hot.values())
    stale, new = set(), set()
    with open(log, errors='replace') as fh:
        for line in fh:
            # (Build scripts take the same flags; they are not part of the binary.)
            if 'build_script_build' in line:
                continue
            m = MISMATCH.search(line)
            if m:
                stale.add(m.group(1))
                continue
            m = MISSING.search(line)
            if m:
                new.add(m.group(1))
    unmatched = {DISAMBIG.sub('C_', s) for s in new}
    changed = [s for s in stale if s in hot]
    renamed = [s for s in hot if s not in stale and DISAMBIG.sub('C_', s) in unmatched]
    hs = changed + renamed
    lost = sum(hot[s] for s in hs)
    share = 100 * lost / hot_total if hot_total else 0.0
    rshare = 100 * sum(hot[s] for s in renamed) / hot_total if hot_total else 0.0
    line = (f'profile staleness: {len(hs)} of {len(hot)} hot functions without profile data in this build '
            f'({len(changed)} changed, {len(renamed)} under another crate id ({rshare:.2f}%); {share:.2f}% of the hot '
            f'functions\' counts); {len(stale)} changed functions in all, {len(new)} without data; profile {total} '
            f'counts, {ran} functions ran')
    if share > warn:
        line = f'WARNING: {line}. Past {warn:g}%: the profile is stale, train again (tools/dist-pgo.sh without --profdata)'
    print(line)
    if hs:
        top = sorted(hs, key=lambda s: -hot[s])[:10]
        print('  most counts dropped: ' + ', '.join(f'{s} {100 * hot[s] / hot_total:.2f}%' for s in top))


def cutoff(counters):
    """LLVM's hot count: the counter value at which the counters, largest first, reach 99%."""
    vals = sorted((x for v in counters.values() for x in v if x > 0), reverse=True)
    goal, acc = 0.99 * sum(vals), 0
    for x in vals:
        acc += x
        if acc >= goal:
            return x
    return 1


if __name__ == '__main__':
    main()
