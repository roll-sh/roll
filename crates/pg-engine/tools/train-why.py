"""Why a verification step failed, from its logs (tools/ref-suites.sh, verification-train.sh).

    train-why.py start <stage log> <engine log>...      exit 0 and print the reason when the
                                                        engine did not start (or its port was taken)
    train-why.py regress <log> <REPORT.md> <fails out>  print the reason; write the failing names
    train-why.py iso <log> <REPORT.md> <fails out>
    train-why.py dump <log> <TAP.md> <fails out>
    train-why.py libtest <binary name> <log> <fails out>   (appends)
    train-why.py panic <log> <name> <fails out>            (appends)

A fails file has one "<name>\\t<message>" line per failing test (a regress unit as file:unit, an
isolation spec, a pg_dump TAP check as file: description, a crate test as binary::test): the
batch train's flake ledger keys on the names.
"""
import re
import sys


def read(path):
    try:
        with open(path, errors='replace') as f:
            return f.read()
    except OSError:
        return ''


def panic_message(text):
    """The first panic's message: the line after "panicked at <file>:<line>:<col>:"."""
    lines = text.splitlines()
    for i, l in enumerate(lines):
        m = re.search(r"panicked at [^ ]+:\d+:\d+:(.*)$", l)
        if m:
            msg = m.group(1).strip() or (lines[i + 1].strip() if i + 1 < len(lines) else '')
            return msg[:200]
    return ''


def start(stage_log, *engine_logs):
    text = read(stage_log)
    # The harnesses' own checks before they start the engine (pg_dump.rs, pg_isolation.rs).
    m = re.search(r'port (\d+) is (?:taken|in use)', text)
    if m:
        print(f'port {m.group(1)} in use before the engine started')
        return 0
    if 'did not start' not in text:
        return 1
    why = 'engine did not start'
    for e in engine_logs:
        m = re.search(r'.*(Address already in use|os error 98).*', read(e))
        if m:
            port = re.search(r':(\d{2,5})\b', m.group(0))
            why += f" (port {port.group(1) + ' ' if port else ''}in use: {m.group(1)})"
            break
    else:
        tails = [l for e in engine_logs for l in read(e).splitlines()[-3:] if l.strip()]
        if tails:
            why += f' (engine log: {tails[-1][:160]})'
    print(why)
    return 0


def write_fails(path, rows, append=False):
    with open(path, 'a' if append else 'w') as f:
        for name, msg in rows:
            f.write(f"{name}\t{' '.join(msg.split())[:200]}\n")


def regress(log, report, out):
    rep = read(report)
    rows, cut = [], rep.find('| file | units | newly differing | newly matching |')
    if cut >= 0:
        for l in rep[cut:].splitlines()[2:]:
            c = [x.strip() for x in l.strip().strip('|').split('|')]
            if len(c) < 3 or not l.startswith('|'):
                break
            rows += [(f'{c[0]}:{u.strip()}', 'newly differing') for u in c[2].split(',') if u.strip()]
    m = re.search(r'Newly differing units: (\d+)', rep)
    write_fails(out, rows)
    if m and rows:
        names = ', '.join(n for n, _ in rows[:8]) + (' ...' if len(rows) > 8 else '')
        print(f'{m.group(1)} newly differing units: {names}')
    elif m:
        print(f'{m.group(1)} newly differing units')
    else:
        p = panic_message(read(log))
        write_fails(out, [('pg_regress', p or 'no report')])
        print(f"no report: {p or 'the harness stopped early'}")


def iso(log, report, out):
    rows = []
    for l in read(report).splitlines():
        m = re.match(r'\| ([^|]+?) \| (\d+)/(\d+) \|', l)
        if m and m.group(2) != m.group(3):
            rows.append((m.group(1), f'{m.group(2)}/{m.group(3)} permutations match'))
    write_fails(out, rows)
    text = read(log)
    p = panic_message(text)
    if p and not rows:
        write_fails(out, [('pg_isolation', p)])
    m = re.findall(r'\d+/\d+ permutations', text)
    print(f"{m[-1] if m else 'no permutation count'}"
          + (f"; specs differing: {', '.join(n for n, _ in rows[:8])}" if rows else '')
          + (f'; {p}' if p and not rows else ''))


def dump(log, tap, out):
    rows = []
    for l in read(tap).splitlines():
        m = re.match(r"- ([^:]+): `not ok \d+ - (.*)`$", l)
        if m:
            rows.append((f'{m.group(1)}: {m.group(2)}', 'not ok'))
        elif l.startswith('- '):
            rows.append((l[2:].replace('`', ''), 'error'))
    text = read(log)
    p = panic_message(text)
    if p and not rows:
        rows.append(('pg_dump_tap', p))
    write_fails(out, rows)
    counts = re.findall(r'(\S+\.pl): (\d+) passed, (\d+) failed', text)
    failed = [f'{f} {n} failed' for f, _, n in counts if n != '0']
    print('; '.join(failed + [f'{n}' for n, _ in rows[:5]]) or p or 'no counts')


def libtest(binary, log, out):
    """Each failing test's panic message, from libtest's "---- <test> stdout ----" blocks."""
    text = read(log)
    rows = []
    for name in re.findall(r'^test (\S+) \.\.\. FAILED$', text, re.M):
        block = re.search(rf'^---- {re.escape(name)} stdout ----\n(.*?)(?=^---- |^failures:$|\Z)', text, re.M | re.S)
        rows.append((f'{binary}::{name}', panic_message(block.group(1)) if block else ''))
    if not rows:
        rows.append((binary, panic_message(text) or 'exit without a failed test'))
    write_fails(out, rows, append=True)


def panic(log, name, out):
    p = panic_message(read(log)) or (read(log).strip().splitlines() or [''])[-1][:200]
    write_fails(out, [(name, p)], append=True)
    print(p)


if __name__ == '__main__':
    kind, args = sys.argv[1], sys.argv[2:]
    if kind == 'start':
        sys.exit(start(*args))
    {'regress': regress, 'iso': iso, 'dump': dump, 'libtest': libtest, 'panic': panic}[kind](*args)
    sys.exit(0)
