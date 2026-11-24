#!/usr/bin/env python3
"""SIGKILL a roll-engine binary under concurrent writes, restart it, and check what survived.

    tools/crash-binary.py <roll-engine> [--seed N] [--rounds N] [--port P] [--keep DIR] [--set NAME=VALUE]

The in-process crash tests (tests/crash_*.rs) run the engine as a library inside a test binary.
This one runs the binary itself, given by path, so it checks the build users run: the plain dist
build or the profile-guided one (tools/dist-pgo.sh --out <dir>: <dir>/roll-engine). It needs only
python3 (its own wire client, trust authentication) and takes 1-2 minutes in its default form.

The server is started on a fresh data directory (`--data`, `--listen 127.0.0.1:<port>`, the
shipped defaults otherwise) and loaded with a TPC-B-like schema: accounts (primary key, a unique
`code`, an index on `bid`), tellers, branches, history (primary key the client's transaction id,
an index on `aid`), and `bulk` (primary key (batch, n), an index on payload). Each round, 8 writers run
TPC-B transactions (one account, teller and branch updated by the same delta and a history row,
one simple-query message from BEGIN to COMMIT; one in 16 ends in ROLLBACK), a churn connection
puts in batches of `bulk` rows with INSERT ... SELECT and deletes older ones, and once a seeded
number of transactions (100-999) were acknowledged (or 15 s passed: a loaded machine) the round's
action runs; then the server process is killed with SIGKILL, by its pid, at a seeded moment:

  tpcb          a seeded time into the writes;
  file          just after the seeded nth .sst / .wal / .tmp file appears in the data directory
                (a flush, a compaction or a WAL rotation starting to write);
  create-index  a seeded time into CREATE INDEX on history (tid, txid);
  copy          a seeded time into COPY bulk FROM STDIN of 150,000 rows;
  vacuum        a seeded 0-2 ms after the first file VACUUM accounts writes (its files written
                again: a compaction of the table), else a seeded 0.5-5 s into it;
  checkpoint    the same for CHECKPOINT (a flush of every memtable);
  close         a seeded time into a fast shutdown (SIGINT: sessions end, shutdown checkpoint).

The rounds cycle through the kinds in a seeded order (the default 7 rounds: each kind once).
After each kill the server starts again on the same directory (the time to accept connections is
printed) and is checked against what the clients saw:
  - every transaction acknowledged (COMMIT's CommandComplete and ReadyForQuery) is in history,
    with the account, teller, branch and delta the client sent; no history row is a transaction
    the client rolled back, or one in flight at an earlier kill that recovery left out;
  - atomicity: every account's, teller's and branch's balance is the sum of its history rows'
    deltas (a transaction's four writes all there or none), and the four sums are equal;
  - each bulk batch has all its rows or none: all when its INSERT or COPY was acknowledged, none
    when its DELETE was; one in flight is either, then fixed for later rounds;
  - every index returns what a sequential scan does (count and the ordered rows' md5, the plan
    checked to use that index), and primary and unique keys have no duplicates;
  - an index whose CREATE INDEX was in flight is either absent or valid and agrees with the
    table; one acknowledged is there (then it is dropped, and stays dropped).
The next round writes on from that state. At the end the server shuts down cleanly (SIGINT, its
exit waited for), starts again and is checked once more.

A failure prints the seed and round, keeps the data directory (and the server log, server.log in
it) and exits 1: `--seed <seed>` repeats the round's plan (kill points and the order of kinds;
the writers' interleaving is the machine's). Run alone:

    crates/pg-engine/tools/crash-binary.py target/dist/roll-engine --seed 7 --rounds 14

quick-check.sh runs it in its crash step with QC_PGO (against <QC_PGO>/roll-engine), and
verification-train.sh in its PGO stage (TRAIN_PGO_PROFDATA's binary). Environment: CRASH_CPUS
(taskset CPUs of the server, e.g. 2,3,20,21), TMPDIR (where the data directory goes).
"""
import argparse
import hashlib
import os
import random
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

