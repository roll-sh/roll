//! Scripts that would take a Rust process down — deep nesting, huge allocations, endless loops,
//! panics in the host — fail the way Elasticsearch fails them (a `script_exception` caused by a
//! `stack_overflow_error`, an `out_of_memory_error`, or the host's cancellation) and leave the
//! engine usable.

use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Lang, MemoryRefused, NoHost, Settings, Value};
use serde_json::{json, Value as Json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The allocator wrapper a host installs to keep executions' memory accounts, over one that
/// records this thread's peak.
#[global_allocator]
static ALLOC: search_painless::Counting<Peak> = search_painless::Counting(Peak);

struct Peak;

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn moved(delta: i64) {
    let live = LIVE.get() + delta;
    LIVE.set(live);
    PEAK.set(PEAK.get().max(live));
}

// SAFETY: forwards to the system allocator; the counters are const thread-locals without
// destructors.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        moved(l.size() as i64);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        moved(-(l.size() as i64));
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        moved(n as i64 - l.size() as i64);
        unsafe { System.realloc(p, l, n) }
    }
}

/// Runs `f`, and how far this thread's live bytes rose above where they started.
fn peak_growth<T>(f: impl FnOnce() -> T) -> (T, i64) {
    let start = LIVE.get();
    PEAK.set(start);
    let r = f();
    (r, PEAK.get() - start)
}

/// An engine whose executions may hold `bytes`.
fn limited(bytes: u64) -> Engine {
    Engine::new(Settings { max_compilations_rate: None, max_execution_bytes: bytes, ..Settings::default() })
}

/// An engine whose objects may not exceed 1 MiB, so the size checks trip without the test
/// allocating much.
fn engine() -> Engine {
    Engine::new(Settings { max_compilations_rate: None, max_object_bytes: 1 << 20, ..Settings::default() })
}

fn run(engine: &Engine, src: &str) -> Result<String, Json> {
    let script = engine.compile_stored(Lang::Painless, src, Context::PainlessTest, &[]).map_err(|e| e.to_response())?;
    let search_painless::Compiled::Painless(script) = script else { unreachable!() };
    let mut inst = script.instance();
    inst.execute(&mut NoHost).map(|v| v.to_java_string()).map_err(|e| e.to_response())
}

fn cause(err: &Json) -> (&str, Option<&str>) {
    let c = &err["error"]["caused_by"];
    (c["type"].as_str().unwrap_or(""), c["reason"].as_str())
}

/// Runs `f` on a thread with Rust's default 2 MiB stack, like a host's worker thread.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().unwrap()
}

#[test]
fn deeply_nested_sources_fail_to_compile_like_elasticsearch() {
    on_small_stack(|| {
        let e = engine();
        // what Elasticsearch answers for `return ((((…1…))))` nested beyond its stack
        let src = format!("return {}1{}", "(".repeat(40_000), ")".repeat(40_000));
        let err = run(&e, &src).unwrap_err();
        let want = json!({
            "error": {
                "root_cause": [{"type": "stack_overflow_error", "reason": null}],
                "type": "script_exception",
                "reason": "compile error",
                "script_stack": [],
                "script": src,
                "lang": "painless",
                "caused_by": {"type": "stack_overflow_error", "reason": null}
            },
            "status": 400
        });
        assert_eq!(err, want);
        for src in [
            format!("return 1{}", " + 1".repeat(40_000)),
            format!("return {}true", "!".repeat(60_000)),
            format!("int x = 0; {} x++; {} return x", "if (true) { ".repeat(20_000), "}".repeat(20_000)),
            format!("return {}{}", "[".repeat(40_000), "]".repeat(40_000)),
            format!("def x = [:]; return x{}", ".a".repeat(40_000)),
        ] {
            match run(&e, &src) {
                Ok(_) => {}
                Err(err) => assert_eq!(cause(&err).0, "stack_overflow_error", "{err}"),
            }
        }
        // Elasticsearch compiles a few hundred levels; so do we, and the engine is still fine
        assert_eq!(run(&e, &format!("return {}1{}", "(".repeat(300), ")".repeat(300))).unwrap(), "1");
        assert_eq!(run(&e, &format!("return 1{}", " + 1".repeat(1000))).unwrap(), "1001");
    });
}

