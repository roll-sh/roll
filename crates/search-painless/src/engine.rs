//! The public face of the crate: compiling scripts through a cache and running them.
//!
//! [`Engine`] owns Elasticsearch's script-service limits — the inline size limit
//! (`script.max_size_in_bytes`), the compiled-script cache (`script.cache.max_size`,
//! `script.cache.expire`) and the compilation-rate token bucket (`script.max_compilations_rate`) —
//! plus the Painless settings (`script.painless.regex.enabled`, `…regex.limit-factor`, the loop
//! counter). A compiled [`Script`] is immutable and shared (`Arc`) across threads.
//!
//! Running a script needs per-thread mutable state (constants materialized as values, call-site
//! caches, a register stack), which lives in an [`Instance`]. Hot loops (`script_score` over a
//! segment) create one instance and call [`Instance::execute`] per document, so nothing is
//! allocated per document beyond what the script itself allocates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use crate::analyze::{self, RegexMode};
use crate::compile;
use crate::contexts::{Context, ContextDef};
use crate::error::{self, ScriptError};
use crate::host::Host;
use crate::lookup::registry;
use crate::mustache::Template;
use crate::parser;
use crate::types::cls;
use crate::value::{JMap, Obj, Value};
use crate::vm::{ConstVal, Program, State, Vm};

/// Engine-wide settings, with Elasticsearch's defaults.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `script.max_size_in_bytes` (65535).
    pub max_size_in_bytes: usize,
    /// `script.cache.max_size` (3000).
    pub cache_max_size: usize,
    /// `script.cache.expire` (none).
    pub cache_expire: Option<Duration>,
    /// `script.max_compilations_rate` (150 per 5 minutes); `None` is `unlimited`.
    pub max_compilations_rate: Option<(u32, Duration)>,
    /// `script.painless.regex.enabled` (`limited`).
    pub regex_enabled: RegexEnabled,
    /// `script.painless.regex.limit-factor` (6).
    pub regex_limit_factor: i32,
    /// Painless' loop counter (`max_loop_counter`, 1,000,000).
    pub max_loop_counter: u32,
    /// Largest single object (array, string, collection) a script may build, in bytes
    /// (256 MiB). Not an Elasticsearch setting: a JVM fails such scripts with an
    /// `OutOfMemoryError` once its heap is exhausted (and Elasticsearch then exits); scripts here
    /// fail with the same `out_of_memory_error` before allocating.
    pub max_object_bytes: usize,
    /// `script.painless.max_allocation_bytes.context.<context>.limit` (Elasticsearch 9.5): the
    /// contexts whose scripts charge their allocations (boxing, arrays, string concatenation,
    /// lambda captures, sized like a HotSpot heap) against a per-execution limit, failing with
    /// a `painless_error` past it. Empty (Elasticsearch's `-1b`): no tracking.
    pub max_allocation_bytes: Vec<(Context, u64)>,
    /// Native stack one execution may use (1 MiB): deep recursions (script functions called
    /// from library code, `toString`/`equals` of deeply nested values) fail with a
    /// `stack_overflow_error` beyond it, as on a JVM. Hosts run executions on threads with at
    /// least this much stack to spare (Rust's default thread stack is 2 MiB).
    pub stack_bytes: usize,
}

/// `script.painless.regex.enabled`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexEnabled {
    True,
    False,
    Limited,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            max_size_in_bytes: 65535,
            cache_max_size: 3000,
            cache_expire: None,
            max_compilations_rate: Some((150, Duration::from_secs(300))),
            regex_enabled: RegexEnabled::Limited,
            regex_limit_factor: 6,
            max_loop_counter: 1_000_000,
            max_object_bytes: crate::guard::DEFAULT_MAX_OBJECT_BYTES,
            stack_bytes: crate::guard::EXECUTION_STACK,
            max_allocation_bytes: Vec::new(),
        }
    }
}

/// A script language.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Painless,
    Mustache,
}

impl Lang {
    pub fn from_name(s: &str) -> Option<Lang> {
        match s {
            "painless" => Some(Lang::Painless),
            "mustache" => Some(Lang::Mustache),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Lang::Painless => "painless",
            Lang::Mustache => "mustache",
        }
    }
}

/// A compiled script of either language.
pub enum Compiled {
    Painless(Arc<Script>),
    Mustache(Arc<Template>),
}