KINDS = ['tpcb', 'file', 'create-index', 'copy', 'vacuum', 'checkpoint', 'close']
ACCOUNTS, TELLERS, BRANCHES = 60_000, 60, 6
WRITERS = 8
CHURN_ROWS = 20_000
COPY_ROWS = 150_000


class PgError(Exception):
    def __init__(self, fields):
        self.code = fields.get('C', '?')
        # FATAL / PANIC: the session ends (a shutdown's 57P01): what it was running is in flight.
        self.fatal = fields.get('V', fields.get('S')) in ('FATAL', 'PANIC')
        super().__init__(f"{fields.get('S', 'ERROR')} {self.code}: {fields.get('M', '')}")


class Lost(Exception):
    """The connection broke: the server is gone (or going)."""


class Conn:
    """A PostgreSQL wire protocol client: trust authentication, the simple query protocol and
    COPY FROM STDIN, nothing else."""

    def __init__(self, port, db='postgres', timeout=120):
        try:
            self.s = socket.create_connection(('127.0.0.1', port), timeout=timeout)
        except OSError as e:
            raise Lost(str(e))
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = bytearray()
        body = struct.pack('!i', 196608) + b'user\0postgres\0database\0' + db.encode() + b'\0\0'
        self.send(struct.pack('!i', len(body) + 4) + body)
        while True:
            t, m = self.read()
            if t == b'R' and struct.unpack('!i', m[:4])[0] != 0:
                raise RuntimeError('the server asked for a password: run it with trust authentication')
            if t == b'E':
                raise PgError(fields(m))
            if t == b'Z':
                return

    def send(self, b):
        try:
            self.s.sendall(b)
        except OSError as e:
            raise Lost(str(e))

    def read(self):
        while True:
            if len(self.buf) >= 5:
                n = struct.unpack('!i', self.buf[1:5])[0]
                if len(self.buf) >= n + 1:
                    t, m = bytes(self.buf[:1]), bytes(self.buf[5:n + 1])
                    del self.buf[:n + 1]
                    return t, m
            try:
                b = self.s.recv(1 << 16)
            except OSError as e:
                raise Lost(str(e))
            if not b:
                raise Lost('connection closed')
            self.buf += b

    def query(self, sql, copy=None):
        """Run `sql` (one or more statements); the last result's rows as lists of str/None,
        and the command tags. Raises PgError after ReadyForQuery when a statement failed;
        the ready status is in self.status. `copy`: an iterable of bytes for COPY FROM STDIN."""
        q = sql.encode() + b'\0'
        self.send(b'Q' + struct.pack('!i', len(q) + 4) + q)
        rows, tags, err = [], [], None
        while True:
            t, m = self.read()
            if t == b'T':
                rows = []
            elif t == b'D':
                n = struct.unpack('!h', m[:2])[0]
                p, row = 2, []
                for _ in range(n):
                    ln = struct.unpack('!i', m[p:p + 4])[0]
                    p += 4
                    if ln < 0:
                        row.append(None)
                    else:
                        row.append(m[p:p + ln].decode())
                        p += ln
                rows.append(row)
            elif t == b'C':
                tags.append(m[:-1].decode())
            elif t == b'E':
                err = PgError(fields(m))
            elif t == b'G':
                for chunk in copy or ():
                    self.send(b'd' + struct.pack('!i', len(chunk) + 4) + chunk)
                self.send(b'c\0\0\0\4')
            elif t == b'Z':
                self.status = m[:1]
                if err:
                    raise err
                return rows, tags

    def one(self, sql):
        rows, _ = self.query(sql)
        return rows[0][0] if rows else None

    def close(self):
        try:
            self.send(b'X\0\0\0\4')
            self.s.close()
        except Exception:
            pass


def fields(m):
    out = {}
    for f in m.split(b'\0'):
        if f:
            out[chr(f[0])] = f[1:].decode(errors='replace')
    return out