#[test]
fn deep_values_overflow_like_a_jvm() {
    on_small_stack(|| {
        let e = engine();
        for op in ["l.toString()", "l.hashCode()", "l.equals(l2)", "[l].contains(l2)", "['k': l].containsValue(l2)", "'' + l"] {
            let src = format!("def l = []; def l2 = []; for (int i = 0; i < 200000; ++i) {{ l = [l]; l2 = [l2]; }} return {op}");
            let err = run(&e, &src).unwrap_err();
            assert_eq!(cause(&err), ("stack_overflow_error", None), "{op}: {err}");
        }
        // after an overflow the next execution starts clean
        assert_eq!(run(&e, "def l = [[1]]; return l.toString()").unwrap(), "[[1]]");
    });
}

#[test]
fn deep_values_drop_and_convert_safely() {
    on_small_stack(|| {
        let e = engine();
        let script = e.compile("def l = []; for (int i = 0; i < 200000; ++i) { l = [l]; } return l", Context::PainlessTest).unwrap();
        let mut inst = script.instance();
        let v = inst.execute(&mut NoHost).unwrap();
        // hosts convert results outside executions: bounded, never a crash
        assert_eq!(v.to_json(), Json::Null);
        let err = v.try_to_json().unwrap_err();
        assert_eq!(err.kind, "stream_constraints_exception");
        assert_eq!(err.reason, "Document nesting depth (1001) exceeds the maximum allowed (1000, from `StreamWriteConstraints.getMaxNestingDepth()`)");
        let _ = v.to_java_string();
        drop(v);
        drop(inst);
        let ok = e.compile("def l = []; for (int i = 0; i < 200; ++i) { l = [l]; } return l", Context::PainlessTest).unwrap();
        assert!(ok.instance().execute(&mut NoHost).unwrap().try_to_json().is_ok());
    });
}

#[test]
fn huge_objects_fail_before_allocating() {
    let e = engine();
    let heap = ("out_of_memory_error", Some("Java heap space"));
    for src in [
        "int[] a = new int[2000000]; return a.length",
        "def a = new long[1000][1000]; return 1",
        "def a = new def[100000][100000][0]; return 1",
        "return Collections.nCopies(2000000000, 'x').size()",
        "String s = 'ab'; while (true) { s += s; }",
        "String s = 'ab'; for (int i = 0; i < 40; ++i) { s = s + s + '!'; } return s.length()",
        "def l = [1]; for (int i = 0; i < 40; ++i) { l.addAll(l); } return l.size()",
        "def l = [1]; for (int i = 0; i < 40; ++i) { l.addAll(0, l); } return l.size()",
        "List l = []; for (int i = 0; i < 2000000; ++i) { l.add(i); } return l.size()",
        "Map m = [:]; for (int i = 0; i < 2000000; ++i) { m.put(i, i); } return m.size()",
        "Map m = [:]; for (int i = 0; i < 2000000; ++i) { m[i] = i; } return m.size()",
        "Set s = new HashSet(); for (int i = 0; i < 2000000; ++i) { s.add(i); } return s.size()",
        "StringBuilder b = new StringBuilder('x'); for (int i = 0; i < 40; ++i) { b.append(b); } return b.length()",
        "StringBuilder b = new StringBuilder('x'); for (int i = 0; i < 40; ++i) { b.insert(0, b.toString()); } return b.length()",
        "StringBuilder b = new StringBuilder(); b.setLength(2000000000); return b.length()",
        "def l = [1]; for (int i = 0; i < 40; ++i) { l = [l, l]; } return l.toString().length()",
        "def l = [1]; for (int i = 0; i < 40; ++i) { l = [l, l]; } return Json.dump(l).length()",
        "def l = ['x']; for (int i = 0; i < 40; ++i) { l = [l, l]; } return String.join(',', [l, l])",
        "String s = 'aaaaaaaaaa'; for (int i = 0; i < 10; ++i) { s = s.replace('a', s); } return s.length()",
        "return IntStream.range(0, 2000000000).sum()",
    ] {
        let err = run(&e, src).unwrap_err();
        let c = cause(&err);
        assert_eq!(c.0, "out_of_memory_error", "{src}: {err}");
        // like the JVM's, the error cannot be caught by the script
        assert!(c == heap || c.1 == Some("Requested array size exceeds VM limit"), "{src}: {err}");
    }
    assert_eq!(cause(&run(&e, "try { int[] a = new int[2000000]; return 1 } catch (Exception x) { return 2 }").unwrap_err()).0, "out_of_memory_error");
    // the JVM's own array limit, whatever the heap
    let big = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
    let err = run(&big, "return new byte[Integer.MAX_VALUE].length").unwrap_err();
    assert_eq!(cause(&err), ("out_of_memory_error", Some("Requested array size exceeds VM limit")));
    // just below the limit everything works
    assert_eq!(run(&e, "int[] a = new int[200000]; return a.length").unwrap(), "200000");
    assert_eq!(run(&e, "List l = []; for (int i = 0; i < 20000; ++i) { l.add(i); } return l.size()").unwrap(), "20000");
}

