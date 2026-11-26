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

Power cuts (`--power drop` or `--power keep`). A SIGKILL keeps what the kernel has in its page
cache; a power cut loses everything not fsynced. With --power the server runs under
tools/powercut/powercut.so (LD_PRELOAD, compiled on the spot with cc; no root, no container):
it records every write, size change, fsync and name change under the data directory, and after
each kill tools/powercut/rebuild.py replaces the directory with what a power cut would have left
(its docstring has the model: POSIX's, a file's bytes as of its last fsync, a directory's
entries as of its last fsync; `keep` also keeps a seeded prefix of the unsynced writes, the next
one maybe torn, and a seeded prefix of the unsynced name changes). The schema is loaded before
the first watched start (a clean shutdown: that state counts as durable). The rounds then cycle
through three more kinds:
  ack           0-300 us after a writer's COMMIT was acknowledged (it must be there);
  ddl           0-3 ms after the nth of a DDL sequence was sent or acknowledged: CREATE TABLE, INSERT, ADD
                COLUMN, CREATE INDEX, ALTER SYSTEM, a physical replication slot, a replication
                origin, DROP TABLE of the last round's table; each acknowledged step must be
                there (ALTER SYSTEM, the slot and the origin under synchronous_commit=off too:
                roll writes them as files, not in a transaction);
  prepare       0-2 ms after the nth two-phase message was sent or acknowledged (PREPARE TRANSACTION, COMMIT /
                ROLLBACK PREPARED of TPC-B transactions on a connection of their own): an
                acknowledged PREPARE is in pg_prepared_xacts after the restart (then committed
                by the check), an acknowledged COMMIT PREPARED is in history, under
                synchronous_commit=off too (both flush in PostgreSQL);
and the end's clean shutdown is followed by a cut as well. With `--set synchronous_commit=off`
the check allows acknowledged transactions (and bulk batches, indexes, DDL steps) to be lost,
but only as a suffix: one may be missing only when nothing that survived was sent after it was
acknowledged; any other loss, and every torn or half state, is still a failure. A failure of the
layer itself (an operation it can't model, a record that doesn't match the directory) is
reported as a tool error, not as the engine's. A failing seed repeats with the same --seed and
--power (and --set): the kill points, the cuts' choices and the order of kinds.

    crates/pg-engine/tools/crash-binary.py <dir>/roll-engine --power drop --seed 1
    crates/pg-engine/tools/crash-binary.py <dir>/roll-engine --power keep --seed 2 --set synchronous_commit=off
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
POWER_KINDS = KINDS + ['ack', 'ddl', 'prepare']
SYS_GUC = 'log_min_duration_statement'  # ALTER SYSTEM's: a value of 100,000+ ms changes nothing here
DDL_STEPS = 5  # levels of a round's ddl_r<r> table: 1 made, 2 a row, 3 column x, 4 an index, 5 dropped
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
        self.env = None  # extra environment (the power-cut layer)

    def start(self):
        cmd = [self.binary, '--data', os.path.join(self.data, 'd'), '--listen', f'127.0.0.1:{self.port}']
        for kv in self.settings:
            cmd += ['-c', kv]
        if self.cpus:
            cmd = ['taskset', '-c', self.cpus] + cmd  # (taskset execs: the pid is the server's)
        self.log.write(f'--- start {time.strftime("%H:%M:%S")}\n'.encode())
        self.log.flush()
        t0 = time.monotonic()
        env = dict(os.environ, **self.env) if self.env else None
        self.proc = subprocess.Popen(cmd, stdout=self.log, stderr=self.log, stdin=subprocess.DEVNULL, env=env)
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