class Server:
    def __init__(self, binary, data, port, cpus, settings=()):
        self.binary, self.data, self.port, self.cpus, self.settings = binary, data, port, cpus, settings
        self.proc = None
        self.log = open(os.path.join(data, 'server.log'), 'ab')

    def start(self):
        cmd = [self.binary, '--data', os.path.join(self.data, 'd'), '--listen', f'127.0.0.1:{self.port}']
        for kv in self.settings:
            cmd += ['-c', kv]
        if self.cpus:
            cmd = ['taskset', '-c', self.cpus] + cmd  # (taskset execs: the pid is the server's)
        self.log.write(f'--- start {time.strftime("%H:%M:%S")}\n'.encode())
        self.log.flush()
        t0 = time.monotonic()
        self.proc = subprocess.Popen(cmd, stdout=self.log, stderr=self.log, stdin=subprocess.DEVNULL)
        while True:
            if self.proc.poll() is not None:
                raise Failure(f'the server exited with {self.proc.returncode} while starting:\n{self.tail()}')
            try:
                c = Conn(self.port, timeout=5)
                c.close()
                return time.monotonic() - t0
            except (Lost, PgError):
                if time.monotonic() - t0 > 120:
                    raise Failure(f'the server did not accept connections in 120 s:\n{self.tail()}')
                time.sleep(0.01)

    def kill(self):
        """SIGKILL, by the pid this process started."""
        os.kill(self.proc.pid, signal.SIGKILL)
        self.proc.wait()

    def stop(self, timeout=120):
        """A fast shutdown, waited for: 0 when it exited cleanly."""
        os.kill(self.proc.pid, signal.SIGINT)
        try:
            return self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            os.kill(self.proc.pid, signal.SIGKILL)
            self.proc.wait()
            raise Failure(f'a fast shutdown did not finish in {timeout} s')

    def tail(self, n=25):
        self.log.flush()
        with open(os.path.join(self.data, 'server.log'), 'rb') as f:
            return '\n'.join(f.read().decode(errors='replace').splitlines()[-n:])


class Failure(Exception):
    pass


class Model:
    """What the clients saw, over all rounds."""

    def __init__(self):
        self.lock = threading.Lock()
        self.txns = {}  # txid -> (aid, tid, bid, delta)
        self.acked = set()  # committed and acknowledged
        self.flight = set()  # COMMIT sent, no answer (this round)
        self.never = set()  # rolled back, or in flight at a kill and left out by recovery
        self.batches = {}  # bulk batch -> rows it has: 'all', 'none' or 'either' (in flight)
        self.batch_rows = {}
        self.indexes = {}  # name -> 'there', 'gone' or 'either'
        self.errors = []
        self.round_acked = 0  # acknowledged in the running round
        self.resolved = (0, 0)  # the last recovery's in-flight transactions: committed, not


def writer(port, model, rng, wid, stop, counter):
    """TPC-B transactions until the connection breaks or `stop` is set."""
    try:
        c = Conn(port, 'crash')
    except Lost:
        return
    seq = 0
    while not stop.is_set():
        seq += 1
        txid = (counter << 32) | (wid << 24) | seq
        aid, tid = rng.randrange(1, ACCOUNTS + 1), rng.randrange(1, TELLERS + 1)
        bid, delta = (tid - 1) % BRANCHES + 1, rng.randrange(-5000, 5001)
        end = 'ROLLBACK' if rng.randrange(16) == 0 else 'COMMIT'
        sql = (f'BEGIN; UPDATE accounts SET abalance = abalance + {delta} WHERE aid = {aid}; '
               f'UPDATE tellers SET tbalance = tbalance + {delta} WHERE tid = {tid}; '
               f'UPDATE branches SET bbalance = bbalance + {delta} WHERE bid = {bid}; '
               f'INSERT INTO history (txid, tid, bid, aid, delta) VALUES ({txid}, {tid}, {bid}, {aid}, {delta}); {end}')
        with model.lock:
            model.txns[txid] = (aid, tid, bid, delta)
            (model.flight if end == 'COMMIT' else model.never).add(txid)
        try:
            _, tags = c.query(sql)
        except Lost:
            return
        except PgError as e:
            if e.fatal:
                return
            with model.lock:
                model.flight.discard(txid)
                model.never.add(txid)
                model.errors.append(f'writer {wid}: {e}')
            try:
                c.query('ROLLBACK')
            except (Lost, PgError):
                return
            continue
        with model.lock:
            if end == 'COMMIT':
                model.flight.discard(txid)
                if tags[-1] != 'COMMIT':
                    model.never.add(txid)
                    model.errors.append(f'writer {wid}: COMMIT answered {tags[-1]}')
                else:
                    model.acked.add(txid)
                    model.round_acked += 1
    c.close()