/// A host whose search can be cancelled after a number of polls.
struct Cancelling {
    polls: u32,
    cancel_after: u32,
}

impl Host for Cancelling {
    fn check_cancelled(&mut self) -> Result<(), HostError> {
        self.polls += 1;
        if self.polls > self.cancel_after {
            return Err(HostError { kind: "task_cancelled_exception", reason: "cancelled".into(), frames: Vec::new() });
        }
        Ok(())
    }
}

#[test]
fn cancellable_loops_poll_instead_of_counting() {
    let e = engine();
    let script = e.compile("long s = 0; for (int i = 0; i < 2000000; ++i) { s += i; } return s", Context::Score).unwrap();
    // `_execute` and write scripts count loop statements
    let mut inst = script.instance();
    let err = inst.execute(&mut Cancelling { polls: 0, cancel_after: u32::MAX }).unwrap_err().to_response();
    assert_eq!(err["error"]["root_cause"][0]["type"], "painless_error");
    // a search binds its cancellation check: no statement limit, a poll every 1,000 iterations
    let mut inst = script.instance();
    inst.set_cancellable(true);
    let mut host = Cancelling { polls: 0, cancel_after: u32::MAX };
    assert_eq!(inst.execute_f64(&mut host).unwrap(), 1999999000000.0);
    assert_eq!(host.polls, 2000);
    // and stops when the search is cancelled
    let mut host = Cancelling { polls: 0, cancel_after: 3 };
    let err = inst.execute(&mut host).unwrap_err().to_response();
    assert_eq!(err["error"]["type"], "script_exception");
    assert_eq!(err["error"]["caused_by"]["type"], "task_cancelled_exception");
    assert_eq!(err["error"]["caused_by"]["reason"], "cancelled");
    assert_eq!(host.polls, 4);
    // an endless loop ends with the search
    let endless = e.compile("int i = 0; while (true) { i++; }", Context::Score).unwrap();
    let mut inst = endless.instance();
    inst.set_cancellable(true);
    let err = inst.execute(&mut Cancelling { polls: 0, cancel_after: 10 }).unwrap_err().to_response();
    assert_eq!(err["error"]["caused_by"]["type"], "task_cancelled_exception");
    // nested loops and loops in functions and lambdas poll too
    let nested = e
        .compile("int f(int n) { int c = 0; for (int i = 0; i < n; ++i) { c++ } return c } long t = 0; for (int i = 0; i < 3000; ++i) { t += f(1000); [1, 2].forEach(x -> { int s = 0; for (int j = 0; j < 500; ++j) { s++ } return s }); } return t", Context::Score)
        .unwrap();
    let mut inst = nested.instance();
    inst.set_cancellable(true);
    let mut host = Cancelling { polls: 0, cancel_after: u32::MAX };
    assert_eq!(inst.execute_f64(&mut host).unwrap(), 3_000_000.0);
    assert!(host.polls >= 3000, "{}", host.polls);
}