impl Clone for Compiled {
    fn clone(&self) -> Self {
        match self {
            Compiled::Painless(s) => Compiled::Painless(s.clone()),
            Compiled::Mustache(t) => Compiled::Mustache(t.clone()),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    lang: Lang,
    source: Arc<str>,
    context: Context,
    options: Vec<(String, String)>,
}

struct CacheEntry {
    value: Compiled,
    last_used: u64,
    inserted: StdInstant,
}

struct Bucket {
    tokens: f64,
    last: StdInstant,
}

struct Cache {
    map: HashMap<CacheKey, CacheEntry>,
    tick: u64,
    bucket: Bucket,
    compilations: u64,
    evictions: u64,
    limit_triggered: u64,
}

/// Compilation statistics (`GET _nodes/stats/script`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub compilations: u64,
    pub cache_evictions: u64,
    pub compilation_limit_triggered: u64,
}

/// Compiles scripts through a cache and enforces Elasticsearch's script-service limits.
pub struct Engine {
    settings: Mutex<Settings>,
    cache: Mutex<Cache>,
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new(Settings::default())
    }
}

impl Engine {
    pub fn new(settings: Settings) -> Engine {
        let tokens = settings.max_compilations_rate.map(|(n, _)| n as f64).unwrap_or(0.0);
        Engine {
            settings: Mutex::new(settings),
            cache: Mutex::new(Cache {
                map: HashMap::new(),
                tick: 0,
                bucket: Bucket { tokens, last: StdInstant::now() },
                compilations: 0,
                evictions: 0,
                limit_triggered: 0,
            }),
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Replaces the settings (dynamic cluster settings). Compiled scripts stay cached unless a
    /// setting that changes compilation moved.
    pub fn update_settings(&self, s: Settings) {
        let mut cur = self.settings.lock().unwrap();
        let recompile = cur.regex_enabled != s.regex_enabled
            || cur.regex_limit_factor != s.regex_limit_factor
            || cur.max_loop_counter != s.max_loop_counter
            || cur.max_object_bytes != s.max_object_bytes
            || cur.stack_bytes != s.stack_bytes
            || cur.max_allocation_bytes != s.max_allocation_bytes;
        let rate_changed = cur.max_compilations_rate != s.max_compilations_rate;
        *cur = s.clone();
        drop(cur);
        let mut c = self.cache.lock().unwrap();
        if recompile || s.cache_max_size < c.map.len() {
            c.map.clear();
        }
        if rate_changed {
            c.bucket = Bucket { tokens: s.max_compilations_rate.map(|(n, _)| n as f64).unwrap_or(0.0), last: StdInstant::now() };
        }
    }

    pub fn stats(&self) -> Stats {
        let c = self.cache.lock().unwrap();
        Stats { compilations: c.compilations, cache_evictions: c.evictions, compilation_limit_triggered: c.limit_triggered }
    }

    /// Compiles a Painless script for a context (cached).
    pub fn compile(&self, src: &str, ctx: Context) -> Result<Arc<Script>, ScriptError> {
        match self.compile_lang(Lang::Painless, src, ctx, &[])? {
            Compiled::Painless(s) => Ok(s),
            Compiled::Mustache(_) => unreachable!(),
        }
    }

    /// Compiles a Mustache template (cached).
    pub fn compile_template(&self, src: &str, ctx: Context, options: &[(String, String)]) -> Result<Arc<Template>, ScriptError> {
        match self.compile_lang(Lang::Mustache, src, ctx, options)? {
            Compiled::Mustache(t) => Ok(t),
            Compiled::Painless(_) => unreachable!(),
        }
    }

    /// Compiles an inline script of any language. `options` are the script's `options`
    /// (`content_type` for Mustache).
    pub fn compile_lang(&self, lang: Lang, src: &str, ctx: Context, options: &[(String, String)]) -> Result<Compiled, ScriptError> {
        let settings = self.settings();
        if src.len() > settings.max_size_in_bytes {
            return Err(ScriptError::plain(
                "illegal_argument_exception",
                format!("exceeded max allowed inline script size in bytes [{}] with size [{}] for script [{src}]", settings.max_size_in_bytes, src.len()),
            ));
        }
        self.compile_unchecked(lang, src, ctx, options, &settings, true)
    }

    /// Compiles a stored script (no inline size limit; stored scripts are checked when stored).
    pub fn compile_stored(&self, lang: Lang, src: &str, ctx: Context, options: &[(String, String)]) -> Result<Compiled, ScriptError> {
        let settings = self.settings();
        self.compile_unchecked(lang, src, ctx, options, &settings, true)
    }

    fn compile_unchecked(&self, lang: Lang, src: &str, ctx: Context, options: &[(String, String)], settings: &Settings, limited: bool) -> Result<Compiled, ScriptError> {
        let key = CacheKey { lang, source: Arc::from(src), context: ctx, options: options.to_vec() };
        {
            let mut c = self.cache.lock().unwrap();
            c.tick += 1;
            let tick = c.tick;
            let expired = match (c.map.get(&key), settings.cache_expire) {
                (Some(e), Some(ttl)) => e.inserted.elapsed() > ttl,
                _ => false,
            };
            if expired {
                c.map.remove(&key);
            } else if let Some(e) = c.map.get_mut(&key) {
                e.last_used = tick;
                return Ok(e.value.clone());
            }
            if limited && ctx.compilation_rate_limited()
                && let Some((count, per)) = settings.max_compilations_rate {
                    let now = StdInstant::now();
                    let per_nano = count as f64 / per.as_nanos().max(1) as f64;
                    let passed = now.duration_since(c.bucket.last).as_nanos() as f64;
                    let mut t = c.bucket.tokens + passed * per_nano;
                    if t > count as f64 {
                        t = count as f64;
                    }
                    c.bucket.last = now;
                    if t >= 1.0 {
                        c.bucket.tokens = t - 1.0;
                    } else {
                        c.bucket.tokens = t;
                        c.limit_triggered += 1;
                        return Err(ScriptError::plain(
                            "circuit_breaking_exception",
                            format!(
                                "[script] Too many dynamic script compilations within, max: [{}]; please use indexed, or scripts with parameters instead; this limit can be changed by the [script.max_compilations_rate] setting",
                                rate_string(count, per)
                            ),
                        )
                        .with_status(429));
                    }
                }
        }
        let value = match lang {
            Lang::Painless => Compiled::Painless(Arc::new(compile_painless(src, ctx, settings)?)),
            Lang::Mustache => Compiled::Mustache(Arc::new(Template::compile(src, options)?)),
        };
        let mut c = self.cache.lock().unwrap();
        c.compilations += 1;
        if settings.cache_max_size == 0 {
            return Ok(value);
        }
        while c.map.len() >= settings.cache_max_size {
            // evict the least recently used entry
            let Some(k) = c.map.iter().min_by_key(|(_, e)| e.last_used).map(|(k, _)| k.clone()) else { break };
            c.map.remove(&k);
            c.evictions += 1;
        }
        let tick = c.tick;
        c.map.insert(key, CacheEntry { value: value.clone(), last_used: tick, inserted: StdInstant::now() });
        Ok(value)
    }
}

fn rate_string(count: u32, per: Duration) -> String {
    let secs = per.as_secs();
    let unit = if secs.is_multiple_of(86400) && secs > 0 {
        format!("{}d", secs / 86400)
    } else if secs.is_multiple_of(3600) && secs > 0 {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) && secs > 0 {
        format!("{}m", secs / 60)
    } else if per.subsec_millis() == 0 {
        format!("{secs}s")
    } else {
        format!("{}ms", per.as_millis())
    };
    format!("{count}/{unit}")
}

/// Compiles Painless without the cache.
///
/// Compilation runs on its own thread with a large stack: the parser and the analyzer recurse
/// once per nesting level of the source, and a source nested deeper than they allow fails
/// with Elasticsearch's `stack_overflow_error` instead of overflowing the caller's stack.
pub fn compile_painless(src: &str, ctx: Context, settings: &Settings) -> Result<Script, ScriptError> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new().name("painless-compile".into()).stack_size(crate::guard::COMPILE_THREAD_STACK).spawn_scoped(scope, || {
            let _guard = crate::guard::StackScope::enter(crate::guard::COMPILE_STACK, settings.max_object_bytes);
            compile_on_this_thread(src, ctx, settings)
        });
        match worker {
            Ok(handle) => match handle.join() {
                Ok(r) => r,
                Err(panic) => Err(ScriptError::internal(src, &*panic)),
            },
            // no thread to be had: compile here, with what is left of this stack
            Err(_) => {
                let _guard = crate::guard::StackScope::enter(settings.stack_bytes / 2, settings.max_object_bytes);
                compile_on_this_thread(src, ctx, settings)
            }
        }
    })
}