def batch_mark(model, b, state):
    with model.lock:
        model.batches[b] = state


def churn(port, model, rng, first, stop):
    """INSERT ... SELECT batches of bulk rows; delete one two batches back now and then."""
    try:
        c = Conn(port, 'crash')
    except Lost:
        return
    b = first
    while not stop.is_set():
        model.batch_rows[b] = CHURN_ROWS
        batch_mark(model, b, 'either')
        try:
            c.query(f"INSERT INTO bulk SELECT {b}, g, md5(({b}::bigint * 1000003 + g)::text) || repeat('c', 40) "
                    f'FROM generate_series(1, {CHURN_ROWS}) g')
        except (Lost, PgError):
            return
        batch_mark(model, b, 'all')
        old = b - 2
        if old >= first and model.batches.get(old) == 'all' and rng.randrange(2) == 0:
            batch_mark(model, old, 'either-delete')
            try:
                c.query(f'DELETE FROM bulk WHERE batch = {old}')
            except (Lost, PgError):
                return
            batch_mark(model, old, 'none')
        b += 1
    c.close()


def copy_rows(b):
    n, out = 0, []
    while n < COPY_ROWS:
        lines = []
        for g in range(n + 1, min(n + 5000, COPY_ROWS) + 1):
            lines.append(f"{b}\t{g}\t{hashlib.md5(str(b * 1000003 + g).encode()).hexdigest()}{'p' * 40}\n")
        out.append(''.join(lines).encode())
        n += 5000
    return out


def files(base):
    out = set()
    for d in os.scandir(base):
        if d.is_dir():
            try:
                for f in os.scandir(d.path):
                    if f.name.endswith(('.sst', '.wal', '.tmp')):
                        out.add(f.path)
            except FileNotFoundError:
                pass
    return out


def wait_files(base, n, deadline):
    """True once n new .sst / .wal / .tmp files appeared under base/<db>/ (False at the deadline)."""
    seen, new = files(base), 0
    while time.monotonic() < deadline:
        now = files(base)
        new += len(now - seen)
        seen |= now
        if new >= n:
            return True
        time.sleep(0.0002)
    return False