#[test]
fn library_driven_lambdas_poll_for_cancellation() {
    let e = engine();
    // `each` / `findAll` / stream reductions iterate in library code: no script loop counts
    // them, so the lambda calls themselves poll every 1,000 calls (`$cancelPoll`).
    let script = e
        .compile("List a = new ArrayList(); for (int i = 0; i < 1000; i++) { a.add(i); } a.each(x -> a.each(y -> a.each(z -> z.hashCode()))); return 1", Context::Score)
        .unwrap();
    let mut inst = script.instance();
    inst.set_cancellable(true);
    let err = inst.execute(&mut Cancelling { polls: 0, cancel_after: 5 }).unwrap_err().to_response();
    assert_eq!(err["error"]["caused_by"]["type"], "task_cancelled_exception");
    let stream = e.compile("IntStream.range(0, 5000).reduce(0, (x, y) -> x + y)", Context::Score).unwrap();
    let mut inst = stream.instance();
    inst.set_cancellable(true);
    let mut host = Cancelling { polls: 0, cancel_after: u32::MAX };
    assert_eq!(inst.execute_f64(&mut host).unwrap(), 12497500.0);
    assert_eq!(host.polls, 5);
    // outside a search nothing polls
    let mut inst = stream.instance();
    let mut host = Cancelling { polls: 0, cancel_after: 0 };
    assert_eq!(inst.execute_f64(&mut host).unwrap(), 12497500.0);
    assert_eq!(host.polls, 0);
}

#[test]
fn scripts_cannot_catch_cancellation() {
    let e = engine();
    // Elasticsearch's `TimeExceededException` can't be caught from Painless: a script that
    // catches `Exception` around a loop must still stop once the search is cancelled.
    let script = e
        .compile("int n = 0; while (true) { try { for (long i = 0; ; i++) {} } catch (Exception e) { n++; } }", Context::Score)
        .unwrap();
    let mut inst = script.instance();
    inst.set_cancellable(true);
    let mut host = Cancelling { polls: 0, cancel_after: 3 };
    let err = inst.execute(&mut host).unwrap_err().to_response();
    assert_eq!(err["error"]["caused_by"]["type"], "task_cancelled_exception");
    assert_eq!(host.polls, 4);
    // the same through a library-driven lambda
    let script = e.compile("List a = [1, 2]; int n = 0; while (true) { try { a.removeIf(x -> { long k = 0; for (long i = 0; i < 100000000000L; i++) { k++; } return k > 0; }); } catch (Exception t) { n++; } }", Context::Score).unwrap();
    let mut inst = script.instance();
    inst.set_cancellable(true);
    let err = inst.execute(&mut Cancelling { polls: 0, cancel_after: 2 }).unwrap_err().to_response();
    assert_eq!(err["error"]["caused_by"]["type"], "task_cancelled_exception");
}

/// A host that panics while reading doc values.
struct Panicking;

impl Host for Panicking {
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        if field == "boom" {
            panic!("host bug");
        }
        out.clear(DocKind::Long);
        out.longs.push(7);
        Ok(true)
    }
}

#[test]
fn panics_become_script_errors() {
    let e = engine();
    let script = e.compile("doc['boom'].value + doc['x'].value", Context::Score).unwrap();
    let mut inst = script.instance();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let err = inst.execute(&mut Panicking).unwrap_err();
    std::panic::set_hook(prev);
    assert_eq!(err.status, 500);
    let r = err.to_response();
    assert_eq!(r["error"]["caused_by"]["type"], "illegal_state_exception");
    assert!(r["error"]["caused_by"]["reason"].as_str().unwrap().contains("host bug"));
    // the instance keeps working
    let ok = e.compile("doc['x'].value * 2", Context::Score).unwrap();
    let mut inst = ok.instance();
    assert_eq!(inst.execute_f64(&mut Panicking).unwrap(), 14.0);
}

#[test]
fn values_outlive_their_execution() {
    // a result handed to the host is an ordinary value: converting it needs no execution
    let e = engine();
    let script = e.compile("def m = ['a': [1, 2.5, 'x']]; m.b = m.a; return m", Context::PainlessTest).unwrap();
    let v = script.instance().execute(&mut NoHost).unwrap();
    assert!(matches!(v, Value::Map(_)));
    assert_eq!(v.to_json(), json!({"a": [1, 2.5, "x"], "b": [1, 2.5, "x"]}));
}

