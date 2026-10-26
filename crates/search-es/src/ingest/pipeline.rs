//! Pipeline execution (`IngestService` / `Pipeline` / `CompoundProcessor`): compiled pipelines
//! (cached per cluster metadata version), processors with `if` conditions (Painless
//! `processor_conditional`), `ignore_failure`, `on_failure` handlers (with the
//! `_ingest.on_failure_*` metadata), nested `pipeline` processors with cycle detection, the
//! write path (request/default pipeline, then the final pipeline) and the `_simulate` API.

use super::Outcome;
use super::doc::IngestDoc;
use super::processors::{self, Flow, Kind};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::write::{Target, WriteArgs};
use bytes::Bytes;
use std::sync::Arc;

/// A compiled processor.
#[derive(Clone)]
pub struct Compiled {
    pub ptype: String,
    pub tag: Option<String>,
    pub description: Option<String>,
    pub cond: Option<crate::scripting::Bound>,
    pub ignore_failure: bool,
    pub on_failure: Vec<Compiled>,
    pub kind: Kind,
}

/// A compiled pipeline.
pub struct Pipeline {
    pub id: String,
    pub processors: Vec<Compiled>,
    pub on_failure: Vec<Compiled>,
}

/// Compiled pipelines of a cluster, valid for one metadata version.
#[derive(Default)]
pub struct Cache {
    map: parking_lot::Mutex<std::collections::HashMap<String, (u64, Arc<Pipeline>)>>,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ingest::Cache")
    }
}

/// Compiles a processor list.
pub fn compile_list(cluster: &Cluster, v: &Value) -> EsResult<Vec<Compiled>> {
    let Value::Arr(a) = v else { return Ok(Vec::new()) };
    let mut out = Vec::with_capacity(a.len());
    for item in a {
        let Value::Obj(m) = item else { continue };
        let Some((ptype, cfg)) = m.iter().next() else { continue };
        let cfg = match cfg {
            Value::Obj(c) => c.clone(),
            _ => Map::new(),
        };
        let tag = cfg.get("tag").map(|v| v.to_es_string());
        let cond = match cfg.get("if") {
            Some(c) => {
                let mut p = crate::query::XParser::from_value(c);
                p.next_token();
                let s = crate::query::script::Script::parse(&mut p)?;
                let st = cluster.state();
                Some(cluster.scripts.compile(&st, &s, search_painless::Context::ProcessorConditional)?)
            }
            None => None,
        };
        let on_failure = match cfg.get("on_failure") {
            Some(of) => compile_list(cluster, of)?,
            None => Vec::new(),
        };
        let kind = Kind::compile(cluster, ptype, &cfg, tag.as_deref())?;
        out.push(Compiled {
            ptype: ptype.clone(),
            tag,
            description: cfg.get("description").map(|v| v.to_es_string()),
            cond,
            ignore_failure: matches!(cfg.get("ignore_failure"), Some(Value::Bool(true))),
            on_failure,
            kind,
        });
    }
    Ok(out)
}

/// Compiles a pipeline definition.
pub fn compile(cluster: &Cluster, id: &str, def: &Value) -> EsResult<Pipeline> {
    Ok(Pipeline {
        id: id.to_string(),
        processors: compile_list(cluster, def.get("processors").unwrap_or(&Value::Null))?,
        on_failure: compile_list(cluster, def.get("on_failure").unwrap_or(&Value::Null))?,
    })
}

