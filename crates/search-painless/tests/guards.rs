//! Scripts that would take a Rust process down — deep nesting, huge allocations, endless loops,
//! panics in the host — fail the way Elasticsearch fails them (a `script_exception` caused by a
//! `stack_overflow_error`, an `out_of_memory_error`, or the host's cancellation) and leave the
//! engine usable.

use search_painless::{Context, DocKind, DocValues, Engine, Host, HostError, Lang, NoHost, Settings, Value};
use serde_json::{json, Value as Json};

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
        .compile("int f(int n) { int c = 0; for (int i = 0; i < n; ++i) { c++ } return c } long t = 0; for (int i = 0; i < 3000; ++i) { t += f(1000); [1, 2].forEach(x -> { for (int j = 0; j < 500; ++j) {} }); } return t", Context::Score)
        .unwrap();
    let mut inst = nested.instance();
    inst.set_cancellable(true);
    let mut host = Cancelling { polls: 0, cancel_after: u32::MAX };
    assert_eq!(inst.execute_f64(&mut host).unwrap(), 3_000_000.0);
    assert!(host.polls >= 3000, "{}", host.polls);
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