/// `script.painless.max_allocation_bytes`: allocations sized like a HotSpot heap's, charged
/// per execution against the context's limit.
#[test]
fn allocation_limit_per_context() {
    let e = Engine::new(Settings { max_compilations_rate: None, max_allocation_bytes: vec![(Context::PainlessTest, 100)], ..Settings::default() });
    let reason = |src: &str| run(&e, src).unwrap_err()["error"]["caused_by"]["reason"].as_str().unwrap_or("").to_string();
    // 16 + 30 * 4
    assert_eq!(reason("int[] a = new int[30]; return 1"), "script allocation limit exceeded: allocation of [136] bytes brings the running total to [136] bytes, over the limit of [100] bytes");
    // [2][5] longs: 16 + 10 * 8 each
    assert_eq!(
        reason("def a = new long[2][5]; def b = new long[2][5]; return 1"),
        "script allocation limit exceeded: allocation of [96] bytes brings the running total to [192] bytes, over the limit of [100] bytes"
    );
    // a concatenation: 32 + each string's UTF-16 bytes + an int's longest rendering
    assert_eq!(
        reason("String s = 'abcdefghij'; return s + 1 + s + s"),
        "script allocation limit exceeded: allocation of [114] bytes brings the running total to [114] bytes, over the limit of [100] bytes"
    );
    // boxing: 24 bytes per long into a list
    assert_eq!(reason("List l = []; for (long i = 0; i < 10; ++i) { l.add(i) } return 1"), "script allocation limit exceeded: allocation of [24] bytes brings the running total to [120] bytes, over the limit of [100] bytes");
    // below the limit, and in an untracked context
    assert_eq!(run(&e, "int[] a = new int[10]; return a.length").unwrap(), "10");
    let score = e.compile("int[] a = new int[1000]; return a.length", Context::Score).unwrap();
    assert_eq!(score.instance().execute_f64(&mut NoHost).unwrap(), 1000.0);
    // the total starts over with every execution
    let s = e.compile("int[] a = new int[15]; return a.length", Context::PainlessTest).unwrap();
    let mut inst = s.instance();
    for _ in 0..3 {
        assert_eq!(inst.execute(&mut NoHost).unwrap().to_java_string(), "15");
    }
}

/// The security review's script, scaled down: every copy is under the object cap, together
/// they would be 1 GiB (125 GiB with its 128 MiB string). The execution's account refuses it
/// the way Elasticsearch 9.5's per-execution guard does, and the thread never holds much more
/// than the limit.
#[test]
fn memory_per_execution_is_bounded() {
    let e = limited(4 << 20);
    let src = "String s = 'x'; while (s.length() < 1024 * 1024) { s = s + s; } List l = new ArrayList(); for (int i = 0; i < 1000; i++) { l.add(s + i); } return l.size();";
    let (r, grew) = peak_growth(|| run(&e, src));
    let err = r.unwrap_err();
    assert_eq!(err["error"]["type"], "script_exception", "{err}");
    assert_eq!(err["error"]["script_stack"], json!(["<<< unknown portion of script >>>"]));
    assert_eq!(err["error"]["root_cause"][0]["type"], "painless_error");
    let (kind, reason) = cause(&err);
    assert_eq!(kind, "painless_error");
    let reason = reason.unwrap();
    assert!(reason.starts_with("script allocation limit exceeded: allocation of [") && reason.ends_with("bytes, over the limit of [4194304] bytes"), "{reason}");
    assert!(grew < 8 << 20, "the thread held {grew} bytes");
}

/// What an execution holds counts, not what it allocated: 128 MiB of temporaries and a list of
/// a thousand strings in a 4 MiB account, run again and again.
#[test]
fn memory_under_the_limit_is_unaffected() {
    let e = limited(4 << 20);
    let script = e.compile("String s = 'x'; while (s.length() < 64 * 1024) { s = s + s; } int n = 0; for (int i = 0; i < 2000; i++) { String t = s + i; n += t.length(); } List l = []; for (int i = 0; i < 1000; i++) { l.add('v' + i); } return n + l.size();", Context::PainlessTest).unwrap();
    let mut inst = script.instance();
    let digits: i64 = (0..2000).map(|i: i64| i.to_string().len() as i64).sum();
    for _ in 0..3 {
        assert_eq!(inst.execute(&mut NoHost).unwrap().to_java_string(), (2000 * 65536 + digits + 1000).to_string());
    }
}

/// A host that adds up what executions report, refusing past a limit like a search's
/// `request` breaker.
struct Breaker {
    held: i64,
    limit: i64,
}