class ToolFailure(Failure):
    """The power-cut layer could not model the run: the tool's failure, not the engine's."""


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
        # When each write was sent and acknowledged (time.monotonic), by key: ('t', txid),
        # ('b+', batch) its INSERT / COPY, ('b-', batch) its DELETE, ('i', index), ('d', r, level).
        self.sent, self.ackt = {}, {}
        self.sync_off = False  # synchronous_commit=off: acknowledged writes may be lost, as a suffix
        self.lost_async = 0  # acknowledged and lost so, over all rounds
        self.ack_evt = threading.Event()  # set at each acknowledged COMMIT
        self.prep = {}  # gid -> [txid, 'preparing' | 'prepared' | 'committing' | 'committed' | 'rolling' | 'rolledback']
        self.ddl = {}  # round -> [acked level, sent level] of its ddl_r<round> table
        self.sysval = [None, None]  # ALTER SYSTEM's value: acknowledged, sent
        self.slots = {}  # name -> 'there', 'gone' or 'either'
        self.origins = {}  # replication origins, the same
        self.step_evt = threading.Event()  # set when the round's ddl / prepare worker sent the step asked for
        self.step_want = 0
        self.steps = 0

    def mark(self, key, sent=False):
        (self.sent if sent else self.ackt)[key] = time.monotonic()

    def step(self):
        """A ddl / prepare worker sent its next statement, or had one acknowledged (two events
        a statement: odd ones sent, even ones acknowledged)."""
        self.steps += 1
        if self.steps >= self.step_want:
            self.step_evt.set()

    def lost_ok(self, key, alive):
        """An acknowledged write may be missing: synchronous_commit=off and acknowledged after the
        last write that survived was sent (so no surviving write is known to follow it)."""
        return self.sync_off and self.ackt.get(key, 0) > alive


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
            model.mark(('t', txid), sent=True)
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
                    model.mark(('t', txid))
                    model.ack_evt.set()
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
        model.mark(('b+', b), sent=True)
        try:
            c.query(f"INSERT INTO bulk SELECT {b}, g, md5(({b}::bigint * 1000003 + g)::text) || repeat('c', 40) "
                    f'FROM generate_series(1, {CHURN_ROWS}) g')
        except (Lost, PgError):
            return
        batch_mark(model, b, 'all')
        model.mark(('b+', b))
        old = b - 2
        if old >= first and model.batches.get(old) == 'all' and rng.randrange(2) == 0:
            batch_mark(model, old, 'either-delete')
            model.mark(('b-', old), sent=True)
            try:
                c.query(f'DELETE FROM bulk WHERE batch = {old}')
            except (Lost, PgError):
                return
            batch_mark(model, old, 'none')
            model.mark(('b-', old))
        b += 1
    c.close()


def twopc(port, model, rng, r, stop):
    """TPC-B transactions in two phases: PREPARE TRANSACTION, then (as its own message)
    COMMIT PREPARED, or one in 8 ROLLBACK PREPARED."""
    try:
        c = Conn(port, 'crash')
    except Lost:
        return
    seq = 0
    while not stop.is_set():
        seq += 1
        txid = (r << 32) | (0xff << 24) | seq
        gid = f'p{txid:x}'
        aid, tid = rng.randrange(1, ACCOUNTS + 1), rng.randrange(1, TELLERS + 1)
        bid, delta = (tid - 1) % BRANCHES + 1, rng.randrange(-5000, 5001)
        with model.lock:
            model.txns[txid] = (aid, tid, bid, delta)
            model.prep[gid] = [txid, 'preparing']
        model.step()
        try:
            c.query(f'BEGIN; UPDATE accounts SET abalance = abalance + {delta} WHERE aid = {aid}; '
                    f'UPDATE tellers SET tbalance = tbalance + {delta} WHERE tid = {tid}; '
                    f'UPDATE branches SET bbalance = bbalance + {delta} WHERE bid = {bid}; '
                    f'INSERT INTO history (txid, tid, bid, aid, delta) VALUES ({txid}, {tid}, {bid}, {aid}, {delta}); '
                    f"PREPARE TRANSACTION '{gid}'")
        except Lost:
            return
        except PgError as e:
            if e.fatal:
                return
            with model.lock:
                model.prep[gid][1] = 'rolledback'
                model.never.add(txid)
                model.errors.append(f'prepare: {e}')
            try:
                c.query('ROLLBACK')
            except (Lost, PgError):
                return
            continue
        with model.lock:
            model.prep[gid][1] = 'prepared'
        model.step()
        commit = rng.randrange(8) != 0
        with model.lock:
            model.prep[gid][1] = 'committing' if commit else 'rolling'
        model.step()
        try:
            c.query(f"{'COMMIT' if commit else 'ROLLBACK'} PREPARED '{gid}'")
        except Lost:
            return
        except PgError as e:
            if e.fatal:
                return
            with model.lock:
                model.errors.append(f'prepare: {e}')
            continue
        with model.lock:
            model.prep[gid][1] = 'committed' if commit else 'rolledback'
            (model.acked if commit else model.never).add(txid)
        model.step()
    c.close()