fn compile_on_this_thread(src: &str, ctx: Context, settings: &Settings) -> Result<Script, ScriptError> {
    let reg = registry();
    let def = ctx.def();
    let ast = parser::parse(src).map_err(|e| error::compile_error(src, &e))?;
    let regex_enabled = match settings.regex_enabled {
        RegexEnabled::True => RegexMode::True,
        RegexEnabled::False => RegexMode::False,
        RegexEnabled::Limited => RegexMode::Limited,
    };
    let limit_factor = if settings.regex_enabled == RegexEnabled::Limited { settings.regex_limit_factor } else { 0 };
    let analyzed = analyze::analyze(&ast, &def, reg, analyze::Settings { regex_enabled })
        .map_err(|e| error::compile_error(src, &e))?;
    let nondeterministic = analyzed.nondeterministic;
    let max_allocation_bytes = settings.max_allocation_bytes.iter().find(|(c, _)| *c == ctx).map_or(0, |(_, l)| *l);
    // the fast path does not charge allocations
    let fast = if max_allocation_bytes > 0 { None } else { crate::fast::plan(&analyzed.functions[analyzed.execute as usize], &def) };
    let doc_var = def.var_index("doc").map(|i| i as u32);
    let opts = compile::Options { max_loop_counter: settings.max_loop_counter, regex_limit_factor: limit_factor, max_allocation_bytes };
    let layout = compile::ExecLayout {
        params: ctx.execute_params().iter().filter_map(|p| def.var_index(p)).map(|i| i as u32).collect(),
        reserved: ctx.reserved_slots(),
    };
    let prog = compile::compile(analyzed, Arc::from(src), reg, &opts, def.vars.len(), doc_var, &layout).map_err(|e| error::compile_error(src, &e))?;
    let statements16 = prog.statements.iter().map(|&b| error::utf16_offset(src, b as usize)).collect();
    Ok(Script { context: ctx, def, prog, statements16, deterministic: !nondeterministic, fast, max_object: settings.max_object_bytes, stack_bytes: settings.stack_bytes })
}

