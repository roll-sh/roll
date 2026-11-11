#!/usr/bin/env python3
"""Markdown tables from benchmark RESULT lines (tools/bench/suite.sh's results/*.tsv).

    report.py <results dir> <label>...   # columns in the given order, the first is the base

Each table row is one (workload, case); a cell is the case's headline metric (tps, or the
median / per-op time), with the ratio to the first label's when it has one.
"""
import os
import sys

HEADLINE = ['tps', 'median_s', 'per_query_ms', 'per_op_ms', 'per_connect_ms', 'kb_per_session',
            'kb_per_database_with_session', 'secs', 'copy_s', 'min_s', 'connect_s']
HIGHER_BETTER = {'tps'}


def load(path, label):
    rows = {}
    for line in open(path):
        f = line.rstrip('\n').split('\t')
        if len(f) < 4 or f[0] != 'RESULT':
            continue
        lab, wl, case = f[1], f[2], f[3]
        kv = dict(x.split('=', 1) for x in f[4:] if '=' in x)
        # Suite labels carry a size suffix (<label>-200000): keep it in the workload.
        suffix = lab[len(label):].lstrip('-') if lab.startswith(label) else ''
        if 'clients' in kv and wl in ('pgbench', 'oltp'):
            case = f"{case} c{kv['clients']}"
        key = (f"{wl} {suffix}".strip(), case)
        rows[key] = kv
    return rows


def headline(kv):
    for k in HEADLINE:
        if k in kv and kv[k] not in ('', None):
            try:
                return k, float(kv[k])
            except ValueError:
                pass
    return None, None


def main():
    d, labels = sys.argv[1], sys.argv[2:]
    data = [load(os.path.join(d, f'{l}.tsv'), l) for l in labels]
    keys = []
    for rows in data:
        for k in rows:
            if k not in keys:
                keys.append(k)
    by_wl = {}
    for k in keys:
        by_wl.setdefault(k[0], []).append(k[1])
    for wl, cases in by_wl.items():
        print(f'\n#### {wl}\n')
        print('| case | metric | ' + ' | '.join(labels) + ' |')
        print('|---|---|' + '---|' * len(labels))
        for c in cases:
            cells = []
            metric = None
            base = None
            for i, rows in enumerate(data):
                kv = rows.get((wl, c))
                if kv is None:
                    cells.append('')
                    continue
                m, v = headline(kv)
                if m is None:
                    cells.append(kv.get('error', '')[:40] or '')
                    continue
                metric = metric or m
                cell = f'{v:g}'
                if 'error' in kv or kv.get('errors', '0') not in ('0', ''):
                    cell += ' (err)'
                if i == 0:
                    base = v
                elif base:
                    r = v / base if m in HIGHER_BETTER else base / v if v else 0
                    cell += f' ({r:.2f}x)'
                cells.append(cell)
            print(f'| {c} | {metric or ""} | ' + ' | '.join(cells) + ' |')


main()