def ddl_worker(port, model, rng, r):
    """The round's DDL sequence (DDL_STEPS levels of ddl_r<r>, ALTER SYSTEM, a slot), each step
    one message; acknowledged steps are recorded."""
    try:
        c = Conn(port, 'crash')
    except Lost:
        return
    t = f'ddl_r{r}'
    with model.lock:
        prev = [k for k, (a, s) in model.ddl.items() if 1 <= a < DDL_STEPS]
        model.ddl[r] = [0, 0]
    steps = [(('d', r, 1), f'CREATE TABLE {t} (id int PRIMARY KEY, v text)'),
             (('d', r, 2), f"INSERT INTO {t} VALUES (1, 'v{r}')"),
             (('d', r, 3), f'ALTER TABLE {t} ADD COLUMN x int DEFAULT {r}'),
             (('d', r, 4), f'CREATE INDEX {t}_v ON {t} (v)'),
             (('sys',), f'ALTER SYSTEM SET {SYS_GUC} = {100000 + r}'),
             (('slot', f's_r{r}'), f"SELECT pg_create_physical_replication_slot('s_r{r}', true)"),
             (('origin', f'o_r{r}'), f"SELECT pg_replication_origin_create('o_r{r}')")]
    steps += [(('d', p, DDL_STEPS), f'DROP TABLE ddl_r{p}') for p in prev]
    for key, sql in steps:
        with model.lock:
            if key[0] == 'd':
                model.ddl[key[1]][1] = key[2]
            elif key[0] == 'sys':
                model.sysval[1] = 100000 + r
            else:
                (model.slots if key[0] == 'slot' else model.origins)[key[1]] = 'either'
            model.mark(key, sent=True)
        model.step()
        try:
            c.query(sql)
        except Lost:
            return
        except PgError as e:
            with model.lock:
                model.errors.append(f'ddl: {sql}: {e}')
            if e.fatal:
                return
            break
        with model.lock:
            if key[0] == 'd':
                model.ddl[key[1]][0] = key[2]
            elif key[0] == 'sys':
                model.sysval[0] = 100000 + r
            else:
                (model.slots if key[0] == 'slot' else model.origins)[key[1]] = 'there'
            model.mark(key)
        model.step()
        time.sleep(rng.random() * 0.02)
    model.step_evt.set()  # all of it acknowledged: a kill after the whole sequence
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
    c.query('SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off')
    # Two-phase transactions first: what was prepared is checked, then committed (their row
    # locks would block the checks' DROP INDEX and the next round's writers).
    if model.prep:
        txids = {int(r[0]) for r in c.query('SELECT txid FROM history WHERE txid >= 0')[0]}
        prepared = {r[0] for r in c.query('SELECT gid FROM pg_prepared_xacts')[0]}
        with model.lock:
            for gid, (txid, st) in sorted(model.prep.items()):
                inh, inp = txid in txids, gid in prepared
                ok = {'preparing': not inh, 'prepared': inp and not inh, 'committing': inh != inp,
                      'committed': inh and not inp, 'rolling': not inh, 'rolledback': not inh and not inp}[st]
                if not ok:
                    bad.append(f'two-phase {gid} ({st}): {"in" if inh else "not in"} history, '
                               f'{"in" if inp else "not in"} pg_prepared_xacts')
                if inh:
                    model.acked.add(txid)
                elif not inp:
                    model.never.add(txid)
        for gid in sorted(prepared):
            c.query(f"COMMIT PREPARED '{gid}'")
            with model.lock:
                if gid in model.prep:
                    model.acked.add(model.prep[gid][0])
        with model.lock:
            model.prep.clear()
    # History, by a sequential scan.
    rows, _ = c.query('SELECT txid, aid, tid, bid, delta FROM history')
    hist = {}
    for r in rows:
        t = int(r[0])
        if t in hist:
            bad.append(f'history: txid {t} twice')
        hist[t] = tuple(int(x) for x in r[1:])
    rows, _ = c.query('SELECT batch, count(*) FROM bulk GROUP BY batch')
    have = {int(b): int(n) for b, n in rows}
    with model.lock:
        # The last write that survived was sent at `alive`: under synchronous_commit=off only
        # writes acknowledged after that may be lost.
        alive = max([model.sent.get(('t', t), 0) for t in hist] + [model.sent.get(('b+', b), 0) for b in have] + [0])
        for t in model.acked - hist.keys():
            if model.lost_ok(('t', t), alive):
                model.acked.discard(t)
                model.never.add(t)
                model.lost_async += 1
                continue
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
    with model.lock:
        for b, state in sorted(model.batches.items()):
            n, full = have.get(b, 0), model.batch_rows[b]
            if n not in (0, full):
                bad.append(f'bulk batch {b}: {n} of {full} rows (its {state} write half there)')
            elif state == 'all' and n != full and not model.lost_ok(('b+', b), alive):
                bad.append(f'bulk batch {b}: acknowledged, {n} of {full} rows')
            elif state == 'none' and n != 0 and not model.lost_ok(('b-', b), alive):
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
            if state == 'there' and name not in there and not model.lost_ok(('i', name), alive):
                bad.append(f'index {name}: CREATE INDEX acknowledged, the index is gone')
            if state == 'gone' and name in there and not model.lost_ok(('i-', name), alive):
                bad.append(f'index {name}: dropped, the index is there')
            model.indexes[name] = 'there' if name in there else 'gone'
    bad += check_ddl(c, model, alive)
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
                model.mark(('i-', name))
        with model.lock:
            slots = [name for name, s in model.slots.items() if s == 'there']
        for name in slots:
            c.query(f"SELECT pg_drop_replication_slot('{name}')")
            with model.lock:
                model.slots[name] = 'gone'
        with model.lock:
            origins = [name for name, s in model.origins.items() if s == 'there']
        for name in origins:
            c.query(f"SELECT pg_replication_origin_drop('{name}')")
            with model.lock:
                model.origins[name] = 'gone'
    c.close()
    if bad:
        raise Failure(f'{where}:\n  ' + '\n  '.join(bad[:30]) + (f'\n  ... {len(bad) - 30} more' if len(bad) > 30 else ''))
    with model.lock:
        return (f'{len(hist)} history rows (in flight at the kill: {model.resolved[0]} committed, {model.resolved[1]} not), '
                f'{sum(1 for s in model.batches.values() if s == "all")} '
                f'bulk batches, sums {sums[0]}')