/// A compiled Painless script.
pub struct Script {
    context: Context,
    def: ContextDef,
    prog: Program,
    statements16: Vec<u32>,
    deterministic: bool,
    /// The expression fast path, when the script has its shape ([`crate::fast`]).
    fast: Option<crate::fast::Plan>,
    /// [`Settings::max_object_bytes`] and [`Settings::stack_bytes`] at compilation.
    max_object: usize,
    stack_bytes: usize,
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Script").field("context", &self.context).field("source", &&*self.prog.source).finish()
    }
}

impl Script {
    pub fn context(&self) -> Context {
        self.context
    }
    pub fn source(&self) -> &str {
        &self.prog.source
    }
    /// Whether the script reads `_score` (scoring can be skipped otherwise).
    pub fn needs_score(&self) -> bool {
        self.prog.uses_score
    }
    /// Whether the script reads a context variable (`doc`, `ctx`, `params`, `_source`…).
    pub fn uses_var(&self, name: &str) -> bool {
        match self.def.var_index(name) {
            Some(i) => self.prog.used_vars.get(i).copied().unwrap_or(false),
            None => false,
        }
    }
    /// Whether results depend only on inputs (no `Math.random`, `System.currentTimeMillis`, …);
    /// non-deterministic scripts must not be cached in the request cache.
    pub fn is_deterministic(&self) -> bool {
        self.deterministic
    }
    /// The compiled instructions, for debugging.
    #[doc(hidden)]
    pub fn disassemble(&self) -> String {
        let mut out = String::new();
        for (i, f) in self.prog.functions.iter().enumerate() {
            out.push_str(&format!("fn {i} ({} regs):\n", f.nregs));
            for (pc, insn) in f.code.iter().enumerate() {
                out.push_str(&format!("  {pc:4}  {insn:?}\n"));
            }
        }
        for (i, c) in self.prog.casts.iter().enumerate() {
            out.push_str(&format!("cast {i}: {c:?}\n"));
        }
        out
    }

    /// The `doc['field']` names the script reads with literal keys (for prefetching).
    pub fn doc_fields(&self) -> &[Box<str>] {
        &self.prog.doc_fields
    }

