#!/usr/bin/env python3
"""The scoreboard table from scoreboard.sh's SCORE / GATE lines.

    scoreboard.py <score.tsv> <mode> <rounds> <idle secs> [--update]

Per workload (the median of the rounds on each side), four ratios, engine against PostgreSQL,
each written so that above 1x means the engine is ahead:
  - throughput: engine tps / PostgreSQL tps (fixed-time phases), else PostgreSQL's wall time /
    the engine's (fixed work);
  - CPU per operation: PostgreSQL's server CPU per operation / the engine's (fixed work: per
    phase, the operations being the same);
  - peak memory: PostgreSQL's (cgroup anon + shmem) / the engine's (RSS);
  - bytes written: PostgreSQL's / the engine's (a CHECKPOINT ends every phase).
A ratio below 1x is bold: a place the engine doesn't beat PostgreSQL yet. The footprint gate
compares the engine with PostgreSQL and with its recorded lead (GATE_BASELINE, below): a gate
value may not grow past its baseline by more than the tolerance.

--update replaces the block between `<!-- scoreboard:begin -->` and `<!-- scoreboard:end -->`
in BENCH.md (next to this script's directory's parent) with the table.
"""
import datetime
import os
import statistics
import subprocess
import sys

# The footprint lead measured 2026-11-03 (fresh engine, fresh PostgreSQL), and how far a run
# may drift above it before the gate fails (noise: absolute slack plus a share).
GATE_BASELINE = {
    'idle_cpu_s_5min': (0.01, 0.05, 0.25),
    'idle_100_sessions_mb': (63.0, 2.0, 0.10),
    'fresh_data_mb': (2.0, 0.5, 0.10),
}
GATE_NAMES = {
    'idle_cpu_s_5min': 'idle CPU over 5 minutes (s)',
    'idle_100_sessions_mb': 'memory with 100 idle sessions (MB)',
    'fresh_data_mb': 'fresh data directory (MB)',
}


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def load(path):
    score, gate, order = {}, {}, []
    for line in open(path):
        f = line.rstrip('\n').split('\t')
        if f[0] == 'SCORE' and len(f) >= 5:
            _, rnd, t, w = f[:4]
            kv = dict(x.split('=', 1) for x in f[4:] if '=' in x)
            if w not in order:
                order.append(w)
            score.setdefault((w, t), []).append(kv)
        elif f[0] == 'GATE' and len(f) == 4:
            gate.setdefault(f[2], {})[f[1]] = num(f[3])
    return score, gate, order


def med(runs, k):
    v = [num(r.get(k)) for r in runs if r.get('ok') == '1']
    v = [x for x in v if x is not None]
    return statistics.median(v) if v else None


def ratio(a, b):
    if a is None or b is None or a <= 0 or b <= 0:
        return None
    return a / b


def cell(r):
    if r is None:
        return 'n/a'
    s = f'{r:.2f}x'
    return s if r >= 1 else f'**{s}**'


def fmt(v, unit=''):
    if v is None:
        return 'n/a'
    if v >= 1000:
        return f'{v:,.0f}{unit}'
    return f'{v:.3g}{unit}'


def table(score, gate, order, mode, rounds, idle):
    rows, wins, cells = [], 0, 0
    for w in order:
        pg, en = score.get((w, 'pg'), []), score.get((w, 'engine'), [])
        kind = (pg or en)[0].get('kind', 'work')
        if kind == 'time':
            tp = ratio(med(en, 'tps'), med(pg, 'tps'))
            shown = f"{fmt(med(en, 'tps'))} / {fmt(med(pg, 'tps'))} tps"
            cpu_op = lambda rs: (med(rs, 'cpu_s') / med(rs, 'ops')) if med(rs, 'cpu_s') and med(rs, 'ops') else None
            cp = ratio(cpu_op(pg), cpu_op(en))
        else:
            tp = ratio(med(pg, 'secs'), med(en, 'secs'))
            shown = f"{fmt(med(en, 'secs'))} / {fmt(med(pg, 'secs'))} s"
            cp = ratio(med(pg, 'cpu_s'), med(en, 'cpu_s'))
        mem = ratio(med(pg, 'peak_mb'), med(en, 'peak_mb'))
        wr_pg, wr_en = med(pg, 'write_mb'), med(en, 'write_mb')
        # (Under 1 MB written on both sides: nothing to compare.)
        wr = None if (wr_pg or 0) < 1 and (wr_en or 0) < 1 else ratio(max(wr_pg or 0, 0.1), max(wr_en or 0, 0.1))
        for r in (tp, cp, mem, wr):
            if r is not None:
                cells += 1
                wins += r >= 1
        bad = sum(1 for x in pg + en if x.get('ok') != '1')
        note = f' ({bad} failed runs)' if bad else ''
        rows.append(f'| {w}{note} | {shown} | {cell(tp)} | {cell(cp)} | {cell(mem)} | {cell(wr)} |')
    try:
        commit = subprocess.run(['git', 'rev-parse', '--short', 'HEAD'], capture_output=True, text=True,
                                cwd=os.path.dirname(os.path.abspath(__file__))).stdout.strip()
    except OSError:
        commit = '?'
    out = [
        f'Scoreboard, {mode} mode ({rounds} interleaved rounds, medians), engine at `{commit}`, '
        f'{datetime.date.today().isoformat()}: the engine ahead in **{wins} of {cells}** cells. '
        'Ratios are engine against PostgreSQL 18.6 on the same cores; above 1x the engine is ahead '
        '(bold: behind). `tools/bench/scoreboard.sh quick|full`.',
        '',
        '| workload | engine / PostgreSQL | throughput | CPU per op | peak memory | bytes written |',
        '|---|---|---|---|---|---|',
        *rows,
        '',
        f'Footprint gate (fresh servers, one at a time; idle CPU measured over {idle:g} s and scaled '
        'to 5 minutes):',
        '',
        '| gate | engine | PostgreSQL | lead | baseline | status |',
        '|---|---|---|---|---|---|',
    ]
    failed = False
    for k, name in GATE_NAMES.items():
        g = gate.get(k, {})
        e, p = g.get('engine'), g.get('pg')
        base, slack, share = GATE_BASELINE[k]
        ok = e is not None and p is not None and e <= max(base + slack, base * (1 + share)) and e < p
        failed |= not ok
        lead = 'n/a' if not e or not p else f'{p / e:.1f}x' if e > 0 else 'inf'
        out.append(f'| {name} | {fmt(e)} | {fmt(p)} | {lead} | {base:g} | {"pass" if ok else "**FAIL**"} |')
    return '\n'.join(out), failed


def main():
    path, mode, rounds, idle = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
    score, gate, order = load(path)
    text, failed = table(score, gate, order, mode, rounds, idle)
    print(text)
    if '--update' in sys.argv[5:]:
        bench = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..', 'BENCH.md')
        src = open(bench).read()
        a, b = '<!-- scoreboard:begin -->', '<!-- scoreboard:end -->'
        if a not in src or b not in src:
            sys.exit(f'{bench}: no scoreboard markers')
        head, rest = src.split(a, 1)
        _, tail = rest.split(b, 1)
        open(bench, 'w').write(f'{head}{a}\n{text}\n{b}{tail}')
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