def check_ddl(c, model, alive):
    """The ddl rounds' tables, ALTER SYSTEM's value and the slots against what was acknowledged."""
    bad = []
    with model.lock:
        tables, sysval, slots = {r: list(v) for r, v in model.ddl.items()}, list(model.sysval), dict(model.slots)
    if not tables and sysval[1] is None and not slots and not model.origins:
        return bad
    present = {r[0] for r in c.query("SELECT relname FROM pg_class WHERE relname LIKE 'ddl\\_r%' AND relkind = 'r'")[0]}
    for r, (acked, sent) in sorted(tables.items()):
        t = f'ddl_r{r}'
        if t in present:
            n = int(c.one(f'SELECT count(*) FROM {t}'))
            hasx = c.one(f"SELECT count(*) FROM pg_attribute WHERE attrelid = '{t}'::regclass AND attname = 'x' AND NOT attisdropped") == '1'
            hasi = c.one(f"SELECT count(*) FROM pg_class WHERE relname = '{t}_v'") == '1'
            eff = [True, n == 1, hasx, hasi]
            k = sum(eff)
            if eff != [True] * k + [False] * (4 - k) or n > 1:
                bad.append(f'{t}: a state no prefix of its DDL makes: {n} rows, column x {hasx}, index {hasi}')
                continue
            if hasx and n == 1 and c.one(f'SELECT x FROM {t}') != str(r):
                bad.append(f'{t}: its row\'s x is {c.one(f"SELECT x FROM {t}")}, the default was {r}')
            cands = [k]
        else:
            cands = [0, DDL_STEPS]
        ok = [lv for lv in cands if acked <= lv <= sent]
        if not ok:
            ok = [lv for lv in cands if lv < acked and model.lost_ok(('d', r, lv + 1), alive)]
            if ok:
                model.lost_async += 1
        if not ok:
            bad.append(f'{t}: at level {" or ".join(map(str, cands))} (1 made, 2 its row, 3 column x, 4 its index, '
                       f'{DDL_STEPS} dropped), acknowledged {acked}, sent {sent}')
            continue
        with model.lock:
            model.ddl[r] = [ok[0], ok[0]]
    if sysval[1] is not None:
        got = c.one(f"SELECT setting FROM pg_settings WHERE name = '{SYS_GUC}'")
        allowed = {str(v) for v in sysval if v is not None} | ({'-1'} if sysval[0] is None else set())
        if got not in allowed:
            bad.append(f'ALTER SYSTEM SET {SYS_GUC}: {got} after the restart, acknowledged {sysval[0]}, sent {sysval[1]}')
        with model.lock:
            model.sysval = [int(got), int(got)] if got != '-1' else [None, None]
    for what, objs, q in (('replication slot', model.slots, "SELECT slot_name FROM pg_replication_slots WHERE slot_name LIKE 's\\_r%'"),
                          ('replication origin', model.origins, "SELECT roname FROM pg_replication_origin WHERE roname LIKE 'o\\_r%'")):
        with model.lock:
            states = dict(objs)
        if not states:
            continue
        there = {r[0] for r in c.query(q)[0]}
        for name, state in sorted(states.items()):
            if state == 'there' and name not in there:
                bad.append(f'{what} {name}: its creation acknowledged, gone after the restart')
            if state == 'gone' and name in there:
                bad.append(f'{what} {name}: dropped, there after the restart')
            with model.lock:
                objs[name] = 'there' if name in there else 'gone'
    return bad


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
        act = action(f'CREATE INDEX {name} ON history (tid, txid)',
                     mark=lambda: (model.indexes.__setitem__(name, 'there'), model.mark(('i', name))))
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
    elif kind == 'ack':
        us = rng.randrange(0, 300)
        model.ack_evt.clear()
        hit = model.ack_evt.wait(10)
        time.sleep(us / 1e6)
        what = f'{us} us after a COMMIT was acknowledged' if hit else '10 s (no COMMIT acknowledged)'
    elif kind in ('ddl', 'prepare'):
        n = rng.randrange(1, 19 if kind == 'ddl' else 41)
        us = rng.randrange(0, 3000 if kind == 'ddl' else 2000)
        model.steps, model.step_want = 0, n
        model.step_evt.clear()
        if kind == 'ddl':
            act = threading.Thread(target=ddl_worker, args=(port, model, random.Random(rng.random()), r), daemon=True)
        else:
            act = threading.Thread(target=twopc, args=(port, model, random.Random(rng.random()), r, stop), daemon=True)
        act.start()
        hit = model.step_evt.wait(15)
        time.sleep(us / 1e6)
        what = (f'{us} us after {kind} statement {(n + 1) // 2} was {"sent" if n % 2 else "acknowledged"}' if hit and model.steps >= n else
                f'{us} us after all {model.steps // 2} {kind} statements were acknowledged' if hit else
                f'15 s ({model.steps} of {n} {kind} events)')
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
    ap.add_argument('--rounds', type=int, default=None, help=f'default: {len(KINDS)}, {len(POWER_KINDS)} with --power')
    ap.add_argument('--port', type=int, default=None, help='default: one from tools/ports.sh')
    ap.add_argument('--keep', default=None, help='keep the data directory here (default: only on a failure)')
    ap.add_argument('--power', choices=('drop', 'keep'), default=None,
                    help='cut the power at each kill (tools/powercut): drop every unsynced change, or keep a seeded part')
    ap.add_argument('--kinds', default=None, help='comma-separated: only these kinds of rounds (default all)')
    ap.add_argument('--set', action='append', default=[], metavar='NAME=VALUE',
                    help="the server's -c (synchronous_commit=off: a power cut may lose acknowledged commits, "
                         'as a suffix; a SIGKILL may not)')
    a = ap.parse_args()
    binary = os.path.realpath(a.binary)
    seed = a.seed if a.seed is not None else int.from_bytes(os.urandom(4), 'little')
    rng = random.Random(seed)
    kinds = POWER_KINDS if a.power else KINDS
    if a.kinds:
        kinds = a.kinds.split(',')
        if set(kinds) - set(POWER_KINDS if a.power else KINDS):
            ap.error(f'--kinds: one of {",".join(POWER_KINDS if a.power else KINDS)}')
    rounds = a.rounds if a.rounds is not None else len(kinds)
    port, release = a.port, None
    if port is None:
        ports = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'ports.sh')
        port = int(subprocess.check_output([ports, 'take', str(os.getpid()), '1']).decode().strip())
        release = lambda: subprocess.run([ports, 'release', str(port)], stdout=subprocess.DEVNULL)
    data = a.keep or tempfile.mkdtemp(prefix='crash-binary-')
    os.makedirs(data, exist_ok=True)
    data = os.path.realpath(data)
    settings = list(a.set)
    if a.power:
        settings.insert(0, 'max_prepared_transactions=16')
    srv = Server(binary, data, port, os.environ.get('CRASH_CPUS'), settings)
    model = Model()
    model.sync_off = any(kv.replace(' ', '').lower() in ('synchronous_commit=off', 'synchronous_commit=local')
                         for kv in a.set) and bool(a.power)
    pc = None
    if a.power:
        sys.dont_write_bytecode = True
        sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), 'powercut'))
        import rebuild
        os.makedirs(os.path.join(data, 'pc'), exist_ok=True)
        pc = rebuild.PowerCut(os.path.join(data, 'd'), os.path.join(data, 'pc'), rebuild.build_lib(os.path.join(data, 'pc')), binary)
    order = []
    while len(order) < rounds:
        k = kinds[:]
        rng.shuffle(k)
        order += k
    replay = f'--seed {seed}' + (f' --rounds {rounds}' if a.rounds is not None else '') + \
        (f' --kinds {a.kinds}' if a.kinds else '') + \
        (f' --power {a.power}' if a.power else '') + ''.join(f" --set '{kv}'" for kv in a.set)
    print(f'crash-binary: {binary} ({hashlib.sha256(open(binary, "rb").read()).hexdigest()[:16]}), seed {seed}, '
          f'{rounds} rounds, {"power cuts (" + a.power + ")" if a.power else "kills"}, port {port}, data {data}', flush=True)
    t0 = time.monotonic()
    ok = False
    r = 0

    def power_cut(what):
        """After the process is gone: the data directory as the power cut leaves it."""
        if not pc:
            return ''
        cut_seed = rng.randrange(1 << 30)
        try:
            st = pc.cut(a.power, cut_seed, keep_old=os.path.join(data, 'd.precut'))
        except rebuild.ToolError as e:
            raise ToolFailure(f'{what}: the power-cut layer: {e}')
        return (f'; cut ({st["records"]} records): {st["writes_dropped"]} writes to {st["files_cut"]} files dropped '
                f'({st["bytes_dropped"]} bytes{", " + str(st["torn"]) + " torn" if st["torn"] else ""}), '
                f'{st["entries_dropped"]} entries undone, {st["entries_back"]} back'
                + (f' (files cut: {", ".join(st["cut_files"][:4])}{" ..." if len(st["cut_files"]) > 4 else ""})' if st['cut_files'] else '')
                + (f', 1 unrecorded: {st["unmatched"][0]}' if st['unmatched'] else ''))

    def start():
        if pc:
            srv.env = pc.env()
        return srv.start()

    tool_error = False
    try:
        srv.start()
        setup(port)
        if pc:
            # The schema, loaded unwatched and shut down cleanly: durable from here on.
            rc = srv.stop()
            if rc != 0:
                raise Failure(f'a fast shutdown exited with {rc}:\n{srv.tail()}')
            start()
        for r in range(1, rounds + 1):
            kind = order[r - 1]
            what = run_round(srv, model, rng, r, kind, r * 1000)
            cut = power_cut(f'round {r}')
            took = start()
            state = check(port, model, f'seed {seed} round {r} ({kind}, killed {what}{cut})')
            print(f'round {r} {kind}: killed {what}{cut}; restarted in {took:.2f} s; ok: {state}', flush=True)
        r += 1
        rc = srv.stop()
        if rc != 0:
            raise Failure(f'a fast shutdown exited with {rc}:\n{srv.tail()}')
        cut = power_cut('after a clean shutdown')
        took = start()
        state = check(port, model, f'seed {seed}, after a clean shutdown{cut}')
        rc = srv.stop()
        print(f'clean shutdown{cut}: restarted in {took:.2f} s; ok: {state}', flush=True)
        if model.lost_async:
            print(f'crash-binary: {model.lost_async} acknowledged writes lost under synchronous_commit=off, each after '
                  'every write that survived (allowed)', flush=True)
        if model.errors:
            print(f'crash-binary: {len(model.errors)} statement errors (not failures), the first: {model.errors[0]}', flush=True)
        ok = True
    except ToolFailure as e:
        tool_error = True
        print(f'crash-binary: TOOL ERROR (not the engine\'s), seed {seed}, round {r}: {e}', flush=True)
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
        print(f'crash-binary: kept {data} (server.log in it{", d.precut: the directory before the last cut, pc: its record" if pc else ""}); '
              f'repeat with {replay}', flush=True)
    sys.exit(0 if ok else (2 if tool_error else 1))


if __name__ == '__main__':
    main()