    /// A fresh per-thread instance.
    pub fn instance(&self) -> Instance<'_> {
        Instance::new(self)
    }

    /// One-shot run: binds `params` and the named variables, executes, and returns the result.
    /// Variables the script wrote (`ctx`, `state`) are written back into `vars`.
    pub fn run(&self, host: &mut dyn Host, params: &serde_json::Value, vars: &mut Vars) -> Result<Value, ScriptError> {
        let mut inst = self.instance();
        inst.set_params(params);
        for (k, v) in vars.0.iter() {
            inst.set_var(k, v.clone());
        }
        let r = inst.execute(host);
        for (k, v) in vars.0.iter_mut() {
            if let Some(i) = self.def.var_index(k) {
                *v = inst.state.vars[i].clone();
            }
        }
        r
    }

    /// Converts a thrown exception into Elasticsearch's `script_exception`.
    #[cold]
    #[inline(never)]
    fn runtime_error(&self, t: &crate::vm::Throw) -> ScriptError {
        let src = &*self.prog.source;
        let offset = match t.offset {
            Some(u32::MAX | crate::vm::NO_SCRIPT_FRAME) | None => None,
            Some(o) => Some(o),
        };
        let name = source_name(src);
        let mut e = error::runtime_error(src, &name, &self.statements16, offset, &t.frames, &t.es_type(), t.message.clone(), !t.catchable || t.es_root);
        if t.offset == Some(crate::vm::NO_SCRIPT_FRAME) {
            // the JVM cut the trace before the script's frame: no script portion at all
            e.script_stack.pop();
        }
        e.metadata = t.metadata.clone();
        e.cause_fields = t.cause_fields.clone();
        if let Some(status) = t.status {
            e.status = status;
        }
        e
    }
}

/// `Location.computeSourceName`: runtime errors name the script by its first line (cut at
/// `\n`, or at an earlier `\r` when there is a `\n`), at most 256 UTF-16 units, with ` ...`
/// when anything was cut.
fn source_name(src: &str) -> String {
    let units: Vec<u16> = src.encode_utf16().collect();
    let pos = |c: u8| units.iter().position(|&u| u == c as u16);
    let mut limit = match pos(b'\n') {
        Some(n) => pos(b'\r').map_or(n, |r| r.min(n)),
        None => units.len(),
    };
    limit = limit.min(256);
    let mut name = String::from_utf16_lossy(&units[..limit]);
    if limit != units.len() {
        name.push_str(" ...");
    }
    name
}

/// A fast-path result.
enum FastOut {
    D(f64),
    Z(bool),
    Void,
    Val(Value),
}

impl From<crate::fast::Ret> for FastOut {
    fn from(r: crate::fast::Ret) -> FastOut {
        match r {
            crate::fast::Ret::D(x) => FastOut::D(x),
            crate::fast::Ret::Z(b) => FastOut::Z(b),
            crate::fast::Ret::Void => FastOut::Void,
            crate::fast::Ret::Val(v) => FastOut::Val(v),
        }
    }
}

/// Named variables passed to [`Script::run`] (`ctx`, `state`, `states`, `_value`, …).
#[derive(Default, Debug)]
pub struct Vars(pub Vec<(String, Value)>);

impl Vars {
    pub fn new() -> Vars {
        Vars(Vec::new())
    }
    pub fn set(&mut self, name: &str, v: Value) {
        match self.0.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = v,
            None => self.0.push((name.to_string(), v)),
        }
    }
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }
}

/// Per-thread state for running one script many times.
pub struct Instance<'s> {
    script: &'s Script,
    state: State,
    stack: Vec<Value>,
    fast: Fast,
    /// This instance already reported a fast-path run to the program.
    fast_ran: bool,
}

/// The instance's specialization of the script's fast path.
enum Fast {
    /// Not specialized yet (or invalidated by new `params` or variables).
    Untried,
    Ready(crate::fast::Compiled),
    /// No fast path: the script has none, or its values do not fit one.
    Off,
}

impl<'s> Instance<'s> {
    fn new(script: &'s Script) -> Instance<'s> {
        let prog = &script.prog;
        let consts = prog
            .consts
            .iter()
            .map(|c| match c {
                ConstVal::Long(v) => Value::Long(*v),
                ConstVal::Float(v) => Value::Float(*v),
                ConstVal::Double(v) => Value::Double(*v),
                ConstVal::Str(s) => Value::str(s),
                ConstVal::Byte(v) => Value::Byte(*v),
                ConstVal::Short(v) => Value::Short(*v),
                ConstVal::Char(v) => Value::Char(*v),
            })
            .collect();
        let patterns = prog
            .patterns
            .iter()
            .map(|p| match p {
                crate::vm::ProgConst::Regex(r) => Value::obj(Obj::Pattern(r.clone())),
                crate::vm::ProgConst::Extractor(x) => crate::es::es_value(crate::es::EsObj::Extractor(x.clone())),
            })
            .collect();
        let mut vars: Vec<Value> = vec![Value::Null; script.def.vars.len()];
        for (i, v) in script.def.vars.iter().enumerate() {
            vars[i] = crate::es::default_var(script.context, v.name);
        }
        let mut state = State::new(prog, consts, patterns, vars);
        state.has_doc = script.def.var_index("doc").is_some();
        state.ctx_var = script.def.var_index("ctx").map(|i| i as u32);
        state.field_name = std::rc::Rc::from(script.context.name());
        let fast = if script.fast.is_some() { Fast::Untried } else { Fast::Off };
        Instance { script, state, stack: Vec::with_capacity(64), fast, fast_ran: false }
    }

    pub fn script(&self) -> &'s Script {
        self.script
    }