impl Host for Breaker {
    fn script_memory(&mut self, delta: i64) -> Result<(), MemoryRefused> {
        self.held += delta;
        if self.held > self.limit {
            return Err(MemoryRefused { reason: format!("[request] Data too large, data for [<script>] would be [{}b]", self.held), wanted: self.held as u64, limit: self.limit as u64 });
        }
        Ok(())
    }
}

/// Each execution has its own account, so a script run per document is also charged to its
/// host what it keeps from one execution to the next (here in a scripted metric's state), and
/// nothing for what it returns or throws away.
#[test]
fn hosts_hear_what_executions_keep() {
    let e = limited(4 << 20);
    let returns = e.compile("String t = 'x'; while (t.length() < 64 * 1024) { t = t + t; } return t + 'y'", Context::PainlessTest).unwrap();
    let mut inst = returns.instance();
    inst.set_result_accounting(true);
    let mut host = Breaker { held: 0, limit: 1 << 20 };
    for _ in 0..1000 {
        let v = inst.execute(&mut host).unwrap();
        assert_eq!(v.to_java_string().len(), 65537);
    }
    assert!(host.held < 1 << 20, "{}", host.held);

    let keeps = e.compile("if (!state.containsKey('l')) { state.l = []; } state.l.add(new int[16384]);", Context::AggsMap).unwrap();
    let mut inst = keeps.instance();
    inst.set_var("state", Value::from_json_map(&json!({})));
    let mut host = Breaker { held: 0, limit: 10 << 20 };
    let runs = (0..1000).position(|_| inst.execute(&mut host).is_err()).expect("the breaker refuses");
    // 64 KiB a document: well past one execution's account, refused near the breaker's limit
    assert!((100..200).contains(&runs), "{runs}");
    let err = inst.execute(&mut host).unwrap_err().to_response();
    assert_eq!(cause(&err).0, "circuit_breaking_exception", "{err}");
}

/// Library calls that build many objects in one call look at the account as they go: a split,
/// a regex split and a parsed document stop near the limit, not after building it all
/// (a 2 MiB string of `a,` splits into a million strings, about 100 MiB).
#[test]
fn bulk_builders_stop_near_the_limit() {
    let e = limited(4 << 20);
    let input = "String s = 'a,'; while (s.length() < 2 * 1024 * 1024) { s = s + s; }";
    for (what, src) in [
        ("splitOnToken", format!("{input} return s.splitOnToken(',').length;")),
        ("Pattern.split", format!("{input} return /,/.split(s).length;")),
        ("Json.load", "StringBuilder b = new StringBuilder('['); for (int i = 0; i < 500000; i++) { b.append('1,'); } b.append('1]'); return Json.load(b.toString()).size();".to_string()),
    ] {
        let (r, grew) = peak_growth(|| run(&e, &src));
        let err = r.unwrap_err();
        assert_eq!(cause(&err).0, "painless_error", "{what}: {err}");
        assert!(grew < 8 << 20, "{what}: the thread held {grew} bytes");
    }
}

/// A size known before it is allocated is checked against the room first.
#[test]
fn known_sizes_are_refused_before_allocating() {
    let e = limited(4 << 20);
    let (r, grew) = peak_growth(|| run(&e, "StringBuilder b = new StringBuilder(); b.setLength(250000000); return b.length()"));
    assert_eq!(cause(&r.unwrap_err()).0, "painless_error");
    assert!(grew < 1 << 20, "the thread held {grew} bytes");
}

/// A host whose `_source` is parsed lazily, when a script first reads it.
struct LargeSource;

impl Host for LargeSource {
    fn source(&mut self) -> Option<Value> {
        Some(Value::from_json_map(&json!({ "big": "x".repeat(8 << 20) })))
    }
}

/// What the host allocates for itself during an execution (a lazily parsed `_source` larger
/// than the limit) isn't the script's: Elasticsearch runs these scripts.
#[test]
fn host_allocations_are_not_the_scripts() {
    let e = limited(4 << 20);
    let script = e.compile("return params['_source']['big'].length()", Context::Score).unwrap();
    let mut inst = script.instance();
    inst.set_params(&json!({}));
    assert_eq!(inst.execute_f64(&mut LargeSource).unwrap(), (8 << 20) as f64);
}

