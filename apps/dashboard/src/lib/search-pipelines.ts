/**
 * Ingest pipelines, as Kibana's "Ingest Pipelines" edits them: a pipeline's processors as a form
 * (type, the type's options, `if`, `tag`, `ignore_failure`, `on_failure`) that round-trips to the
 * JSON Elasticsearch stores, `_ingest/pipeline/_simulate?verbose` on sample documents read back
 * per processor, and the indices whose `default_pipeline` names a pipeline. Pure logic for
 * `components/search/pipelines.tsx`.
 */

// --- Processors --------------------------------------------------------------------------------

/** One processor as the form holds it. `options` are the type's own settings, kept as given. */
export interface Processor {
  type: string;
  options: Record<string, unknown>;
  if?: string;
  tag?: string;
  description?: string;
  ignoreFailure?: boolean;
  onFailure?: Processor[];
}

/** The settings every processor takes; everything else is the type's own. */
const COMMON_KEYS = ["if", "tag", "description", "ignore_failure", "on_failure"];

/** A processor's JSON (`{"set": {...}}`) as the form holds it. */
export function processorFromJson(json: unknown): Processor {
  if (!json || typeof json !== "object" || Array.isArray(json)) throw new Error("A processor is an object naming its type: {\"set\": {…}}.");
  const entries = Object.entries(json as Record<string, unknown>);
  if (entries.length !== 1) throw new Error("A processor names exactly one type.");
  const [type, body] = entries[0];
  // A `script` processor may be given as just its source (`{"script": "ctx.a = 1"}`).
  const b = typeof body === "string" && type === "script" ? { source: body } : body;
  if (!b || typeof b !== "object" || Array.isArray(b)) throw new Error(`[${type}]'s settings must be an object.`);
  const all = b as Record<string, unknown>;
  const options: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(all)) if (!COMMON_KEYS.includes(k)) options[k] = v;
  const p: Processor = { type, options };
  if (typeof all.if === "string") p.if = all.if;
  else if (all.if !== undefined) options.if = all.if; // a script object: kept verbatim
  if (typeof all.tag === "string") p.tag = all.tag;
  if (typeof all.description === "string") p.description = all.description;
  if (all.ignore_failure === true) p.ignoreFailure = true;
  if (Array.isArray(all.on_failure)) p.onFailure = all.on_failure.map(processorFromJson);
  return p;
}

/** The form's processor as Elasticsearch stores it: its options, then the common settings. */
export function processorToJson(p: Processor): Record<string, unknown> {
  const body: Record<string, unknown> = { ...p.options };
  if (p.description) body.description = p.description;
  if (p.if?.trim()) body.if = p.if.trim();
  if (p.tag?.trim()) body.tag = p.tag.trim();
  if (p.ignoreFailure) body.ignore_failure = true;
  if (p.onFailure?.length) body.on_failure = p.onFailure.map(processorToJson);
  return { [p.type]: body };
}

/** A pipeline as `PUT _ingest/pipeline/<id>` takes it; `rest` keeps `version`, `_meta` and the like. */
export interface Pipeline {
  description: string;
  processors: Processor[];
  onFailure: Processor[];
  rest: Record<string, unknown>;
}

export function pipelineFromJson(json: unknown): Pipeline {
  if (!json || typeof json !== "object" || Array.isArray(json)) throw new Error("A pipeline is an object with a processors list.");
  // The dates the cluster keeps (`GET` gives them; `PUT` refuses them) aren't the pipeline's.
  const { description, processors, on_failure, created_date, created_date_millis, modified_date, modified_date_millis, ...rest } = json as Record<string, unknown>;
  if (processors !== undefined && !Array.isArray(processors)) throw new Error("processors must be a list.");
  if (on_failure !== undefined && !Array.isArray(on_failure)) throw new Error("on_failure must be a list.");
  return {
    description: typeof description === "string" ? description : "",
    processors: (processors ?? []).map(processorFromJson),
    onFailure: (on_failure ?? []).map(processorFromJson),
    rest,
  };
}

export function pipelineToJson(p: Pipeline): Record<string, unknown> {
  return {
    ...(p.description ? { description: p.description } : {}),
    processors: p.processors.map(processorToJson),
    ...(p.onFailure.length ? { on_failure: p.onFailure.map(processorToJson) } : {}),
    ...p.rest,
  };
}