    /// Binds `params` (converted once; search contexts also expose `_source` through it).
    pub fn set_params(&mut self, params: &serde_json::Value) {
        let v = match params {
            serde_json::Value::Object(_) => Value::from_json(params, cls::HASH_MAP),
            _ => Value::Map(std::rc::Rc::new(JMap::hash_map())),
        };
        self.set_params_value(v);
    }

    /// Binds an already converted `params` map.
    pub fn set_params_value(&mut self, v: Value) {
        self.reset_fast();
        let v = crate::es::wrap_params(self.script.context, v);
        if let Some(i) = self.script.def.var_index("params") {
            self.state.vars[i] = v;
        }
    }

    /// Sets a context variable (`ctx`, `state`, `states`, `_value`, `values`…). Unknown names
    /// are ignored.
    pub fn set_var(&mut self, name: &str, v: Value) {
        self.reset_fast();
        if let Some(i) = self.script.def.var_index(name) {
            self.state.vars[i] = v;
        }
    }

    /// Current value of a context variable (after execution: what the script left in it).
    pub fn var(&self, name: &str) -> Option<&Value> {
        self.script.def.var_index(name).map(|i| &self.state.vars[i])
    }

    /// Runs loops the way Elasticsearch 9.5 runs them in the contexts a search can cancel
    /// (`script_score`, script queries, `script_fields`, sort and aggregation scripts, runtime
    /// fields…, when the search binds its cancellation check): without the
    /// `max_loop_counter` statement limit, polling [`Host::check_cancelled`] every 1,000
    /// iterations instead. Off by default, which is how the `_execute` API and write scripts
    /// (update, ingest) run.
    pub fn set_cancellable(&mut self, on: bool) {
        self.state.cancellable = on;
    }

    /// Values emitted by a runtime-field script during the last execution.
    pub fn emitted(&self) -> &[Value] {
        &self.state.emitted
    }

    /// Values a composite runtime-field script emitted, per sub-field (`<field>.<sub>`), in
    /// first-emit order.
    pub fn emitted_fields(&self) -> &[(String, Vec<Value>)] {
        &self.state.emitted_fields
    }

    /// Names the runtime field (`emit` limit messages and composite sub-field prefixes); by
    /// default the context name, as in the `_execute` API.
    pub fn set_field_name(&mut self, name: &str) {
        self.state.field_name = std::rc::Rc::from(name);
    }

    /// What a score script passed to `explanation.set(…)` during the last execution.
    pub fn explanation(&self) -> Option<&str> {
        self.state.explanation.as_deref()
    }

    /// Runs `execute`.
    pub fn execute(&mut self, host: &mut dyn Host) -> Result<Value, ScriptError> {
        if !matches!(self.fast, Fast::Off) {
            match self.run_fast_guarded(host)? {
                Some(FastOut::D(x)) => return Ok(Value::Double(x)),
                Some(FastOut::Z(b)) => return Ok(Value::Bool(b)),
                Some(FastOut::Void) => return Ok(Value::Null),
                Some(FastOut::Val(v)) => return Ok(v),
                None => {}
            }
        }
        self.run_entry(host, std::mem::take)
    }

    /// Runs `execute` and returns the result as a double (score, number sort, similarity).
    pub fn execute_f64(&mut self, host: &mut dyn Host) -> Result<f64, ScriptError> {
        if !matches!(self.fast, Fast::Off) {
            match self.run_fast_guarded(host)? {
                Some(FastOut::D(x)) => return Ok(x),
                Some(FastOut::Val(v)) => return Ok(v.as_f64().unwrap_or(0.0)),
                _ => {}
            }
        }
        self.run_entry(host, |v| match *v {
            Value::Double(d) => d,
            ref other => other.as_f64().unwrap_or(0.0),
        })
    }

    /// Runs `execute` and returns the result as a boolean (filter, conditionals).
    pub fn execute_bool(&mut self, host: &mut dyn Host) -> Result<bool, ScriptError> {
        if !matches!(self.fast, Fast::Off)
            && let Some(FastOut::Z(b)) = self.run_fast_guarded(host)?
        {
            return Ok(b);
        }
        self.run_entry(host, |v| matches!(v, Value::Bool(true)))
    }