/// A host that hands scripts fresh data on every call: a 64 KiB `_source` and stored field.
/// After 3,000 calls it has nothing more, so a broken guard fails the test's assertions
/// instead of taking the machine's memory.
struct FreshData {
    calls: u32,
}

impl FreshData {
    fn more(&mut self) -> bool {
        self.calls += 1;
        self.calls <= 3000
    }
}

impl Host for FreshData {
    fn source(&mut self) -> Option<Value> {
        self.more().then(|| Value::from_json_map(&json!({ "body": "x".repeat(64 << 10) })))
    }
    fn stored_field(&mut self, _field: &str) -> Result<Option<Vec<Value>>, HostError> {
        Ok(self.more().then(|| vec![Value::string("y".repeat(64 << 10))]))
    }
}

/// Data a host hands over can't be turned into room: keeping `params._source` over and over
/// keeps one document (it is parsed once per execution), and reading and dropping host data in
/// a loop gives the script nothing to grow into. Each loop ends with the limit's failure while
/// the thread holds little more than the limit.
#[test]
fn host_data_gives_no_room() {
    let e = limited(4 << 20);
    for src in [
        "List l = []; while (true) { l.add(params._source); }",
        "List l = []; while (true) { def s = params._source; s.body = s.body + 'z'; l.add(new int[1024]); }",
        "List l = []; while (true) { def f = params._fields['f']; l.add(new int[1024]); }",
    ] {
        let script = e.compile(src, Context::Score).unwrap();
        let mut inst = script.instance();
        inst.set_params(&json!({}));
        let (r, grew) = peak_growth(|| inst.execute_f64(&mut FreshData { calls: 0 }).map_err(|e| e.to_response()));
        let err = r.unwrap_err();
        assert_eq!(cause(&err).0, "painless_error", "{src}: {err}");
        assert!(err["error"]["caused_by"]["reason"].as_str().unwrap().starts_with("script allocation limit exceeded"), "{src}: {err}");
        assert!(grew < 10 << 20, "{src}: the thread held {grew} bytes");
    }
}

/// A host whose doc values come from another script (a runtime field's), keeping what it
/// returns.
struct RuntimeField<'a> {
    script: &'a search_painless::Script,
    kept: Vec<Value>,
}

impl Host for RuntimeField<'_> {
    fn field_values(&mut self, _field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let v = self.script.instance().execute(&mut NoHost).unwrap();
        self.kept.push(v);
        out.clear(DocKind::Long);
        out.longs.push(1);
        Ok(true)
    }
}

/// What a script run inside a host call (a runtime field's) holds at its end counts against
/// the execution that called the host, while the host's own buffers don't.
#[test]
fn scripts_inside_host_calls_count() {
    let e = limited(4 << 20);
    let inner = e.compile("String s = 'x'; while (s.length() < 1024 * 1024) { s = s + s; } return s", Context::PainlessTest).unwrap();
    let outer = e.compile("List l = []; for (int i = 0; i < 100; ++i) { l.add(field('rt')); } return 1", Context::Score).unwrap();
    let mut inst = outer.instance();
    inst.set_params(&json!({}));
    let mut host = RuntimeField { script: &inner, kept: Vec::new() };
    let err = inst.execute_f64(&mut host).unwrap_err().to_response();
    assert_eq!(cause(&err).0, "painless_error", "{err}");
    assert!(host.kept.len() < 10, "{} runtime values kept", host.kept.len());
}

/// A host whose doc values fail with a 64 KiB reason, for the first 500 reads.
struct FailingField {
    calls: u32,
}

impl Host for FailingField {
    fn doc_values(&mut self, _field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        self.calls += 1;
        if self.calls > 500 {
            out.clear(DocKind::Long);
            out.longs.push(1);
            return Ok(true);
        }
        Err(HostError::illegal_argument("x".repeat(64 << 10)))
    }
}