// --- The form of each processor type -------------------------------------------------------------

/** One option of a processor type, as the form edits it. */
export interface OptionSpec {
  key: string;
  label?: string;
  /** `list`: one value per line (an array). `json`: any JSON value. */
  kind: "text" | "list" | "bool" | "select" | "json" | "code";
  required?: boolean;
  choices?: string[];
  placeholder?: string;
}

const field = (required = true): OptionSpec => ({ key: "field", kind: "text", required, placeholder: "message" });
const target: OptionSpec = { key: "target_field", kind: "text", placeholder: "same field" };
const missing: OptionSpec = { key: "ignore_missing", label: "Ignore missing", kind: "bool" };

/**
 * The options the form shows for the processor types people use most (Kibana's processor forms).
 * Another type's options are edited as JSON; either way, options the form doesn't list are kept.
 */
export const PROCESSOR_FORMS: Record<string, OptionSpec[]> = {
  set: [field(), { key: "value", kind: "json", placeholder: '"a value" or {{other_field}}' }, { key: "copy_from", kind: "text" }, { key: "override", kind: "bool" }],
  remove: [{ key: "field", kind: "list", required: true }, missing],
  rename: [field(), { ...target, required: true, placeholder: "new name" }, missing],
  append: [field(), { key: "value", kind: "list", required: true }, { key: "allow_duplicates", kind: "bool" }],
  convert: [field(), { key: "type", kind: "select", required: true, choices: ["integer", "long", "float", "double", "boolean", "string", "ip", "auto"] }, target, missing],
  date: [field(), { key: "formats", kind: "list", required: true, placeholder: "ISO8601" }, { ...target, placeholder: "@timestamp" }, { key: "timezone", kind: "text", placeholder: "UTC" }],
  grok: [field(), { key: "patterns", kind: "list", required: true, placeholder: "%{IP:client.ip} %{WORD:http.method}" }, { key: "pattern_definitions", kind: "json" }, missing],
  dissect: [field(), { key: "pattern", kind: "text", required: true, placeholder: "%{client.ip} %{http.method}" }, missing],
  gsub: [field(), { key: "pattern", kind: "text", required: true }, { key: "replacement", kind: "text" }, target, missing],
  split: [field(), { key: "separator", kind: "text", required: true, placeholder: "," }, target, missing],
  join: [field(), { key: "separator", kind: "text", required: true }, target],
  json: [field(), target, { key: "add_to_root", kind: "bool" }],
  kv: [field(), { key: "field_split", kind: "text", required: true, placeholder: "&" }, { key: "value_split", kind: "text", required: true, placeholder: "=" }, target, missing],
  lowercase: [field(), target, missing],
  uppercase: [field(), target, missing],
  trim: [field(), target, missing],
  html_strip: [field(), target, missing],
  urldecode: [field(), target, missing],
  user_agent: [field(), target, missing],
  geoip: [field(), { ...target, placeholder: "geoip" }, missing],
  csv: [field(), { key: "target_fields", kind: "list", required: true }, { key: "separator", kind: "text", placeholder: "," }],
  dot_expander: [{ ...field(), placeholder: "* for every field" }],
  script: [{ key: "lang", kind: "select", choices: ["painless", "mustache"] }, { key: "source", kind: "code", required: true, placeholder: "ctx.total = ctx.price * ctx.quantity" }, { key: "params", kind: "json" }],
  pipeline: [{ key: "name", kind: "text", required: true, placeholder: "another pipeline" }],
  fail: [{ key: "message", kind: "text", required: true }],
  drop: [],
};

/** A value as its option's form field shows it. */
export function optionText(spec: OptionSpec, v: unknown): string | boolean {
  if (spec.kind === "bool") return v === true;
  if (v === undefined) return "";
  if (spec.kind === "list") return Array.isArray(v) ? v.map(String).join("\n") : String(v);
  if (spec.kind === "json") return typeof v === "string" ? JSON.stringify(v) : JSON.stringify(v, null, 2);
  return typeof v === "string" ? v : JSON.stringify(v);
}

/**
 * The option a form field's text stands for (`undefined`: left out). A `json` field that isn't
 * JSON is taken as a string (`hello` and `"hello"` are both the string).
 */
