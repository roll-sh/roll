# redis-engine

The Redis-compatible data engine behind roll's serverless Redis. It is pure and synchronous: one
`Engine` is one shard (16 logical databases). `redis-server` wraps each shard in a mutex and adds
networking, tenancy, persistence and replication.

The goal is **byte-for-byte reply compatibility with Redis 8**. Every reply, error message,
encoding name (`OBJECT ENCODING`), keyspace notification and edge case should match. When in doubt,
ask a real Redis (see Testing).

## Layout

| file | what |
| --- | --- |
| `engine.rs` | `Engine`: dispatch (arity, ACL, MULTI queueing, OOM, replica read-only), stats, slowlog, WATCH, blocking, eviction, active expiry, effects |
| `cx.rs` | `Cx`, the handler context, and `KeyMut`, the write guard |
| `commands/*.rs` | one module per family, each exports `pub static COMMANDS: &[Command]` |
| `commands/mod.rs` | `Command`, flags, `cmd!` / `container!` macros, `Error`, arg helpers |
| `types/*.rs` | aggregate types (`List`, `Hash`, `Set`, `ZSet`, `Stream`) |
| `dict.rs` | chained hash table with incremental rehash and Redis-exact SCAN cursors |
| `pack.rs` | `Pack`, a listpack-like compact encoding for small aggregates |
| `value.rs` | `Value`, `Entry`, `Kind`, `Str` (`SmallVec<[u8; 22]>`) |
| `hub.rs` | pub/sub and MONITOR (`Sink` is how out-of-band data reaches a client) |
| `acl/` | users, selectors, permission bitmaps (`index.rs`; `spec.rs` is generated from Redis' commands.def), ACL LOG; `commands/acl.rs` is the `ACL` command |
| `config.rs` | `CONFIG GET/SET` parameters |
| `notify.rs` | keyspace notification classes |
| `decimal.rs` | INCRBYFLOAT arithmetic (quad-precision behaviour of Redis on arm64) |

## Writing a command

```rust
pub static COMMANDS: &[Command] = &[
    //    name     arity  flags                     first last step handler group
    cmd!("lpush", -3, WRITE | DENYOOM | FAST, 1, 1, 1, lpush, List),
    // movable keys: a function returning key argument indexes
    cmd!("lmpop", -4, WRITE, keys = lmpop_keys, lmpop, List),
    // containers dispatch on args[1] to `subs`
    container!("xinfo", 0, Stream, XINFO),
];

pub fn lpush(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut e = cx.get_or_create(&args[1], Kind::List)?;  // WRONGTYPE handled
    for v in &args[2..] { e.list().push_front(v); }
    let len = e.list().len();
    e.notify(notify::LIST, "lpush");                       // before the guard drops
    Ok(Reply::Int(len as i64))
}
```

- `args[0]` is the command name. Arity is checked before the handler runs; arity errors for
  subcommands use `container|sub`.
- Return `Err(Error)` for error replies. Shared errors: `WRONGTYPE`, `NOT_INT`, `NOT_FLOAT`,
  `SYNTAX`, `NO_KEY`, `OVERFLOW`, `OUT_OF_RANGE`; helpers `int()`, `float()`, `eq()` (case-insensitive
  option match), `range()`, `timeout_secs()`.
- Validate **all** arguments before modifying anything, like Redis does.

### Key access (`Cx`)

| call | use |
| --- | --- |
| `cx.get(key)` / `cx.get_list(key)?` … | read (lazy expiry, LRU touch, hit/miss stats); typed getters return WRONGTYPE |
| `cx.peek(key)` | read without touching LRU or stats |
| `cx.check_type(key, Kind)?` | `Ok(false)` if missing, WRONGTYPE if different |
| `cx.get_mut(key)` / `cx.get_mut_typed(key, Kind)?` | `KeyMut` for an existing key |
| `cx.get_or_create(key, Kind)?` | `KeyMut`, creating an empty value (emits `new`) |
| `cx.set(key, Value, keep_ttl)` | overwrite (removes TTL unless `keep_ttl`) |
| `cx.delete(key)` / `cx.take(key)` | delete / remove and return the entry |
| `cx.set_expire(key, at_ms)` / `cx.expire_of(key)` | TTLs (absolute Unix ms, 0 = none) |
| `cx.notify(class, event, key)` | keyspace notification |

**`KeyMut` does the bookkeeping on drop**: memory accounting (each type's `mem()` must be O(1)
and maintained incrementally), marking the key modified (WATCH, blocked clients, propagation), and
deleting aggregates that became empty (with a `del` notification, after your own events, so call
`e.notify(..)` before the guard drops). Call `e.unchanged()` when the command turned out to be a
no-op (e.g. SADD of existing members) so it isn't propagated.

### Propagation (replication and durability)

A command that changed data is propagated **as is** unless the handler rewrites it. Anything
non-deterministic must be rewritten with `cx.propagate_as(args)`:

- relative TTLs → absolute (`SET … PXAT`, `PEXPIREAT`);
- random choices → the concrete effect (`SPOP` → `SREM key members…`);
- `INCRBYFLOAT` → `SET key value KEEPTTL`;
- blocking pops when served → the non-blocking form (`BLPOP` → `LPOP key`, `BLMOVE` → `LMOVE`);
- `XADD *` → the generated ID; `XREADGROUP`/`XCLAIM` → `XCLAIM … JUSTID`-style deterministic forms
  (match what Redis propagates).

Replicas replay effects with `Mode::Replay` (no expiry checks, no rewriting). `cx.no_propagate()`
skips the command itself (EXEC, EVAL propagate their inner effects instead).

### Blocking commands

When there is nothing to return, call `return Ok(cx.block_on(keys, timeout_ms, timeout_reply))`.
Inside MULTI or scripts it returns `timeout_reply` immediately. When a watched key changes, the
engine **re-runs the whole command** for the blocked client (`Mode::Unblock`); if it blocks again
it keeps its place in the queue. So a blocking handler is just "try; if empty, `block_on`".

### Types

Each type in `types/` owns its encodings and reports Redis' `OBJECT ENCODING` names with Redis'
thresholds (from `cx.cfg()`: `hash-max-listpack-entries`, `set-max-intset-entries`, …). Use `Pack`
for small encodings and convert when limits are exceeded (Redis never converts back, except where
it does). `mem()` must be O(1).

## Testing

1. **Unit tests** next to the code (`cargo test -p redis-engine`). Use `Engine::standalone()` and
   `Client::new(1)`; call `engine.call(&mut client, &args)`.
2. **Differential tests against real Redis**: scripts in `crates/redis-server/tests/compat/*.txt`
   run against `redis:8-alpine` in Docker and against roll; every reply must be identical. See the
   syntax at the top of `crates/redis-server/tests/compat.rs`.

   ```sh
   ROLL_COMPAT_FILTER=lists cargo test -p redis-server --test compat   # one file
   cargo test -p redis-server --test compat                            # all
   ```

   Each run starts its own throwaway container. To poke at real Redis by hand, start your own
   (`docker run -d --rm --name <unique-name> redis:8-alpine`, then `docker exec <name> redis-cli …`).

`docs/redis-8.10-commands.txt` is the full command list of the reference server.