def check(port, model, where):
    """Every invariant against the restarted server; raises Failure."""
    c = Conn(port, 'crash')
    bad = []
    # History, by a sequential scan.
    c.query('SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off')
    rows, _ = c.query('SELECT txid, aid, tid, bid, delta FROM history')
    hist = {}
    for r in rows:
        t = int(r[0])
        if t in hist:
            bad.append(f'history: txid {t} twice')
        hist[t] = tuple(int(x) for x in r[1:])
    with model.lock:
        for t in model.acked - hist.keys():
            bad.append(f'acknowledged transaction {t:#x} {model.txns[t]} lost')
        for t in model.never & hist.keys():
            bad.append(f'transaction {t:#x} there: rolled back or left out by an earlier recovery')
        for t, v in hist.items():
            if t not in model.txns:
                bad.append(f'history row {t:#x} {v}: no client wrote it')
            elif model.txns[t] != v:
                bad.append(f'history row {t:#x} is {v}, the client wrote {model.txns[t]}')
        # The ones in flight: committed or not, from now on fixed.
        for t in model.flight:
            (model.acked if t in hist else model.never).add(t)
        n = sum(1 for t in model.flight if t in hist)
        model.resolved = (n, len(model.flight) - n)
        model.flight.clear()
    # Balances: each the sum of its history rows' deltas.
    want = [{}, {}, {}]
    for aid, tid, bid, delta in hist.values():
        for m, k in zip(want, (aid, tid, bid)):
            m[k] = m.get(k, 0) + delta
    sums = []
    for (table, key, col, n), m in zip((('accounts', 'aid', 'abalance', ACCOUNTS), ('tellers', 'tid', 'tbalance', TELLERS),
                                        ('branches', 'bid', 'bbalance', BRANCHES)), want):
        rows, _ = c.query(f'SELECT {key}, {col} FROM {table}')
        got = {int(k): int(v) for k, v in rows}
        if len(rows) != n or set(got) != set(range(1, n + 1)):
            bad.append(f'{table}: {len(rows)} rows, {len(got)} keys, expected 1..{n}')
        wrong = [(k, v, m.get(k, 0)) for k, v in got.items() if v != m.get(k, 0)]
        if wrong:
            bad.append(f'{table}: {len(wrong)} balances not their history rows\' sum, e.g. {key} {wrong[0][0]}: {wrong[0][1]} vs {wrong[0][2]}')
        sums.append(sum(got.values()))
    sums.append(sum(v[3] for v in hist.values()))
    if len(set(sums)) != 1:
        bad.append(f'balance sums differ (accounts, tellers, branches, history): {sums}')
    # Bulk batches: all or none.
    rows, _ = c.query('SELECT batch, count(*) FROM bulk GROUP BY batch')
    have = {int(b): int(n) for b, n in rows}
    with model.lock:
        for b, state in sorted(model.batches.items()):
            n, full = have.get(b, 0), model.batch_rows[b]
            if n not in (0, full):
                bad.append(f'bulk batch {b}: {n} of {full} rows (its {state} write half there)')
            elif state == 'all' and n != full:
                bad.append(f'bulk batch {b}: acknowledged, {n} of {full} rows')
            elif state == 'none' and n != 0:
                bad.append(f'bulk batch {b}: its DELETE acknowledged, {n} rows')
            else:
                model.batches[b] = 'all' if n else 'none'
        for b in have.keys() - model.batches.keys():
            bad.append(f'bulk batch {b}: {have[b]} rows no client wrote')
    # Unique keys, by a sequential scan.
    for table, cols in (('accounts', 'aid'), ('accounts', 'code'), ('history', 'txid'), ('bulk', 'batch, n')):
        n, d = c.query(f'SELECT count(*), count(DISTINCT ({cols})) FROM {table}')[0][0]
        if n != d:
            bad.append(f'{table} ({cols}): {n} rows, {d} distinct')
    # Indexes created in flight.
    rows, _ = c.query("SELECT c.relname, i.indisvalid, i.indisready FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname LIKE 'hx_r%'")
    there = {r[0]: r[1:] for r in rows}
    with model.lock:
        for name, state in model.indexes.items():
            if name in there and there[name] != ['t', 't']:
                bad.append(f'index {name}: valid {there[name][0]}, ready {there[name][1]}')
            if state == 'there' and name not in there:
                bad.append(f'index {name}: CREATE INDEX acknowledged, the index is gone')
            if state == 'gone' and name in there:
                bad.append(f'index {name}: dropped, the index is there')
            model.indexes[name] = 'there' if name in there else 'gone'
    c.query('RESET ALL')
    # Every index against a sequential scan: count and the md5 of the rows in the index's own
    # key order (a total order, so both paths agree; and the order the index gives, so the
    # planner, sequential scans off, reads that index rather than another one and a filter).
    probes = [('accounts', 'accounts_pkey', 'aid >= 0', "aid::text || ':' || abalance || ':' || code", 'aid'),
              ('accounts', 'accounts_code_key', "code >= ''", "aid::text || ':' || abalance || ':' || code", 'code'),
              ('accounts', 'accounts_bid', 'bid >= 0', "aid::text || ':' || abalance", 'bid, aid'),
              ('tellers', 'tellers_pkey', 'tid >= 0', "tid::text || ':' || tbalance", 'tid'),
              ('branches', 'branches_pkey', 'bid >= 0', "bid::text || ':' || bbalance", 'bid'),
              ('history', 'history_pkey', 'txid >= 0', "txid::text || ':' || aid || ':' || delta", 'txid'),
              ('history', 'history_aid', 'aid >= 0', "txid::text || ':' || aid || ':' || delta", 'aid, txid'),
              ('bulk', 'bulk_pkey', 'batch >= 0', "batch::text || ':' || n || ':' || payload", 'batch, n'),
              ('bulk', 'bulk_payload', "payload >= ''", "batch::text || ':' || n || ':' || payload", 'payload, batch, n')]
    with model.lock:
        probes += [('history', name, 'tid >= 0', "txid::text || ':' || tid", 'tid, txid') for name, s in model.indexes.items() if s == 'there']
    for table, index, cond, row, order in probes:
        q = f'SELECT count(*), md5(string_agg({row}, \',\' ORDER BY {order})) FROM {table} WHERE {cond}'
        c.query('RESET ALL; SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off')
        seq = c.query(q)[0][0]
        c.query('RESET ALL; SET enable_seqscan = off')
        plan = '\n'.join(r[0] for r in c.query('EXPLAIN ' + q)[0])
        if f' {index} ' not in plan + ' ' and f' {index}\n' not in plan + '\n':
            bad.append(f'{index}: the plan does not read it:\n{plan}')
            continue
        idx = c.query(q)[0][0]
        if seq != idx:
            bad.append(f'{index}: {idx} through the index, {seq} by a sequential scan')
    c.query('RESET ALL')
    # A created index, checked, is dropped (one on history (tid, txid) at a time; later checks
    # find the DROP kept).
    if not bad:
        with model.lock:
            there = [name for name, s in model.indexes.items() if s == 'there']
        for name in there:
            c.query(f'DROP INDEX {name}')
            with model.lock:
                model.indexes[name] = 'gone'
    c.close()
    if bad:
        raise Failure(f'{where}:\n  ' + '\n  '.join(bad[:30]) + (f'\n  ... {len(bad) - 30} more' if len(bad) > 30 else ''))
    with model.lock:
        return (f'{len(hist)} history rows (in flight at the kill: {model.resolved[0]} committed, {model.resolved[1]} not), '
                f'{sum(1 for s in model.batches.values() if s == "all")} '
                f'bulk batches, sums {sums[0]}')