export function optionValue(spec: OptionSpec, text: string | boolean): unknown {
  if (spec.kind === "bool") return text === true ? true : undefined;
  const t = String(text);
  if (!t.trim()) return undefined;
  if (spec.kind === "list") {
    const items = t.split("\n").map((s) => s.trim()).filter(Boolean);
    // `remove` takes a field or a list; one stays a string, as people write it.
    return spec.key === "field" && items.length === 1 ? items[0] : items;
  }
  if (spec.kind === "json") {
    try {
      return JSON.parse(t);
    } catch {
      return t;
    }
  }
  return t;
}

/** What's missing before a processor (and its failure processors) can be saved, or null. */
export function processorProblem(p: Processor): string | null {
  if (!p.type) return "Pick a type.";
  for (const s of PROCESSOR_FORMS[p.type] ?? []) {
    if (s.required && (p.options[s.key] === undefined || p.options[s.key] === "")) return `[${p.type}] needs ${s.label ?? s.key}.`;
  }
  for (const f of p.onFailure ?? []) {
    const problem = processorProblem(f);
    if (problem) return problem;
  }
  return null;
}

/** A processor in a few words for its row: `set message = "hi"`, `rename a → b`. */
export function processorSummary(p: Processor): string {
  const o = p.options;
  const s = (v: unknown) => (typeof v === "string" ? v : JSON.stringify(v));
  const f = o.field !== undefined ? s(o.field) : "";
  switch (p.type) {
    case "set":
      return o.copy_from !== undefined ? `${f} = ${s(o.copy_from)}` : `${f} = ${JSON.stringify(o.value)}`;
    case "rename":
      return `${f} → ${s(o.target_field ?? "")}`;
    case "convert":
      return `${f} to ${s(o.type ?? "")}`;
    case "grok":
    case "date":
      return `${f}${Array.isArray(o.patterns ?? o.formats) ? ` · ${(o.patterns ?? o.formats) as string[]}`.slice(0, 80) : ""}`;
    case "script":
      return s(o.source ?? o.id ?? "").replace(/\s+/g, " ").slice(0, 80);
    case "pipeline":
      return s(o.name ?? "");
    case "fail":
      return s(o.message ?? "");
    default:
      return f + (o.target_field !== undefined ? ` → ${s(o.target_field)}` : "");
  }
}

// --- Simulate ----------------------------------------------------------------------------------

/**
 * Documents to test with, from what was typed: a JSON array (or one object), each a document
 * (`{"_source": {...}}`, with `_index`/`_id` if wanted) or just its source.
 */
export function sampleDocs(text: string): { docs: Record<string, unknown>[] } | { error: string } {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch (e) {
    return { error: `The documents aren't JSON: ${e instanceof Error ? e.message : String(e)}` };
  }
  const list = Array.isArray(v) ? v : [v];
  if (list.length === 0) return { error: "Add at least one document." };
  const docs: Record<string, unknown>[] = [];
  for (const d of list) {
    if (!d || typeof d !== "object" || Array.isArray(d)) return { error: "Each document is a JSON object." };
    const o = d as Record<string, unknown>;
    docs.push(o._source && typeof o._source === "object" ? o : { _source: o });
  }
  return { docs };
}

/** Search hits as simulate documents (index, id and source). */
export function hitsAsDocs(hits: { _index: string; _id: string; _source?: unknown }[]): Record<string, unknown>[] {
  return hits.map((h) => ({ _index: h._index, _id: h._id, _source: h._source ?? {} }));
}

/**
 * The processors tagged for a verbose simulate, so each result maps back to the processor that
 * made it (as Kibana does): an untagged processor gets its path (`0`, `1.on_failure.0`, and
 * `on_failure.0` for the pipeline's), and `paths` maps every tag to the path.
 */
export function taggedForSimulate(p: Pipeline): { body: Record<string, unknown>; paths: Record<string, string> } {
  const paths: Record<string, string> = {};
  const tag = (list: Processor[], prefix: string): Processor[] =>
    list.map((proc, i) => {
      const path = `${prefix}${i}`;
      const t = proc.tag?.trim() || `__roll_${path}`;
      paths[t] = path;
      return { ...proc, tag: t, onFailure: proc.onFailure && tag(proc.onFailure, `${path}.on_failure.`) };
    });
  const body = pipelineToJson({ ...p, processors: tag(p.processors, ""), onFailure: tag(p.onFailure, "on_failure.") });
  return { body, paths };
}

export type StepStatus = "success" | "error" | "error_ignored" | "skipped" | "dropped";