/// A host's error is built outside the account: a script that catches and drops it in a loop
/// gets back only what the copy it was handed cost. Keeping as much as each error frees, it
/// still meets the limit (were the frees credited, the account would stay flat while the
/// thread grew by 32 MiB over the 500 errors).
#[test]
fn caught_host_errors_give_no_room() {
    let e = limited(4 << 20);
    let script = e.compile("List l = []; while (true) { try { def v = doc['body'].value; } catch (Exception x) {} l.add(new byte[65536]); }", Context::Score).unwrap();
    let mut inst = script.instance();
    inst.set_params(&json!({}));
    let (r, grew) = peak_growth(|| inst.execute_f64(&mut FailingField { calls: 0 }).map_err(|e| e.to_response()));
    let err = r.unwrap_err();
    assert_eq!(cause(&err).0, "painless_error", "{err}");
    assert!(grew < 10 << 20, "the thread held {grew} bytes");
}

/// A host whose `field()` reads a runtime field: another script on a host of its own (with an
/// 8 MiB `_source`), whose errors it lets go as `on_script_error: continue` does, charging what
/// its scripts hold to a breaker like a search's.
struct LenientRuntime<'a> {
    script: &'a search_painless::Script,
    held: i64,
}

impl Host for LenientRuntime<'_> {
    fn field_values(&mut self, _field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let mut inst = self.script.instance();
        inst.set_params(&json!({}));
        let _ = inst.execute(&mut LargeSource);
        out.clear(DocKind::Long);
        out.longs.push(1);
        Ok(true)
    }
    fn script_memory(&mut self, delta: i64) -> Result<(), MemoryRefused> {
        self.held += delta;
        Ok(())
    }
}

/// An execution a host call starts leaves its caller nothing it doesn't keep: not its error
/// (a runtime field throwing a 64 KiB message on every document), nor the `_source` it read,
/// which its own limit leaves out.
#[test]
fn runtime_fields_leave_their_caller_nothing() {
    let e = limited(4 << 20);
    let throws = e.compile("String s = 'x'; while (s.length() < 65536) { s = s + s; } throw new IllegalArgumentException(s);", Context::Score).unwrap();
    let reads = e.compile("return params['_source']['big'].length()", Context::Score).unwrap();
    for (inner, n) in [(&throws, 1000), (&reads, 3)] {
        let outer = e.compile(&format!("for (int i = 0; i < {n}; ++i) {{ field('rt'); }} return 1"), Context::Score).unwrap();
        let mut inst = outer.instance();
        inst.set_params(&json!({}));
        let mut host = LenientRuntime { script: inner, held: 0 };
        assert_eq!(inst.execute_f64(&mut host).map_err(|e| e.to_response()).unwrap(), 1.0);
        assert!(host.held < 1 << 20, "the breaker holds {}", host.held);
    }
}

/// A host whose `field()` reads a runtime field on one instance kept across documents, whose
/// script returns a field of its document's 8 MiB `_source`, kept until the next document as
/// an emitted value is; it charges what its scripts hold to a breaker like a search's.
struct KeepsSource<'a> {
    inner: search_painless::Instance<'a>,
    held: i64,
}

impl Host for KeepsSource<'_> {
    fn field_values(&mut self, _field: &str, out: &mut DocValues) -> Result<bool, HostError> {
        let _ = self.inner.execute(&mut LargeSource);
        out.clear(DocKind::Long);
        out.longs.push(1);
        Ok(true)
    }
    fn script_memory(&mut self, delta: i64) -> Result<(), MemoryRefused> {
        self.held += delta;
        Ok(())
    }
}

/// What a runtime field keeps of the `_source` it was handed stays outside its caller's limit,
/// document after document, and its caller's breaker hears nothing of it.
#[test]
fn a_runtime_field_keeping_its_source_charges_its_caller_nothing() {
    let e = limited(4 << 20);
    let inner = e.compile("return params['_source']['big']", Context::Field).unwrap();
    let mut kept = inner.instance();
    kept.set_params(&json!({}));
    kept.set_result_accounting(true);
    let outer = e.compile("for (int i = 0; i < 3; ++i) { field('rt'); } return 1", Context::Score).unwrap();
    let mut inst = outer.instance();
    inst.set_params(&json!({}));
    let mut host = KeepsSource { inner: kept, held: 0 };
    for _ in 0..2 {
        assert_eq!(inst.execute_f64(&mut host).map_err(|e| e.to_response()).unwrap(), 1.0);
    }
    assert!(host.held < 1 << 20, "the breaker holds {}", host.held);
}