    /// Whether the expression fast path is specialized for this instance.
    #[doc(hidden)]
    pub fn fast_path_active(&self) -> bool {
        matches!(self.fast, Fast::Ready(_))
    }

    /// Turns the expression fast path off (tests compare it with the interpreter).
    #[doc(hidden)]
    pub fn disable_fast_path(&mut self) {
        self.fast = Fast::Off;
    }

    fn reset_fast(&mut self) {
        if !matches!(self.fast, Fast::Off) || self.script.fast.is_some() {
            self.fast = if self.script.fast.is_some() { Fast::Untried } else { Fast::Off };
        }
    }

    /// [`Instance::run_fast`], with a panic (a bug, here or in the host) reported as an error.
    #[inline(always)]
    fn run_fast_guarded(&mut self, host: &mut dyn Host) -> Result<Option<FastOut>, ScriptError> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_fast(host))) {
            Ok(r) => Ok(r),
            Err(panic) => {
                self.stack.clear();
                self.state.begin_document();
                Err(ScriptError::internal(&self.script.prog.source, &*panic))
            }
        }
    }

    /// The fast path for this document: `None` when the instance has none or it bailed out
    /// (the interpreter then runs the document).
    fn run_fast(&mut self, host: &mut dyn Host) -> Option<FastOut> {
        let script = self.script;
        let prog = &script.prog;
        // a new document: only the doc-values generation matters to the fast path (it emits
        // nothing and explains nothing; the interpreter resets the rest if it takes over)
        self.state.doc_gen = self.state.doc_gen.wrapping_add(1);
        if matches!(self.fast, Fast::Untried) {
            let plan = script.fast.as_ref()?;
            let params = script.def.var_index("params").map(|i| self.state.vars[i].clone());
            let mut vm = Vm::new(prog, &mut self.state, host, &mut self.stack);
            self.fast = match crate::fast::specialize(plan, params.as_ref(), script.def.ret, &mut vm) {
                Some(c) => Fast::Ready(c),
                None => Fast::Off,
            };
            return match &self.fast {
                Fast::Ready(c) => c.run(&mut vm).map(FastOut::from),
                _ => None,
            };
        }
        let mut vm = Vm::new(prog, &mut self.state, host, &mut self.stack);
        match &self.fast {
            Fast::Ready(c) => {
                let out = c.run(&mut vm).map(FastOut::from);
                if out.is_some() && !self.fast_ran {
                    self.fast_ran = true;
                    prog.fast_ran.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                out
            }
            _ => None,
        }
    }

    #[inline]
    fn run_entry<R>(&mut self, host: &mut dyn Host, read: impl FnOnce(&mut Value) -> R) -> Result<R, ScriptError> {
        let script = self.script;
        let prog = &script.prog;
        self.state.begin_document();
        self.stack.clear();
        let _scope = crate::guard::StackScope::enter(script.stack_bytes, script.max_object);
        crate::guard::take_pending();
        // a panic is a bug in the engine: it must not unwind into the host (or poison its
        // thread), so it becomes a script error and the instance starts over
        let (state, stack) = (&mut self.state, &mut self.stack);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut vm = Vm::new(prog, state, host, stack);
            vm.call_entry(prog.execute, read)
        }));
        let r = match r {
            Ok(r) => r,
            Err(panic) => {
                self.stack.clear();
                self.state.begin_document();
                return Err(ScriptError::internal(&prog.source, &*panic));
            }
        };
        // a recursion that could not report its overflow (map key hashing, …) fails the run
        let r = match crate::guard::take_pending() {
            Some(t) if r.is_ok() => Err(t),
            _ => r,
        };
        match r {
            Ok(v) => Ok(v),
            Err(t) => Err(script.runtime_error(&t)),
        }
    }
}