def setup(port):
    c = Conn(port)
    c.query('CREATE DATABASE crash')
    c.close()
    c = Conn(port, 'crash')
    c.query('CREATE TABLE branches (bid int PRIMARY KEY, bbalance bigint NOT NULL); '
            'CREATE TABLE tellers (tid int PRIMARY KEY, bid int NOT NULL, tbalance bigint NOT NULL); '
            'CREATE TABLE accounts (aid int PRIMARY KEY, bid int NOT NULL, abalance bigint NOT NULL, code text NOT NULL UNIQUE, filler text); '
            'CREATE TABLE history (txid bigint PRIMARY KEY, tid int, bid int, aid int, delta int); '
            'CREATE TABLE bulk (batch int, n int, payload text, PRIMARY KEY (batch, n))')
    c.query(f'INSERT INTO branches SELECT g, 0 FROM generate_series(1, {BRANCHES}) g; '
            f'INSERT INTO tellers SELECT g, (g - 1) % {BRANCHES} + 1, 0 FROM generate_series(1, {TELLERS}) g')
    c.query('COPY accounts FROM STDIN', copy=[''.join(
        f'{a}\t{(a - 1) % BRANCHES + 1}\t0\tk{a * 7919 % 1000003:07d}-{a}\t{"f" * 60}\n'
        for a in range(lo, min(lo + 10000, ACCOUNTS + 1))).encode() for lo in range(1, ACCOUNTS + 1, 10000)])
    c.query('CREATE INDEX accounts_bid ON accounts (bid); CREATE INDEX history_aid ON history (aid); CREATE INDEX bulk_payload ON bulk (payload)')
    c.close()