/** One processor's turn on one document. */
export interface SimStep {
  /** The processor's path in the pipeline (`taggedForSimulate`), or null for one it didn't tag. */
  path: string | null;
  type: string;
  status: StepStatus;
  /** Fields this step added, changed or removed (dotted names), against the step before. */
  changed: string[];
  error?: string;
  /** The document after it (absent when it failed or was skipped). */
  source?: unknown;
}

export interface SimDoc {
  /** The source given. */
  input: unknown;
  steps: SimStep[];
  /** The document after the pipeline, null when a processor dropped it. */
  output: unknown;
  /** The pipeline failed on it (an error no `on_failure` handled). */
  error?: string;
}

interface VerboseResult {
  processor_type?: string;
  status?: StepStatus;
  tag?: string;
  doc?: { _source?: unknown };
  error?: { reason?: string; type?: string };
  ignored_error?: { error?: { reason?: string } };
}

/** Dotted names whose value differs between two sources (fields added, changed or removed). */
export function changedFields(before: unknown, after: unknown, prefix = ""): string[] {
  const isObj = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);
  if (!isObj(before) || !isObj(after)) return JSON.stringify(before) === JSON.stringify(after) ? [] : [prefix.replace(/\.$/, "") || "."];
  const out: string[] = [];
  for (const k of new Set([...Object.keys(before), ...Object.keys(after)])) {
    const b = before[k];
    const a = after[k];
    if (isObj(b) && isObj(a)) out.push(...changedFields(b, a, `${prefix}${k}.`));
    else if (JSON.stringify(b) !== JSON.stringify(a)) out.push(`${prefix}${k}`);
  }
  return out;
}

/** A verbose `_simulate` answer, per document and per processor, with what each one changed. */
export function simulateResults(docs: Record<string, unknown>[], r: { docs?: { processor_results?: VerboseResult[]; error?: { reason?: string } }[] }, paths: Record<string, string>): SimDoc[] {
  return (r.docs ?? []).map((d, i) => {
    const input = docs[i]?._source ?? {};
    let prev: unknown = input;
    let output: unknown = input;
    let error: string | undefined = d.error?.reason;
    const steps: SimStep[] = (d.processor_results ?? []).map((s) => {
      const status = s.status ?? "success";
      const source = s.doc?._source;
      const step: SimStep = { path: (s.tag && paths[s.tag]) ?? null, type: s.processor_type ?? "?", status, changed: source !== undefined ? changedFields(prev, source) : [] };
      if (s.error) step.error = s.error.reason ?? s.error.type;
      if (s.ignored_error?.error?.reason) step.error = s.ignored_error.error.reason;
      if (source !== undefined) {
        step.source = source;
        prev = source;
        output = source;
      }
      if (status === "dropped") output = null;
      return step;
    });
    // A failure no `on_failure` handled is the pipeline's last step: the document isn't indexed.
    const last = steps[steps.length - 1];
    if (!error && last?.status === "error") error = last.error;
    return { input, steps, output: error ? undefined : output, error };
  });
}

// --- Indices' pipelines ------------------------------------------------------------------------

/** Each pipeline's indices, from every index's `index.default_pipeline` and `index.final_pipeline`. */
export function pipelineUsers(settings: Record<string, { settings?: { index?: { default_pipeline?: string; final_pipeline?: string } } }>): Record<string, { index: string; as: "default" | "final" }[]> {
  const out: Record<string, { index: string; as: "default" | "final" }[]> = {};
  for (const [index, s] of Object.entries(settings)) {
    const i = s.settings?.index;
    if (i?.default_pipeline && i.default_pipeline !== "_none") (out[i.default_pipeline] ??= []).push({ index, as: "default" });
    if (i?.final_pipeline && i.final_pipeline !== "_none") (out[i.final_pipeline] ??= []).push({ index, as: "final" });
  }
  for (const list of Object.values(out)) list.sort((a, b) => a.index.localeCompare(b.index));
  return out;
}

/** Same rule as Elasticsearch's pipeline ids: anything but blank or whitespace. */
export function pipelineIdProblem(id: string, existing: string[]): string | null {
  if (!id.trim()) return "Name the pipeline.";
  if (/\s/.test(id)) return "A pipeline's name has no spaces.";
  if (existing.includes(id)) return `${id} exists already.`;
  return null;
}