impl ScriptError {
    /// An error that is not a `script_exception` (size limits, circuit breakers): rendered as
    /// the plain exception.
    pub fn plain(kind: &str, reason: String) -> ScriptError {
        ScriptError {
            reason: reason.clone(),
            script_stack: Vec::new(),
            script: String::new(),
            lang: "painless",
            position: None,
            cause_type: kind.to_string(),
            cause_reason: Some(reason),
            cause_is_root: true,
            metadata: Vec::new(),
            status: 400,
            plain: true,
            cause_fields: Vec::new(),
        }
    }
    /// A panic inside the engine (a bug): reported as a script error with an
    /// `illegal_state_exception` cause instead of unwinding into the host.
    #[cold]
    pub fn internal(src: &str, panic: &(dyn std::any::Any + Send)) -> ScriptError {
        let msg = panic.downcast_ref::<String>().map(String::as_str).or_else(|| panic.downcast_ref::<&str>().copied()).unwrap_or("panic");
        ScriptError {
            reason: "runtime error".into(),
            script_stack: Vec::new(),
            script: src.to_string(),
            lang: "painless",
            position: None,
            cause_type: "illegal_state_exception".into(),
            cause_reason: Some(format!("internal error in the script engine: {msg}")),
            cause_is_root: false,
            metadata: Vec::new(),
            status: 500,
            plain: false,
            cause_fields: Vec::new(),
        }
    }
    pub fn with_status(mut self, status: u16) -> ScriptError {
        self.status = status;
        self
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn send_sync<T: Send + Sync>() {}

    #[test]
    fn shareable_across_threads() {
        send_sync::<Engine>();
        send_sync::<Script>();
        send_sync::<Compiled>();
    }

    #[test]
    fn cache_reuses_compiled_scripts() {
        let e = Engine::new(Settings { cache_max_size: 2, ..Settings::default() });
        let a = e.compile("return 1", Context::PainlessTest).unwrap();
        let b = e.compile("return 1", Context::PainlessTest).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // the same source in another context is another script
        let c = e.compile("return 1", Context::Score).unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(e.stats().compilations, 2);
        e.compile("return 2", Context::PainlessTest).unwrap();
        assert_eq!(e.stats().cache_evictions, 1);
    }

    #[test]
    fn compilation_rate_limit() {
        let e = Engine::new(Settings { max_compilations_rate: Some((2, Duration::from_secs(3600))), ..Settings::default() });
        e.compile("return 1", Context::PainlessTest).unwrap();
        e.compile("return 2", Context::PainlessTest).unwrap();
        let err = e.compile("return 3", Context::PainlessTest).unwrap_err();
        assert_eq!(err.status, 429);
        assert_eq!(err.to_response()["error"]["type"], "circuit_breaking_exception");
        assert_eq!(
            err.to_response()["error"]["reason"],
            "[script] Too many dynamic script compilations within, max: [2/1h]; please use indexed, or scripts with parameters instead; this limit can be changed by the [script.max_compilations_rate] setting"
        );
        // cached scripts and ingest scripts do not count
        e.compile("return 1", Context::PainlessTest).unwrap();
        e.compile("ctx.a = 4", Context::Ingest).unwrap();
        assert_eq!(e.stats().compilation_limit_triggered, 1);
    }

    #[test]
    fn inline_size_limit() {
        let e = Engine::new(Settings { max_size_in_bytes: 10, ..Settings::default() });
        let err = e.compile("return 12345678", Context::PainlessTest).unwrap_err();
        assert_eq!(
            err.to_response()["error"]["reason"],
            "exceeded max allowed inline script size in bytes [10] with size [15] for script [return 12345678]"
        );
        // stored scripts are checked when stored
        assert!(e.compile_stored(Lang::Painless, "return 12345678", Context::PainlessTest, &[]).is_ok());
    }

    #[test]
    fn instances_run_documents_without_rebinding() {
        let e = Engine::default();
        let s = e.compile("params.n += 1; return params.n * 2", Context::Score).unwrap();
        let mut inst = s.instance();
        inst.set_params(&serde_json::json!({"n": 0}));
        // params persist across documents of one instance, like Elasticsearch's per-leaf map
        assert_eq!(inst.execute_f64(&mut crate::NoHost).unwrap(), 2.0);
        assert_eq!(inst.execute_f64(&mut crate::NoHost).unwrap(), 4.0);
    }

    #[test]
    fn deep_recursion_reports_stack_overflow() {
        let e = Engine::default();
        let s = e.compile("int f(int n) { n == 0 ? 0 : 1 + f(n - 1) } return f(params.n)", Context::PainlessTest).unwrap();
        let mut ok = s.instance();
        ok.set_params(&serde_json::json!({"n": 5000}));
        assert_eq!(ok.execute(&mut crate::NoHost).unwrap().to_java_string(), "5000");
        let mut deep = s.instance();
        deep.set_params(&serde_json::json!({"n": 100000}));
        let err = deep.execute(&mut crate::NoHost).unwrap_err();
        assert_eq!(err.to_response()["error"]["caused_by"]["type"], "stack_overflow_error");
    }
}