def run_round(srv, model, rng, r, kind, batch0):
    """One round: writers, churn and the kind's action; the kill; nothing checked yet."""
    port = srv.port
    stop = threading.Event()
    ths = [threading.Thread(target=writer, args=(port, model, random.Random(rng.random()), w, stop, r), daemon=True) for w in range(WRITERS)]
    ths.append(threading.Thread(target=churn, args=(port, model, random.Random(rng.random()), batch0, stop), daemon=True))
    for t in ths:
        t.start()
    base = os.path.join(srv.data, 'd', 'base')
    what = ''
    act = None

    def action(sql, copy=None, mark=None):
        def go():
            try:
                c = Conn(port, 'crash')
                c.query(sql, copy=copy)
                if mark:
                    mark()
                c.close()
            except (Lost, PgError) as e:
                if isinstance(e, PgError) and not e.fatal:
                    model.errors.append(f'{kind}: {e}')
        t = threading.Thread(target=go, daemon=True)
        t.start()
        return t

    # Writes under way first: a seeded number of acknowledged transactions (at most 15 s: a
    # loaded machine still gets its kill).
    model.round_acked = 0
    warm = 100 + rng.randrange(900)
    until = time.monotonic() + 15
    while model.round_acked < warm and time.monotonic() < until:
        time.sleep(0.005)
    if kind == 'tpcb':
        d = 0.2 + rng.random() * 2.5
        time.sleep(d)
        what = f'{d + 0.3:.2f} s into the writes'
    elif kind == 'file':
        n = rng.randrange(1, 5)
        us = rng.randrange(0, 3000)
        hit = wait_files(base, n, time.monotonic() + 8)
        time.sleep(us / 1e6)
        what = f'{us} us after new file {n} appeared' if hit else f'8 s (fewer than {n} new files)'
    elif kind == 'create-index':
        name = f'hx_r{r}'
        with model.lock:
            model.indexes[name] = 'either'
        d = rng.random() * 0.6
        act = action(f'CREATE INDEX {name} ON history (tid, txid)', mark=lambda: model.indexes.__setitem__(name, 'there'))
        time.sleep(d)
        what = f'{d * 1000:.0f} ms into CREATE INDEX {name}'
    elif kind == 'copy':
        b = batch0 + 100_000
        model.batch_rows[b] = COPY_ROWS
        batch_mark(model, b, 'either')
        data = copy_rows(b)
        d = rng.random() * 1.5
        act = action('COPY bulk FROM STDIN', copy=data, mark=lambda: batch_mark(model, b, 'all'))
        time.sleep(d)
        what = f'{d * 1000:.0f} ms into COPY of batch {b} ({COPY_ROWS} rows)'
    elif kind in ('vacuum', 'checkpoint'):
        sql = 'VACUUM accounts' if kind == 'vacuum' else 'CHECKPOINT'
        us = rng.randrange(0, 2000)
        fallback = 0.5 + rng.random() * 4.5
        act = action(sql)
        hit = wait_files(base, 1, time.monotonic() + fallback)
        time.sleep(us / 1e6)
        what = f'{us} us after {sql} wrote its first file' if hit else f'{fallback * 1000:.0f} ms into {sql} (no new file yet)'
    elif kind == 'close':
        d = rng.random() * 0.15
        os.kill(srv.proc.pid, signal.SIGINT)
        time.sleep(d)
        what = f'{d * 1000:.0f} ms into a fast shutdown'
    alive = srv.proc.poll() is None
    if alive:
        srv.kill()
    else:
        srv.proc.wait()
        what += f' (the server had exited by itself, status {srv.proc.returncode})'
    what += f', {model.round_acked} transactions acknowledged in the round'
    stop.set()
    for t in ths + ([act] if act else []):
        t.join(30)
    return what


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('binary')
    ap.add_argument('--seed', type=int, default=None)
    ap.add_argument('--rounds', type=int, default=len(KINDS))
    ap.add_argument('--port', type=int, default=None, help='default: one from tools/ports.sh')
    ap.add_argument('--keep', default=None, help='keep the data directory here (default: only on a failure)')
    ap.add_argument('--set', action='append', default=[], metavar='NAME=VALUE',
                    help="the server's -c (e.g. synchronous_commit=off, under which a kill may lose "
                         'acknowledged commits: the check of them then fails, as it should)')
    a = ap.parse_args()
    binary = os.path.realpath(a.binary)
    seed = a.seed if a.seed is not None else int.from_bytes(os.urandom(4), 'little')
    rng = random.Random(seed)
    port, release = a.port, None
    if port is None:
        ports = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'ports.sh')
        port = int(subprocess.check_output([ports, 'take', str(os.getpid()), '1']).decode().strip())
        release = lambda: subprocess.run([ports, 'release', str(port)], stdout=subprocess.DEVNULL)
    data = a.keep or tempfile.mkdtemp(prefix='crash-binary-')
    os.makedirs(data, exist_ok=True)
    srv = Server(binary, data, port, os.environ.get('CRASH_CPUS'), a.set)
    model = Model()
    order = []
    while len(order) < a.rounds:
        k = KINDS[:]
        rng.shuffle(k)
        order += k
    print(f'crash-binary: {binary} ({hashlib.sha256(open(binary, "rb").read()).hexdigest()[:16]}), seed {seed}, '
          f'{a.rounds} rounds, port {port}, data {data}', flush=True)
    t0 = time.monotonic()
    ok = False
    r = 0
    try:
        srv.start()
        setup(port)
        for r in range(1, a.rounds + 1):
            kind = order[r - 1]
            what = run_round(srv, model, rng, r, kind, r * 1000)
            took = srv.start()
            state = check(port, model, f'seed {seed} round {r} ({kind}, killed {what})')
            print(f'round {r} {kind}: killed {what}; restarted in {took:.2f} s; ok: {state}', flush=True)
        r += 1
        rc = srv.stop()
        if rc != 0:
            raise Failure(f'a fast shutdown exited with {rc}:\n{srv.tail()}')
        took = srv.start()
        state = check(port, model, f'seed {seed}, after a clean shutdown')
        rc = srv.stop()
        print(f'clean shutdown: restarted in {took:.2f} s; ok: {state}', flush=True)
        if model.errors:
            print(f'crash-binary: {len(model.errors)} statement errors (not failures), the first: {model.errors[0]}', flush=True)
        ok = True
    except Failure as e:
        print(f'crash-binary: FAILED, seed {seed}, round {r}: {e}', flush=True)
        if model.errors:
            print('statement errors before it:\n  ' + '\n  '.join(model.errors[:10]), flush=True)
    finally:
        if srv.proc and srv.proc.poll() is None:
            srv.kill()
        if release:
            release()
    print(f'crash-binary: {"ok" if ok else "FAILED"} in {time.monotonic() - t0:.0f} s', flush=True)
    if ok and not a.keep:
        shutil.rmtree(data, ignore_errors=True)
    elif not ok:
        print(f'crash-binary: kept {data} (server.log in it); repeat with --seed {seed}', flush=True)
    sys.exit(0 if ok else 1)


if __name__ == '__main__':
    main()
