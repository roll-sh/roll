#!/usr/bin/env python3
"""The scoreboard table from scoreboard.sh's SCORE / GATE lines.

    scoreboard.py <score.tsv> <mode> <rounds> <idle secs> [--update]

Per workload, four ratios, engine against PostgreSQL, each the median of the rounds' own ratios
(the sides run back to back within a round) and written so that above 1x means the engine is
ahead:
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
    # Re-recorded from 63 (the host engine's RSS) when the engine moved into a container of its
    # own (2026-11-03, agent 156): its cgroup's anon + shmem, as PostgreSQL's is measured. RSS
    # counted the mapped binary's pages; the cgroup's anon doesn't, so the same engine reads
    # about half and the old baseline would no longer guard anything.
    'idle_100_sessions_mb': (30.0, 2.0, 0.10),
    # Re-recorded from 2.0 the scoreboard's way: the running server with template0, template1
    # and postgres open, a preallocated 1 MB WAL file each (the 2.0 was a du with fewer open).
    'fresh_data_mb': (3.0, 0.5, 0.10),
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
            kv['round'] = rnd
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


def cell(r, split=False):
    if r is None:
        return 'n/a'
    s = f'{r:.2f}x' + ('~' if split else '')
    return s if r >= 1 else f'**{s}**'


def per_round(pg, en, ratio_of):
    """The ratio within each round: the two sides ran back to back, so load that drifts over
    the run (other agents' builds take the same cores from servers at nice 19) cancels."""
    rs = []
    for rnd in sorted({x['round'] for x in pg} & {x['round'] for x in en}):
        a = [x for x in pg if x['round'] == rnd]
        b = [x for x in en if x['round'] == rnd]
        r = ratio_of(a, b)
        if r is not None:
            rs.append(r)
    return rs


def paired(pg, en, ratio_of):
    """A cell: the median of the rounds' ratios (else the ratio of the medians), and whether the
    rounds disagree on which side is ahead (a cell within the noise)."""
    rs = per_round(pg, en, ratio_of)
    if not rs:
        return ratio_of(pg, en), False
    return statistics.median(rs), len(rs) > 1 and min(rs) < 1 <= max(rs)


def fmt(v, unit=''):
    if v is None:
        return 'n/a'
    if v >= 1000:
        return f'{v:,.0f}{unit}'
    return f'{v:.3g}{unit}'


def ratios(kind, other, en):
    """The four cells of the engine against another server's runs (`other`)."""
    if not any(x.get('ok') == '1' for x in other):
        return [(None, False)] * 4
    cpu_op = lambda rs: (med(rs, 'cpu_s') / med(rs, 'ops')) if med(rs, 'cpu_s') and med(rs, 'ops') else None
    if kind == 'time':
        tp_of = lambda p, e: ratio(med(e, 'tps'), med(p, 'tps'))
        cp_of = lambda p, e: ratio(cpu_op(p), cpu_op(e))
    else:
        tp_of = lambda p, e: ratio(med(p, 'secs'), med(e, 'secs'))
        cp_of = lambda p, e: ratio(med(p, 'cpu_s'), med(e, 'cpu_s'))
    mem_of = lambda p, e: ratio(med(p, 'peak_mb'), med(e, 'peak_mb'))
    out = [paired(other, en, f) for f in (tp_of, cp_of, mem_of)]
    wr_o, wr_en = med(other, 'write_mb'), med(en, 'write_mb')
    # (Under 1 MB written on both sides: nothing to compare.)
    wr = None if (wr_o or 0) < 1 and (wr_en or 0) < 1 else ratio(max(wr_o or 0, 0.1), max(wr_en or 0, 0.1))
    return out + [(wr, False)]


def table(score, gate, order, mode, rounds, idle):
    # pgrust (a third server, github.com/malisper/pgrust) when the run had it.
    third = any(t == 'pgrust' for (_, t) in score)
    rows, wins, cells, pwins, pcells, failures = [], 0, 0, 0, 0, []
    for w in order:
        pg, en, pr = (score.get((w, t), []) for t in ('pg', 'engine', 'pgrust'))
        kind = (pg or en)[0].get('kind', 'work')
        key, unit = ('tps', ' tps') if kind == 'time' else ('secs', ' s')
        shown = f"{fmt(med(en, key))} / {fmt(med(pg, key))}" + (f" / {fmt(med(pr, key))}" if third else '') + unit
        vs_pg = ratios(kind, pg, en)
        for r, _ in vs_pg:
            if r is not None:
                cells += 1
                wins += r >= 1
        bad = sum(1 for x in pg + en if x.get('ok') != '1')
        note = f' ({bad} failed runs)' if bad else ''
        if not third:
            rows.append(f'| {w}{note} | {shown} | ' + ' | '.join(cell(r, s) for r, s in vs_pg) + ' |')
            continue
        vs_pr = ratios(kind, pr, en)
        for r, _ in vs_pr:
            if r is not None:
                pcells += 1
                pwins += r >= 1
        errs = [x.get('err') or 'failed' for x in pr if x.get('ok') != '1']
        if errs:
            failures.append(f'- {w}: pgrust {len(errs)} of {len(pr)} runs failed: `{errs[0]}`')
        rows.append(f'| {w}{note} | {shown} | '
                    + ' | '.join(f'{cell(a, sa)} | {cell(b, sb)}' for (a, sa), (b, sb) in zip(vs_pg, vs_pr)) + ' |')
    commit = os.environ.get('SB_COMMIT')
    try:
        commit = commit or subprocess.run(['git', 'rev-parse', '--short', 'HEAD'], capture_output=True, text=True,
                                cwd=os.path.dirname(os.path.abspath(__file__))).stdout.strip()
    except OSError:
        commit = '?'
    if third:
        header = [
            f'Scoreboard, {mode} mode ({rounds} interleaved rounds, the median of the rounds\' ratios), '
            f'engine at `{commit}`, {datetime.date.today().isoformat()}, all three servers in containers on '
            f'the same cores: the engine ahead of PostgreSQL 18.6 in **{wins} of {cells}** cells and of '
            f'pgrust in **{pwins} of {pcells}**. Ratios are the engine against each (÷PG, ÷pgrust); above 1x '
            'the engine is ahead (bold: behind; ~: the rounds disagree on which is ahead). '
            '`tools/bench/scoreboard.sh quick|full`.',
            '',
            '| workload | engine / PostgreSQL / pgrust | throughput ÷PG | ÷pgrust | CPU per op ÷PG | ÷pgrust '
            '| peak memory ÷PG | ÷pgrust | bytes written ÷PG | ÷pgrust |',
            '|---|---|---|---|---|---|---|---|---|---|',
            *rows,
        ]
        if failures:
            header += ['', 'pgrust failures (its cells n/a):', *failures]
    else:
        header = [
            f'Scoreboard, {mode} mode ({rounds} interleaved rounds, the median of the rounds\' ratios), '
            f'engine at `{commit}` (in a container, as PostgreSQL), '
            f'{datetime.date.today().isoformat()}: the engine ahead in **{wins} of {cells}** cells. '
            'Ratios are engine against PostgreSQL 18.6 on the same cores; above 1x the engine is ahead '
            '(bold: behind; ~: the rounds disagree on which is ahead). `tools/bench/scoreboard.sh quick|full`.',
            '',
            '| workload | engine / PostgreSQL | throughput | CPU per op | peak memory | bytes written |',
            '|---|---|---|---|---|---|',
            *rows,
        ]
    out = header + [
        '',
        f'Footprint gate (fresh servers, one at a time; idle CPU measured over {idle:g} s and scaled '
        'to 5 minutes):',
        '',
        '| gate | engine | PostgreSQL | lead | baseline | status |' + (' pgrust |' if third else ''),
        '|---|---|---|---|---|---|' + ('---|' if third else ''),
    ]
    failed = False
    for k, name in GATE_NAMES.items():
        g = gate.get(k, {})
        e, p = g.get('engine'), g.get('pg')
        base, slack, share = GATE_BASELINE[k]
        ok = e is not None and p is not None and e <= max(base + slack, base * (1 + share)) and e < p
        failed |= not ok
        # (No idle CPU measured: the lead is at least PostgreSQL's over one 10 ms clock tick,
        # scaled as the measurement is.)
        floor = 0.01 * 300 / idle if k == 'idle_cpu_s_5min' else None
        lead = ('n/a' if e is None or not p else f'{p / e:.1f}x' if e > 0
                else f'>{p / floor:.1f}x' if floor else 'n/a')
        out.append(f'| {name} | {fmt(e)} | {fmt(p)} | {lead} | {base:g} | {"pass" if ok else "**FAIL**"} |'
                   + (f' {fmt(g.get("pgrust"))} |' if third else ''))
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