thread_local! {
    /// `POST _ingest/_simulate` `pipeline_substitutions`, while a simulation runs.
    static SUBSTITUTIONS: std::cell::RefCell<std::collections::HashMap<String, Arc<Pipeline>>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Runs `f` (synchronously) with `subs` standing in for the stored pipelines of their names.
pub fn with_substitutions<R>(subs: &std::collections::HashMap<String, Arc<Pipeline>>, f: impl FnOnce() -> R) -> R {
    SUBSTITUTIONS.with(|s| *s.borrow_mut() = subs.clone());
    let r = f();
    SUBSTITUTIONS.with(|s| s.borrow_mut().clear());
    r
}

/// The compiled pipeline `id` (`None` when it doesn't exist).
pub fn get(cluster: &Cluster, id: &str) -> EsResult<Option<Arc<Pipeline>>> {
    if let Some(p) = SUBSTITUTIONS.with(|s| s.borrow().get(id).cloned()) {
        return Ok(Some(p));
    }
    let st = cluster.state();
    let version = st.metadata.version;
    if let Some((v, p)) = cluster.ingest.map.lock().get(id)
        && *v == version
    {
        return Ok(Some(p.clone()));
    }
    let Some(def) = st.metadata.pipelines.get(id) else { return Ok(None) };
    let p = Arc::new(compile(cluster, id, def)?);
    cluster.ingest.map.lock().insert(id.to_string(), (version, p.clone()));
    Ok(Some(p))
}

fn cond_true(c: &crate::scripting::Bound, d: &IngestDoc) -> EsResult<bool> {
    let Some(sc) = c.painless() else { return Ok(true) };
    let ctx = painless_ctx(d)?;
    let mut inst = sc.instance();
    inst.set_params(&c.params);
    inst.set_var("ctx", search_painless::es::write::conditional_ctx(&ctx));
    inst.execute_bool(&mut search_painless::NoHost).map_err(crate::scripting::script_error)
}

fn painless_ctx(d: &IngestDoc) -> EsResult<search_painless::Value> {
    use search_painless::Value as PV;
    let src = search_painless::xcontent::parse_source_unordered(&Value::Obj(d.source.clone()).to_json()).map_err(|e| EsError::parse(format!("{e:?}")))?;
    let mut meta: Vec<(&str, PV)> = Vec::new();
    let s = |k: &str| d.meta.get(k).map(|v| PV::str(&v.to_es_string()));
    meta.push(("_index", s("_index").unwrap_or(PV::Null)));
    meta.push(("_id", s("_id").unwrap_or(PV::Null)));
    meta.push(("_version", match d.meta.get("_version") {
        Some(Value::Num(n)) => PV::Long(n.as_i64().unwrap_or(-3)),
        _ => PV::Long(-3),
    }));
    if let Some(r) = s("_routing") {
        meta.push(("_routing", r));
    }
    if let Some(v) = s("_version_type") {
        meta.push(("_version_type", v));
    }
    let now = d.ingest.get("timestamp").and_then(Value::as_str).and_then(|t| crate::time::DateFormatter::new("strict_date_optional_time").ok().and_then(|f| f.parse_millis(t).ok())).unwrap_or(0);
    search_painless::es::write::ctx_value(search_painless::es::write::CtxKind::Ingest, src, &meta, Some(search_painless::value::zoned_date_time_millis(now))).map_err(EsError::illegal_argument)
}

/// The `script` processor.
pub fn run_script(b: &crate::scripting::Bound, d: &mut IngestDoc) -> EsResult<()> {
    let Some(sc) = b.painless() else { return Err(EsError::illegal_argument("only painless scripts run in ingest")) };
    let ctx = painless_ctx(d)?;
    let mut inst = sc.instance();
    inst.set_params(&b.params);
    inst.set_var("ctx", ctx.clone());
    inst.execute(&mut search_painless::NoHost).map_err(crate::scripting::script_error)?;
    let c = search_painless::es::write::ctx_of(&ctx).ok_or_else(|| EsError::illegal_state("script lost its ctx"))?;
    let src = c.source();
    search_painless::xcontent::check_ingest_value(&src).map_err(|e| EsError::illegal_argument(e.reason.clone()))?;
    let json = search_painless::xcontent::to_xcontent(&src).map_err(|e| EsError::illegal_argument(e.reason.clone()))?;
    if let Ok(Some(Value::Obj(m))) = crate::json::parse(json.as_bytes()) {
        d.source = m;
    }
    for (k, v) in c.metadata() {
        if super::doc::METADATA.contains(&k.as_str()) {
            let jv = crate::scripting::from_serde(&v.to_json());
            if jv.is_null() {
                d.meta.shift_remove(&k);
            } else {
                d.meta.insert(k, jv);
            }
        }
    }
    Ok(())
}

/// Where a document's ingest failed (for its failure store document).
#[derive(Clone, Debug, Default)]
pub struct IngestFailure {
    /// The pipelines entered, outermost first.
    pub pipelines: Vec<String>,
    pub processor_type: String,
    pub processor_tag: Option<String>,
    /// The write's target when the failing pipeline started (`IndexRequest.indices()[0]`):
    /// whose failure store takes the document.
    pub index: Option<String>,
}

thread_local! {
    static LAST_FAILURE: std::cell::RefCell<Option<IngestFailure>> = const { std::cell::RefCell::new(None) };
}

/// The failure recorded by the last failed processor on this thread (taken).
pub fn take_failure() -> Option<IngestFailure> {
    LAST_FAILURE.with(|f| f.borrow_mut().take())
}

impl IngestFailure {
    fn to_value(&self, created: &[String]) -> Value {
        let mut m = Map::new();
        m.insert("pipelines".into(), Value::Arr(self.pipelines.iter().cloned().map(Value::Str).collect()));
        m.insert("processor_type".into(), Value::Str(self.processor_type.clone()));
        if let Some(t) = &self.processor_tag {
            m.insert("processor_tag".into(), Value::Str(t.clone()));
        }
        if let Some(i) = &self.index {
            m.insert("index".into(), Value::Str(i.clone()));
        }
        m.insert("created".into(), Value::Arr(created.iter().cloned().map(Value::Str).collect()));
        Value::Obj(m)
    }
}

/// Where the ingest failure behind a write error happened, and the data streams the write's
/// reroutes created (fresh failure stores start at their data stream's generation).
pub fn failure_of(e: &EsError) -> Option<(IngestFailure, Vec<String>)> {
    let v = e.hidden("ingest_failure")?;
    let strs = |k: &str| -> Vec<String> {
        match v.get(k) {
            Some(Value::Arr(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => Vec::new(),
        }
    };
    let f = IngestFailure {
        pipelines: strs("pipelines"),
        processor_type: v.get("processor_type").and_then(Value::as_str).unwrap_or_default().to_string(),
        processor_tag: v.get("processor_tag").and_then(Value::as_str).map(str::to_string),
        index: v.get("index").and_then(Value::as_str).map(str::to_string),
    };
    Some((f, strs("created")))
}

/// Records the target a failed pipeline started from (on the processor's failure, or a new
/// one for failures between pipelines).
fn record_failure_index(index: &str, fallback: impl FnOnce() -> IngestFailure) {
    LAST_FAILURE.with(|f| {
        let mut f = f.borrow_mut();
        let rec = f.get_or_insert_with(fallback);
        if rec.index.is_none() {
            rec.index = Some(index.to_string());
        }
    });
}

/// Processor failure as Elasticsearch reports it (the cause, with the processor's identity).
fn failure(p: &Compiled, e: EsError, d: &IngestDoc) -> EsError {
    if e.meta().iter().any(|(k, _)| k == "processor_type") {
        return e;
    }
    // The innermost failing processor is the one recorded (outer `pipeline` processors may
    // rewrap its error).
    LAST_FAILURE.with(|f| {
        let mut f = f.borrow_mut();
        if f.is_some() {
            return;
        }
        *f = Some(IngestFailure { pipelines: d.pipelines.clone(), processor_type: p.ptype.clone(), processor_tag: p.tag.clone(), index: None });
    });
    // A pipeline cycle surfaces as it is (`GraphStructureException`), without processor headers.
    if e.kind == "graph_structure_exception" {
        return e;
    }
    let mut e = e.with_header_values("processor_type", vec![p.ptype.clone()]);
    if let Some(t) = &p.tag {
        e = e.with_header_values("processor_tag", vec![t.clone()]);
    }
    e
}

const FAILURE_KEYS: [&str; 4] = ["on_failure_message", "on_failure_processor_type", "on_failure_processor_tag", "on_failure_pipeline"];

/// Puts back the `_ingest.on_failure_*` values that were there before a failure handler ran.
fn restore(d: &mut IngestDoc, saved: Vec<(String, Option<Value>)>) {
    for (k, v) in saved {
        match v {
            Some(v) => {
                d.ingest.insert(k, v);
            }
            None => {
                d.ingest.shift_remove(&k);
            }
        }
    }
}

/// `_ingest.on_failure_message`: the message of the failure's root cause.
fn root_message(e: &EsError) -> String {
    let mut cur = e;
    while let Some(c) = &cur.caused_by {
        cur = c;
    }
    cur.reason().to_string()
}

/// Runs one processor (condition, failure handling).
pub fn run_one(cluster: &Cluster, p: &Compiled, d: &mut IngestDoc, depth: usize) -> EsResult<Flow> {
    if let Some(c) = &p.cond
        && !cond_true(c, d)?
    {
        return Ok(Flow::Continue);
    }
    let r = processors::run(cluster, &p.kind, d, depth);
    d.track_size();
    // A self-referencing document is only detected once the pipeline is done.
    let r = match r {
        Err(e) if matches!(p.kind, Kind::Script { .. }) && e.reason().starts_with("Iterable object is self-referencing itself") => {
            if d.self_reference.is_none() {
                d.self_reference = Some(e);
            }
            Ok(Flow::Continue)
        }
        r => r,
    };
    match r {
        Ok(f) => Ok(f),
        Err(_) if p.ignore_failure => {
            // A handled failure is not the document's failure.
            let _ = take_failure();
            Ok(Flow::Continue)
        }
        Err(e) if !p.on_failure.is_empty() => {
            let _ = take_failure();
            let saved: Vec<(String, Option<Value>)> = FAILURE_KEYS.iter().map(|k| (k.to_string(), d.ingest.get(*k).cloned())).collect();
            d.ingest.insert("on_failure_message".into(), Value::Str(root_message(&e)));
            d.ingest.insert("on_failure_processor_type".into(), Value::Str(p.ptype.clone()));
            if let Some(t) = &p.tag {
                d.ingest.insert("on_failure_processor_tag".into(), Value::Str(t.clone()));
            }
            if let Some(pl) = d.pipelines.last() {
                d.ingest.insert("on_failure_pipeline".into(), Value::Str(pl.clone()));
            }
            let mut flow = Flow::Continue;
            for h in &p.on_failure {
                if let Flow::Drop = run_one(cluster, h, d, depth)? {
                    flow = Flow::Drop;
                    break;
                }
            }
            restore(d, saved);
            Ok(flow)
        }
        Err(e) => Err(failure(p, e, d)),
    }
}

/// Runs a pipeline over a document.
pub fn run_pipeline(cluster: &Cluster, pl: &Pipeline, d: &mut IngestDoc, depth: usize) -> EsResult<Flow> {
    if depth > 100 || d.pipelines.contains(&pl.id) {
        let mut chain = d.pipelines.clone();
        chain.push(pl.id.clone());
        return Err(EsError::new(500, "graph_structure_exception", format!("Cycle detected for pipeline: {}", pl.id)));
    }
    d.pipelines.push(pl.id.clone());
    d.ingest.insert("pipeline".into(), Value::Str(pl.id.clone()));
    let mut result = Ok(Flow::Continue);
    for p in &pl.processors {
        match run_one(cluster, p, d, depth) {
            Ok(Flow::Drop) => {
                result = Ok(Flow::Drop);
                break;
            }
            Ok(Flow::Continue) => {
                if matches!(p.kind, Kind::Terminate) || matches!(p.kind, Kind::Reroute { .. }) {
                    break;
                }
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    let mut saved_failure = None;
    if let Err(e) = &result
        && !pl.on_failure.is_empty()
    {
        let saved: Vec<(String, Option<Value>)> = FAILURE_KEYS.iter().map(|k| (k.to_string(), d.ingest.get(*k).cloned())).collect();
        d.ingest.insert("on_failure_message".into(), Value::Str(root_message(e)));
        let header = |name: &str| e.headers().iter().find(|(k, _)| k == name).and_then(|(_, v)| v.first().cloned());
        if let Some(t) = header("processor_type") {
            d.ingest.insert("on_failure_processor_type".into(), Value::Str(t));
        }
        if let Some(t) = header("processor_tag") {
            d.ingest.insert("on_failure_processor_tag".into(), Value::Str(t));
        }
        d.ingest.insert("on_failure_pipeline".into(), Value::Str(pl.id.clone()));
        saved_failure = Some(saved);
        let _ = take_failure();
        result = Ok(Flow::Continue);
        for h in &pl.on_failure {
            match run_one(cluster, h, d, depth) {
                Ok(Flow::Drop) => {
                    result = Ok(Flow::Drop);
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
    }
    if let Some(saved) = saved_failure {
        restore(d, saved);
    }
    d.pipelines.pop();
    match d.pipelines.last() {
        Some(prev) => {
            d.ingest.insert("pipeline".into(), Value::Str(prev.clone()));
        }
        None => {
            d.ingest.shift_remove("pipeline");
        }
    }
    result
}

/// The `pipeline` processor.
pub fn run_named(cluster: &Cluster, name: &str, d: &mut IngestDoc, depth: usize, ignore_missing: bool) -> EsResult<Flow> {
    match get(cluster, name)? {
        Some(p) => run_pipeline(cluster, &p, d, depth),
        None if ignore_missing => Ok(Flow::Continue),
        None => Err(EsError::illegal_argument(format!("Pipeline processor configured for non-existent pipeline [{name}]"))),
    }
}

/// Hashes for `fingerprint`.
pub fn hash(method: &str, data: &[u8]) -> Vec<u8> {
    use sha1::Digest;
    match method {
        "MD5" => md5(data).to_vec(),
        "SHA-256" => sha2::Sha256::digest(data).to_vec(),
        "SHA-512" => sha2::Sha512::digest(data).to_vec(),
        "MurmurHash3" => (crate::meta::routing::murmur3_x86_32(data, 0) as u32).to_be_bytes().to_vec(),
        _ => sha1::Sha1::digest(data).to_vec(),
    }
}

/// MD5 (RFC 1321).
fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f2 = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f2.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    out
}

fn meta_of(target: &Target, args: &WriteArgs, id: &str) -> Map {
    let mut meta = Map::new();
    meta.insert("_index".into(), Value::Str(target.data_stream.clone().unwrap_or_else(|| target.index.name.clone())));
    meta.insert("_id".into(), Value::Str(id.to_string()));
    if let Some(r) = &args.routing {
        meta.insert("_routing".into(), Value::Str(r.clone()));
    }
    meta.insert("_version".into(), Value::from(args.version.unwrap_or(-3)));
    if let Some(vt) = args.version_type {
        meta.insert(
            "_version_type".into(),
            Value::Str(match vt {
                search_index::VersionType::Internal => "internal",
                search_index::VersionType::External => "external",
                search_index::VersionType::ExternalGte => "external_gte",
            }
            .into()),
        );
    }
    meta
}

/// Runs the pipelines of a write and re-targets the document when a pipeline changed `_index`
/// (`IngestService.executePipelines`): each pipeline starts from the current target; when it
/// changes `_index`, a final pipeline fails the document, a target seen before is an index
/// cycle, and otherwise the new target's pipelines replace the remaining ones (its default
/// pipeline only after a `reroute`). A document a script made self-referencing fails when its
/// pipeline ends. Failures record the target their pipeline started from (whose failure store
/// takes the document).
#[allow(clippy::too_many_arguments)]
pub async fn run_write(cluster: &Cluster, target: Target, args: &WriteArgs, id: &str, value: Value, first: Option<String>, fin: Option<String>, now_ms: i64) -> EsResult<Outcome> {
    let Value::Obj(src) = value else { return Ok(Outcome::Index(target, Value::Obj(Map::new()), Bytes::new())) };
    // Enrich processors read in-memory tables of the enrich indices.
    crate::xpack::enrich::warm(cluster).await;
    let mut d = IngestDoc::new(src, meta_of(&target, args, id), now_ms);
    let doc_id = args.id.clone().unwrap_or_else(|| "null".to_string());
    let index_of = |d: &IngestDoc| match d.meta.get("_index") {
        Some(Value::Str(s)) => s.clone(),
        Some(other) => other.to_es_string(),
        None => String::new(),
    };
    let mut current = target;
    let mut history: Vec<String> = vec![index_of(&d)];
    let mut created: Vec<String> = Vec::new();
    let _ = take_failure();
    // `(pipeline, is_final)`, in execution order.
    let mut queue: std::collections::VecDeque<(String, bool)> = first.map(|p| (p, false)).into_iter().chain(fin.map(|f| (f, true))).collect();
    while let Some((pid, is_final)) = queue.pop_front() {
        let original = index_of(&d);
        // The failure travels with the error (the request may resume on another thread).
        let fail = |e: EsError, pipelines: Vec<String>| -> EsError {
            record_failure_index(&original, || IngestFailure { pipelines, processor_type: String::new(), processor_tag: None, index: None });
            match take_failure() {
                Some(f) => e.with_hidden("ingest_failure", f.to_value(&created)),
                None => e,
            }
        };
        d.reroute = None;
        let pl = match get(cluster, &pid)? {
            Some(pl) => pl,
            None => return Err(fail(EsError::illegal_argument(format!("pipeline with id [{pid}] does not exist")), vec![pid.clone()])),
        };
        match run_pipeline(cluster, &pl, &mut d, 0) {
            Ok(Flow::Drop) => return Ok(Outcome::Dropped(current.index.name.clone())),
            Ok(Flow::Continue) => {}
            Err(e) => return Err(fail(e, vec![pid.clone()])),
        }
        if let Some(r) = d.reroute.clone() {
            d.meta.insert("_index".into(), Value::Str(r));
        }
        if let Some(cause) = d.self_reference.take() {
            let e = EsError::illegal_argument(format!("Failed to generate the source document for ingest pipeline [{pid}] for document [{original}/{doc_id}]")).caused_by(cause);
            return Err(fail(e, vec![pid.clone()]));
        }
        let new_index = index_of(&d);
        if new_index == original {
            continue;
        }
        if is_final {
            let e = EsError::illegal_state(format!("final pipeline [{pid}] can't change the target index (from [{original}] to [{new_index}]) for document [{doc_id}]"));
            return Err(fail(e, vec![pid.clone()]));
        }
        if let Some(e) = crate::xpack::streams::check_reroute(&cluster.state(), &pid, &original, &new_index, &history) {
            return Err(fail(e, vec![pid.clone()]).with_hidden("item_index", Value::Str(new_index)));
        }
        if history.contains(&new_index) {
            let mut cycle = history.clone();
            cycle.push(new_index.clone());
            let e = EsError::illegal_state(format!("index cycle detected while processing pipeline [{pid}] for document [{doc_id}]: [{}]", cycle.join(", ")));
            return Err(fail(e, vec![pid.clone()]));
        }
        history.push(new_index.clone());
        let mut a = args.clone();
        a.pipeline = None;
        let existed = crate::write::resolve_existing(&cluster.state(), &new_index).is_some();
        current = crate::write::resolve_target(cluster, &new_index, &a, now_ms).await?;
        if !existed {
            created.push(new_index.clone());
        }
        queue.clear();
        let setting = |k: &str| current.index.settings.get(k).filter(|p| *p != "_none").map(str::to_string);
        // Changing `_index` without `reroute` skips the new target's default pipeline.
        if d.reroute.is_some()
            && let Some(p) = setting("index.default_pipeline")
        {
            queue.push_back((p, false));
        }
        if let Some(f) = setting("index.final_pipeline") {
            queue.push_back((f, true));
        }
    }
    // Elasticsearch re-serializes the ingest document's hash maps.
    // Elasticsearch re-parses those bytes, so a non-finite double comes back as a string.
    let mut doc = Value::Obj(d.ordered_source());
    crate::json::write::quote_non_finite(&mut doc);
    let source = Bytes::from(doc.to_json());
    Ok(Outcome::Index(current, doc, source))
}

/// `POST _ingest/pipeline/_simulate`, `POST _ingest/pipeline/{id}/_simulate`.
pub fn simulate(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.ok_or_else(|| EsError::parse("request body or source parameter is required"))?;
    let verbose = ctx.bool_param("verbose", false)?;
    let pipeline: Arc<Pipeline> = match ctx.path_param("id") {
        Some(id) => get(ctx.cluster, id)?.ok_or_else(|| EsError::illegal_argument(format!("pipeline [{id}] does not exist")))?,
        None => {
            let Some(def) = body.get("pipeline") else { return Err(EsError::parse("[pipeline] required property is missing").with_meta("property_name", "pipeline")) };
            if let Value::Obj(m) = def {
                crate::api::ingest::validate_pipeline("_simulate_pipeline", m)?;
            }
            Arc::new(compile(ctx.cluster, "_simulate_pipeline", def)?)
        }
    };
    let Some(Value::Arr(docs)) = body.get("docs") else { return Err(EsError::parse("[docs] required property is missing").with_meta("property_name", "docs")) };
    if docs.is_empty() {
        return Err(EsError::parse("must specify at least one document in [docs]"));
    }
    let mut out = Vec::new();
    for dv in docs {
        let Value::Obj(dm) = dv else { return Err(EsError::parse("malformed document")) };
        let src = match dm.get("_source") {
            Some(Value::Obj(s)) => s.clone(),
            _ => return Err(EsError::parse("[_source] required property is missing").with_meta("property_name", "_source")),
        };
        let mut meta = Map::new();
        meta.insert("_index".into(), dm.get("_index").cloned().unwrap_or(Value::Str("_index".into())));
        meta.insert("_version".into(), dm.get("_version").cloned().unwrap_or(Value::from(-3i64)));
        meta.insert("_id".into(), dm.get("_id").cloned().unwrap_or(Value::Str("_id".into())));
        for k in ["_routing", "_version_type", "_if_seq_no", "_if_primary_term"] {
            if let Some(v) = dm.get(k) {
                meta.insert(k.into(), v.clone());
            }
        }
        let mut d = IngestDoc::new(src, meta, ctx.now_ms);
        if verbose {
            let mut results = Vec::new();
            d.pipelines.push(pipeline.id.clone());
            d.ingest.insert("pipeline".into(), Value::Str(pipeline.id.clone()));
            for p in &pipeline.processors {
                let mut r = Map::new();
                r.insert("processor_type".into(), Value::Str(p.ptype.clone()));
                match run_one(ctx.cluster, p, &mut d, 0) {
                    Ok(Flow::Drop) => {
                        r.insert("status".into(), Value::Str("dropped".into()));
                        results.push(Value::Obj(r));
                        break;
                    }
                    // A self-referencing document can't be rendered: every processor from
                    // the one that made it reports the serialization failure instead.
                    Ok(_) if d.self_reference.is_some() => {
                        r.insert("status".into(), Value::Str("error".into()));
                        if let Some(t) = &p.tag {
                            r.insert("tag".into(), Value::Str(t.clone()));
                        }
                        let e = d.self_reference.clone().unwrap_or_else(|| EsError::illegal_argument("self-referencing document"));
                        r.insert("error".into(), e.to_value_with_root(false));
                        results.push(Value::Obj(r));
                    }
                    Ok(_) => {
                        r.insert("status".into(), Value::Str("success".into()));
                        if let Some(t) = &p.tag {
                            r.insert("tag".into(), Value::Str(t.clone()));
                        }
                        r.insert("doc".into(), d.to_simulate(true));
                        results.push(Value::Obj(r));
                    }
                    Err(e) => {
                        r.insert("status".into(), Value::Str("error".into()));
                        if let Some(t) = &p.tag {
                            r.insert("tag".into(), Value::Str(t.clone()));
                        }
                        r.insert("error".into(), e.without_headers(&["processor_type", "processor_tag"]).to_value_with_root(false));
                        results.push(Value::Obj(r));
                        break;
                    }
                }
            }
            out.push(obj! { "processor_results" => Value::Arr(results) });
        } else {
            match run_pipeline(ctx.cluster, &pipeline, &mut d, 0) {
                Ok(Flow::Drop) => out.push(Value::Null),
                // The document can't be serialized (`IngestDocument` → `WriteableIngestDocument`).
                Ok(_) if d.self_reference.is_some() => {
                    let e = d.self_reference.take().unwrap_or_else(|| EsError::illegal_argument("self-referencing document"));
                    out.push(obj! { "error" => e.to_value_with_root(false) })
                }
                Ok(_) => out.push(obj! { "doc" => d.to_simulate(false) }),
                Err(e) => out.push(obj! { "error" => e.without_headers(&["processor_type", "processor_tag"]).to_value_with_root(false) }),
            }
        }
    }
    Reply::ok(obj! { "docs" => Value::Arr(out) })
}
