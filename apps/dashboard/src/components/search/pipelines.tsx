import { ArrowDown, ArrowUp, ChevronRight, FlaskConical, Plus, SquareTerminal, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { esJson, indexPath, searchApi, type SearchCluster } from "@/lib/search-api";
import {
  hitsAsDocs,
  optionText,
  optionValue,
  PROCESSOR_FORMS,
  pipelineFromJson,
  pipelineIdProblem,
  pipelineToJson,
  pipelineUsers,
  processorProblem,
  processorSummary,
  sampleDocs,
  simulateResults,
  taggedForSimulate,
  type OptionSpec,
  type Pipeline,
  type Processor,
  type SimDoc,
  type SimStep,
} from "@/lib/search-pipelines";
import { Field, Segmented } from "../redis/shared";
import { btn, ErrorNote, input, Panel, primary } from "../ui";
import { JsonText } from "./json-view";
import { consoleText, RequestPreview } from "./request-preview";

const enc = encodeURIComponent;

type Users = ReturnType<typeof pipelineUsers>;

/**
 * Ingest pipelines (Kibana's "Ingest Pipelines"): every pipeline with what it does and the indices
 * that run it; a pipeline edited as a list of processors (type, options, `if`, `on_failure`) or as
 * JSON, tested on sample documents or documents from an index with a verbose `_simulate` read
 * back per processor, and set as an index's `default_pipeline`.
 */
export function PipelinesTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [pipelines, setPipelines] = useState<Record<string, Record<string, unknown>> | null>(null);
  const [users, setUsers] = useState<Users>({});
  const [types, setTypes] = useState<string[]>([]);
  const [indices, setIndices] = useState<string[]>([]);
  const [builtIn, setBuiltIn] = useState(false);
  const [editing, setEditing] = useState<{ id: string | null; json: Record<string, unknown> } | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [p, s, rows] = await Promise.all([
        esJson<Record<string, Record<string, unknown>>>(cluster.id, "GET", "/_ingest/pipeline"),
        esJson<Parameters<typeof pipelineUsers>[0]>(cluster.id, "GET", "/*/_settings/index.default_pipeline,index.final_pipeline").catch(() => ({})),
        searchApi.indices(cluster.id).catch(() => []),
      ]);
      setPipelines(p);
      setUsers(pipelineUsers(s));
      setIndices(rows.filter((r) => !r.index.startsWith(".")).map((r) => r.index).sort());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setPipelines((p) => p ?? {});
    }
  }, [cluster.id]);

  useEffect(() => {
    load();
    // The processor types this cluster's nodes run (`GET _nodes/ingest`).
    esJson<{ nodes: Record<string, { ingest?: { processors?: { type: string }[] } }> }>(cluster.id, "GET", "/_nodes/ingest?filter_path=nodes.*.ingest.processors").then(
      (r) => setTypes([...new Set(Object.values(r.nodes ?? {}).flatMap((n) => n.ingest?.processors?.map((p) => p.type) ?? []))].sort()),
      () => setTypes(Object.keys(PROCESSOR_FORMS).sort()),
    );
  }, [cluster.id, load]);

  if (!pipelines) return <p className="text-[13px] text-subtle"><span className="shimmer">Loading pipelines…</span></p>;
  const ids = Object.keys(pipelines).sort();
  const managed = (id: string) => (pipelines[id]._meta as { managed?: boolean } | undefined)?.managed === true;
  const own = ids.filter((id) => !managed(id));
  const shown = builtIn ? ids : own;
  const remove = async (id: string) => {
    setBusy(id);
    setError(null);
    try {
      await esJson(cluster.id, "DELETE", `/_ingest/pipeline/${enc(id)}`);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="space-y-6">
      <Panel
        title="Ingest pipelines"
        aside={
          <span className="flex items-center gap-2">
            {ids.length > own.length && (
              <button onClick={() => setBuiltIn((b) => !b)} className={btn}>
                {builtIn ? "Hide built-in" : `Show ${ids.length - own.length} built-in`}
              </button>
            )}
            {!editing && (
              <button onClick={() => setEditing({ id: null, json: { description: "", processors: [] } })} className={btn}>
                <Plus className="size-3.5" /> New pipeline
              </button>
            )}
          </span>
        }
      >
        {shown.length === 0 ? (
          <p className="px-5 py-4 text-[13px] text-muted">No pipelines of your own. A pipeline changes documents before they&apos;re indexed: parse a log line, rename fields, drop what isn&apos;t needed.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Pipeline</th>
                  <th className="px-3 py-2 text-right font-normal">Processors</th>
                  <th className="px-3 py-2 font-normal">Used by</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {shown.map((id) => {
                  const p = pipelines[id];
                  const n = Array.isArray(p.processors) ? p.processors.length : 0;
                  const used = users[id] ?? [];
                  return (
                    <tr key={id} className="border-b border-line align-top last:border-0">
                      <td className="max-w-96 px-5 py-2.5">
                        <button onClick={() => setEditing({ id, json: p })} className="block max-w-full truncate text-left font-mono text-[12px] hover:underline">
                          {id}
                          {managed(id) && <span className="ml-2 font-sans text-[11px] text-subtle">built-in</span>}
                        </button>
                        {typeof p.description === "string" && p.description && <div className="line-clamp-2 text-[12px] text-muted">{p.description}</div>}
                      </td>
                      <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{n}</td>
                      <td className="max-w-72 px-3 py-2.5 font-mono text-[12px] text-muted">
                        {used.length === 0 ? <span className="font-sans text-subtle">—</span> : used.slice(0, 3).map((u) => `${u.index}${u.as === "final" ? " (final)" : ""}`).join(", ") + (used.length > 3 ? ` +${used.length - 3}` : "")}
                      </td>
                      <td className="px-5 py-2 text-right whitespace-nowrap">
                        <span className="inline-flex items-center gap-1">
                          <button onClick={() => onOpenConsole(`GET /_ingest/pipeline/${enc(id)}`)} className="rounded-md p-1 text-subtle transition hover:text-fg" title="Open in console" aria-label="Open in console">
                            <SquareTerminal className="size-3.5" />
                          </button>
                          {!managed(id) && <DeleteButton label={id} busy={busy === id} onDelete={() => remove(id)} />}
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {error && (
          <div className="px-5 pb-4">
            <ErrorNote error={error} />
          </div>
        )}
      </Panel>
      {editing && (
        <PipelineEditor
          key={editing.id ?? "new"}
          cluster={cluster}
          id={editing.id}
          json={editing.json}
          taken={ids}
          types={types}
          indices={indices}
          users={editing.id ? (users[editing.id] ?? []) : []}
          onClose={() => setEditing(null)}
          onSaved={async (id, json) => {
            setEditing({ id, json });
            await load();
          }}
          onOpenConsole={onOpenConsole}
        />
      )}
    </div>
  );
}

function DeleteButton({ label, busy, onDelete }: { label: string; busy: boolean; onDelete: () => void }) {
  const [armed, setArmed] = useState(false);
  if (busy) return <span className="shimmer text-[12px] text-muted">Deleting…</span>;
  return armed ? (
    <button onClick={onDelete} onBlur={() => setArmed(false)} autoFocus className="rounded-md px-1.5 py-0.5 text-[12px] text-danger transition hover:bg-danger/10">
      Delete?
    </button>
  ) : (
    <button onClick={() => setArmed(true)} className="rounded-md p-1 text-subtle transition hover:text-danger" title={`Delete ${label}`} aria-label={`Delete ${label}`}>
      <Trash2 className="size-3.5" />
    </button>
  );
}

// --- The editor --------------------------------------------------------------------------------

function PipelineEditor({
  cluster,
  id,
  json,
  taken,
  types,
  indices,
  users,
  onClose,
  onSaved,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  id: string | null;
  json: Record<string, unknown>;
  taken: string[];
  types: string[];
  indices: string[];
  users: { index: string; as: "default" | "final" }[];
  onClose: () => void;
  onSaved: (id: string, json: Record<string, unknown>) => Promise<void>;
  onOpenConsole: (text: string) => void;
}) {
  const [name, setName] = useState(id ?? "");
  const [pipeline, setPipeline] = useState<Pipeline | null>(() => {
    try {
      return pipelineFromJson(json);
    } catch {
      return null;
    }
  });
  const [view, setView] = useState<"form" | "json">(pipeline ? "form" : "json");
  const [jsonText, setJsonText] = useState(() => JSON.stringify(json, null, 2));
  const [jsonError, setJsonError] = useState<string | null>(pipeline ? null : "This pipeline can't be shown as a form.");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [steps, setSteps] = useState<SimStep[] | null>(null);

  const body = useMemo(() => (pipeline ? pipelineToJson(pipeline) : null), [pipeline]);
  const switchView = (v: "form" | "json") => {
    if (v === "json" && body) setJsonText(JSON.stringify(body, null, 2));
    if (v === "form" && jsonError) return;
    setView(v);
  };
  const editJson = (text: string) => {
    setJsonText(text);
    try {
      setPipeline(pipelineFromJson(JSON.parse(text)));
      setJsonError(null);
    } catch (e) {
      setJsonError(e instanceof Error ? e.message : String(e));
    }
  };

  const trimmed = name.trim();
  const problem = (id ? null : pipelineIdProblem(trimmed, taken)) ?? jsonError ?? (pipeline ? [...pipeline.processors, ...pipeline.onFailure].map(processorProblem).find(Boolean) ?? null : null);
  const path = `/_ingest/pipeline/${enc(trimmed || "my-pipeline")}`;

  return (
    <Panel
      title={id ? <span className="font-mono">{id}</span> : "New pipeline"}
      className="rise"
      aside={
        <span className="flex items-center gap-2">
          <Segmented
            value={view}
            options={[
              { label: "processors", value: "form" },
              { label: "json", value: "json" },
            ]}
            onChange={switchView}
          />
          <button onClick={onClose} className={btn} aria-label="Close pipeline">
            <X className="size-3.5" />
          </button>
        </span>
      }
    >
      <form
        className="space-y-5 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!body) return;
          setBusy(true);
          setError(null);
          try {
            await esJson(cluster.id, "PUT", path, body);
            await onSaved(trimmed, body);
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="grid gap-3 sm:grid-cols-[minmax(0,16rem)_minmax(0,1fr)]">
          <Field label="Name">
            <input value={name} onChange={(e) => setName(e.target.value)} disabled={!!id} placeholder="web-logs" className={`${input} w-full font-mono disabled:opacity-60`} spellCheck={false} autoFocus={!id} />
          </Field>
          {view === "form" && pipeline && (
            <Field label="Description">
              <input value={pipeline.description} onChange={(e) => setPipeline({ ...pipeline, description: e.target.value })} placeholder="What it does" className={`${input} w-full`} />
            </Field>
          )}
        </div>

        {view === "json" ? (
          <div className="space-y-2">
            <textarea value={jsonText} onChange={(e) => editJson(e.target.value)} rows={18} spellCheck={false} aria-label="Pipeline JSON" className={`${input} w-full resize-y font-mono text-[12px] leading-relaxed`} />
            {jsonError && <p className="text-[12px] text-danger">{jsonError}</p>}
          </div>
        ) : (
          pipeline && (
            <>
              <ProcessorList title="Processors" list={pipeline.processors} onChange={(processors) => setPipeline({ ...pipeline, processors })} types={types} steps={steps} prefix="" />
              <ProcessorList
                title="Failure processors"
                hint="run when a processor fails and has no on_failure of its own"
                list={pipeline.onFailure}
                onChange={(onFailure) => setPipeline({ ...pipeline, onFailure })}
                types={types}
                steps={steps}
                prefix="on_failure."
              />
            </>
          )
        )}

        {body && <RequestPreview method="PUT" path={path} body={body} onOpenConsole={onOpenConsole} />}
        <div className="flex flex-wrap items-center gap-3">
          <button type="submit" disabled={busy || !!problem || !body} className={primary}>
            {busy ? <span className="shimmer">Saving…</span> : id ? "Save pipeline" : "Create pipeline"}
          </button>
          {problem && (trimmed || id) && <span className="text-[12px] text-danger">{problem}</span>}
        </div>
        <ErrorNote error={error} />
      </form>
      {pipeline && <TestPipeline cluster={cluster} pipeline={pipeline} indices={indices} onSteps={setSteps} onOpenConsole={onOpenConsole} />}
      {id && <DefaultPipeline cluster={cluster} id={id} indices={indices} users={users} onDone={() => onSaved(id, body ?? json)} />}
    </Panel>
  );
}

const STATUS_TONE: Record<string, string> = {
  success: "text-ok",
  error: "text-danger",
  error_ignored: "text-muted",
  skipped: "text-subtle",
  dropped: "text-muted",
};
const STATUS_LABEL: Record<string, string> = { success: "ran", error: "failed", error_ignored: "failed, ignored", skipped: "skipped", dropped: "dropped the document" };

function ProcessorList({
  title,
  hint,
  list,
  onChange,
  types,
  steps,
  prefix,
  depth = 0,
}: {
  title: string;
  hint?: string;
  list: Processor[];
  onChange: (list: Processor[]) => void;
  types: string[];
  steps: SimStep[] | null;
  prefix: string;
  depth?: number;
}) {
  const [open, setOpen] = useState<number | null>(null);
  const move = (i: number, d: -1 | 1) => {
    const next = [...list];
    [next[i], next[i + d]] = [next[i + d], next[i]];
    onChange(next);
    if (open === i) setOpen(i + d);
    else if (open === i + d) setOpen(i);
  };
  const add = () => {
    onChange([...list, { type: "set", options: {} }]);
    setOpen(list.length);
  };
  return (
    <div className="space-y-2">
      <div className="flex items-baseline justify-between gap-3">
        <h3 className="text-[12px] font-medium">
          {title}
          {hint && <span className="ml-2 font-normal text-subtle">{hint}</span>}
        </h3>
      </div>
      {list.length > 0 && (
        <ol className="divide-y divide-line overflow-hidden rounded-lg border border-line">
          {list.map((p, i) => {
            const path = `${prefix}${i}`;
            // The last result of this processor on the selected document (on_failure runs after it).
            const step = steps?.filter((s) => s.path === path).at(-1);
            const expanded = open === i;
            return (
              <li key={i} className={expanded ? "bg-panel-2" : ""}>
                <div className="flex min-w-0 items-center gap-2 px-3 py-2">
                  <button type="button" onClick={() => setOpen(expanded ? null : i)} className="flex min-w-0 flex-1 items-center gap-2 text-left" aria-expanded={expanded}>
                    <ChevronRight className={`size-3.5 shrink-0 text-subtle transition-transform duration-200 motion-reduce:transition-none ${expanded ? "rotate-90" : ""}`} />
                    <span className="shrink-0 font-mono text-[11px] text-subtle tabular-nums">{i + 1}</span>
                    <span className="shrink-0 font-mono text-[12px]">{p.type}</span>
                    <span className="min-w-0 truncate font-mono text-[12px] text-muted">{p.description || processorSummary(p)}</span>
                    {p.if && <span className="shrink-0 rounded border border-line px-1 text-[10px] text-subtle">if</span>}
                    {!!p.onFailure?.length && <span className="shrink-0 rounded border border-line px-1 text-[10px] text-subtle">on failure {p.onFailure.length}</span>}
                  </button>
                  {step && (
                    <span className={`shrink-0 text-[11px] ${STATUS_TONE[step.status]}`} title={step.error ?? (step.changed.length ? `changed ${step.changed.join(", ")}` : undefined)}>
                      {STATUS_LABEL[step.status]}
                    </span>
                  )}
                  <span className="flex shrink-0 items-center">
                    <button type="button" onClick={() => move(i, -1)} disabled={i === 0} className="rounded-md p-1 text-subtle transition hover:text-fg disabled:opacity-30" aria-label={`Move ${p.type} up`}>
                      <ArrowUp className="size-3.5" />
                    </button>
                    <button type="button" onClick={() => move(i, 1)} disabled={i === list.length - 1} className="rounded-md p-1 text-subtle transition hover:text-fg disabled:opacity-30" aria-label={`Move ${p.type} down`}>
                      <ArrowDown className="size-3.5" />
                    </button>
                    <button type="button" onClick={() => (onChange(list.filter((_, j) => j !== i)), setOpen(null))} className="rounded-md p-1 text-subtle transition hover:text-danger" aria-label={`Remove ${p.type}`}>
                      <Trash2 className="size-3.5" />
                    </button>
                  </span>
                </div>
                {expanded && (
                  <div className="border-t border-line px-3 py-3 sm:pl-9">
                    <ProcessorForm p={p} onChange={(q) => onChange(list.map((x, j) => (j === i ? q : x)))} types={types} steps={steps} path={path} depth={depth} />
                  </div>
                )}
              </li>
            );
          })}
        </ol>
      )}
      <button type="button" onClick={add} className={btn}>
        <Plus className="size-3.5" /> {list.length ? "Processor" : `Add ${title.toLowerCase().replace(/s$/, "")}`}
      </button>
    </div>
  );
}

function ProcessorForm({ p, onChange, types, steps, path, depth }: { p: Processor; onChange: (p: Processor) => void; types: string[]; steps: SimStep[] | null; path: string; depth: number }) {
  const spec = PROCESSOR_FORMS[p.type];
  const known = types.length ? types : Object.keys(PROCESSOR_FORMS);
  const choices = known.includes(p.type) ? known : [p.type, ...known];
  const setOption = (key: string, v: unknown) => {
    const options = { ...p.options };
    if (v === undefined) delete options[key];
    else options[key] = v;
    onChange({ ...p, options });
  };
  return (
    <div className="space-y-3">
      <div className="grid gap-3 sm:grid-cols-[12rem_minmax(0,1fr)]">
        <Field label="Type">
          <select value={p.type} onChange={(e) => onChange({ ...p, type: e.target.value, options: {} })} className={`${input} w-full font-mono text-[12px]`}>
            {choices.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Description (optional)">
          <input value={p.description ?? ""} onChange={(e) => onChange({ ...p, description: e.target.value || undefined })} className={`${input} w-full`} />
        </Field>
      </div>
      {spec ? (
        <div className="grid gap-3 sm:grid-cols-2">
          {spec.map((o) => (
            <OptionInput key={`${p.type}.${o.key}`} spec={o} value={p.options[o.key]} onChange={(v) => setOption(o.key, v)} />
          ))}
        </div>
      ) : (
        <JsonOptions key={p.type} value={p.options} onChange={(options) => onChange({ ...p, options })} />
      )}
      <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_12rem]">
        <Field label="Condition (if, Painless)">
          <input value={p.if ?? ""} onChange={(e) => onChange({ ...p, if: e.target.value || undefined })} placeholder="ctx.event?.kind == 'alert'" className={`${input} w-full font-mono text-[12px]`} spellCheck={false} />
        </Field>
        <Field label="Tag">
          <input value={p.tag ?? ""} onChange={(e) => onChange({ ...p, tag: e.target.value || undefined })} className={`${input} w-full font-mono text-[12px]`} spellCheck={false} />
        </Field>
      </div>
      <label className="flex items-center gap-2 text-[13px] text-muted">
        <input type="checkbox" checked={!!p.ignoreFailure} onChange={(e) => onChange({ ...p, ignoreFailure: e.target.checked || undefined })} className="accent-accent" />
        Ignore failure (carry on with the next processor)
      </label>
      {depth < 2 && (
        <ProcessorList title="On failure" list={p.onFailure ?? []} onChange={(onFailure) => onChange({ ...p, onFailure: onFailure.length ? onFailure : undefined })} types={types} steps={steps} prefix={`${path}.on_failure.`} depth={depth + 1} />
      )}
    </div>
  );
}

/** One option's field: its text is kept as typed, and the value read from it on each change. */
function OptionInput({ spec, value, onChange }: { spec: OptionSpec; value: unknown; onChange: (v: unknown) => void }) {
  const [text, setText] = useState(() => optionText(spec, value));
  const label = `${spec.label ?? spec.key}${spec.required ? "" : " (optional)"}`;
  if (spec.kind === "bool")
    return (
      <label className="flex items-center gap-2 self-end pb-1.5 text-[13px] text-muted">
        <input type="checkbox" checked={value === true} onChange={(e) => onChange(e.target.checked ? true : undefined)} className="accent-accent" />
        <span className="font-mono text-[12px]">{spec.label ?? spec.key}</span>
      </label>
    );
  const set = (t: string) => {
    setText(t);
    onChange(optionValue(spec, t));
  };
  if (spec.kind === "select")
    return (
      <Field label={label}>
        <select value={String(text)} onChange={(e) => set(e.target.value)} className={`${input} w-full font-mono text-[12px]`}>
          <option value="">{spec.required ? "pick one" : "default"}</option>
          {spec.choices?.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
      </Field>
    );
  const multiline = spec.kind === "list" || spec.kind === "code" || spec.kind === "json";
  return (
    <Field label={spec.kind === "list" ? `${label}, one per line` : label}>
      {multiline ? (
        <textarea value={String(text)} onChange={(e) => set(e.target.value)} rows={spec.kind === "code" ? 4 : 2} placeholder={spec.placeholder} className={`${input} w-full resize-y font-mono text-[12px]`} spellCheck={false} />
      ) : (
        <input value={String(text)} onChange={(e) => set(e.target.value)} placeholder={spec.placeholder} className={`${input} w-full font-mono text-[12px]`} spellCheck={false} />
      )}
    </Field>
  );
}

/** A type the form has no fields for: its options as a JSON object. */
function JsonOptions({ value, onChange }: { value: Record<string, unknown>; onChange: (v: Record<string, unknown>) => void }) {
  const [text, setText] = useState(() => JSON.stringify(value, null, 2));
  const [bad, setBad] = useState(false);
  return (
    <Field label="Options (JSON)">
      <textarea
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          try {
            const v = JSON.parse(e.target.value || "{}");
            const ok = !!v && typeof v === "object" && !Array.isArray(v);
            setBad(!ok);
            if (ok) onChange(v);
          } catch {
            setBad(true);
          }
        }}
        rows={4}
        spellCheck={false}
        className={`${input} w-full resize-y font-mono text-[12px] ${bad ? "border-danger/60" : ""}`}
      />
    </Field>
  );
}

// --- Test --------------------------------------------------------------------------------------

const SAMPLE = `[
  { "message": "203.0.113.7 GET /index.html 200" }
]`;

function TestPipeline({ cluster, pipeline, indices, onSteps, onOpenConsole }: { cluster: SearchCluster; pipeline: Pipeline; indices: string[]; onSteps: (s: SimStep[] | null) => void; onOpenConsole: (text: string) => void }) {
  const [docsText, setDocsText] = useState(SAMPLE);
  const [from, setFrom] = useState("");
  const [results, setResults] = useState<SimDoc[] | null>(null);
  const [selected, setSelected] = useState(0);
  const [busy, setBusy] = useState<"run" | "fetch" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const parsed = sampleDocs(docsText);

  const fetchDocs = async () => {
    if (!from) return;
    setBusy("fetch");
    setError(null);
    try {
      const r = await esJson<{ hits: { hits: { _index: string; _id: string; _source?: unknown }[] } }>(cluster.id, "POST", `${indexPath(from)}/_search`, { size: 5, query: { match_all: {} } });
      setDocsText(JSON.stringify(hitsAsDocs(r.hits.hits), null, 2));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  const run = async () => {
    if ("error" in parsed) return;
    setBusy("run");
    setError(null);
    try {
      const { body, paths } = taggedForSimulate(pipeline);
      const r = await esJson<Parameters<typeof simulateResults>[1]>(cluster.id, "POST", "/_ingest/pipeline/_simulate?verbose=true", { pipeline: body, docs: parsed.docs });
      const res = simulateResults(parsed.docs, r, paths);
      setResults(res);
      const i = Math.min(selected, res.length - 1);
      setSelected(i);
      onSteps(res[i]?.steps ?? null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      onSteps(null);
    } finally {
      setBusy(null);
    }
  };

  const doc = results?.[selected];
  return (
    <section className="space-y-3 border-t border-line px-5 py-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h3 className="flex items-center gap-2 text-[13px] font-medium">
          <FlaskConical className="size-3.5 text-muted" /> Test
        </h3>
        <span className="flex flex-wrap items-center gap-2">
          {indices.length > 0 && (
            <>
              <select value={from} onChange={(e) => setFrom(e.target.value)} aria-label="Index to take documents from" className={`${input} max-w-48 py-1 font-mono text-[12px]`}>
                <option value="">documents from an index…</option>
                {indices.map((i) => (
                  <option key={i} value={i}>
                    {i}
                  </option>
                ))}
              </select>
              <button type="button" onClick={fetchDocs} disabled={!from || busy !== null} className={btn}>
                {busy === "fetch" ? <span className="shimmer">Fetching…</span> : "Fetch 5"}
              </button>
            </>
          )}
        </span>
      </div>
      <textarea value={docsText} onChange={(e) => setDocsText(e.target.value)} rows={6} spellCheck={false} aria-label="Sample documents" className={`${input} w-full resize-y font-mono text-[12px] leading-relaxed`} />
      <div className="flex flex-wrap items-center gap-3">
        <button type="button" onClick={run} disabled={busy !== null || "error" in parsed} className={primary}>
          {busy === "run" ? <span className="shimmer">Running…</span> : "Run the pipeline"}
        </button>
        {"error" in parsed && <span className="text-[12px] text-danger">{parsed.error}</span>}
        {"docs" in parsed && (
          <button
            type="button"
            onClick={() => onOpenConsole(consoleText("POST", "/_ingest/pipeline/_simulate?verbose=true", { pipeline: pipelineToJson(pipeline), docs: parsed.docs }))}
            className="inline-flex items-center gap-1 text-[12px] text-subtle transition hover:text-fg"
          >
            <SquareTerminal className="size-3.5" /> Open in console
          </button>
        )}
      </div>
      <ErrorNote error={error} />
      {results && (
        <div className="rise space-y-3">
          {results.length > 1 && (
            <div className="flex flex-wrap gap-1" role="tablist" aria-label="Documents">
              {results.map((r, i) => (
                <button
                  key={i}
                  type="button"
                  role="tab"
                  aria-selected={i === selected}
                  onClick={() => (setSelected(i), onSteps(r.steps))}
                  className={`rounded-md border px-2 py-0.5 font-mono text-[11px] transition ${i === selected ? "border-line-2 text-fg" : "border-transparent text-muted hover:text-fg"} ${r.error ? "text-danger" : ""}`}
                >
                  doc {i + 1}
                </button>
              ))}
            </div>
          )}
          {doc && <SimulatedDoc doc={doc} />}
        </div>
      )}
    </section>
  );
}

function SimulatedDoc({ doc }: { doc: SimDoc }) {
  const [json, setJson] = useState(false);
  return (
    <div className="space-y-3">
      <ol className="space-y-1">
        {doc.steps.map((s, i) => (
          <li key={i} className="grid grid-cols-[1.5rem_minmax(0,1fr)] gap-x-2 text-[12px] sm:grid-cols-[1.5rem_10rem_8rem_minmax(0,1fr)]">
            <span className="font-mono text-subtle tabular-nums">{i + 1}</span>
            <span className="truncate font-mono">
              {s.type}
              {s.path?.includes("on_failure") && <span className="ml-1 text-subtle">(on failure)</span>}
            </span>
            <span className={`col-start-2 sm:col-start-auto ${STATUS_TONE[s.status]}`}>{STATUS_LABEL[s.status]}</span>
            <span className="col-start-2 min-w-0 font-mono break-words text-muted sm:col-start-auto">
              {s.error ? <span className="text-danger">{s.error}</span> : s.changed.length ? s.changed.join(", ") : s.status === "success" ? <span className="font-sans text-subtle">no change</span> : null}
            </span>
          </li>
        ))}
      </ol>
      {doc.error ? (
        <p className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[12px] text-danger">Not indexed: {doc.error}</p>
      ) : doc.output === null ? (
        <p className="text-[12px] text-muted">Dropped: the document isn&apos;t indexed.</p>
      ) : (
        <div className="rounded-lg border border-line">
          <div className="flex items-center justify-between gap-3 border-b border-line px-3 py-1.5">
            <span className="text-[11px] text-subtle">Result</span>
            <Segmented
              value={json ? "both" : "after"}
              options={[
                { label: "after", value: "after" },
                { label: "before / after", value: "both" },
              ]}
              onChange={(v) => setJson(v === "both")}
            />
          </div>
          <div className={json ? "grid divide-line sm:grid-cols-2 sm:divide-x" : ""}>
            {json && (
              <pre className="max-h-72 overflow-auto px-3 py-2 font-mono text-[12px] leading-relaxed">
                <JsonText text={JSON.stringify(doc.input, null, 2)} />
              </pre>
            )}
            <pre className="max-h-72 overflow-auto px-3 py-2 font-mono text-[12px] leading-relaxed">
              <JsonText text={JSON.stringify(doc.output, null, 2)} />
            </pre>
          </div>
        </div>
      )}
    </div>
  );
}

// --- default_pipeline --------------------------------------------------------------------------

/** Sets (or clears) an index's `index.default_pipeline` to this pipeline. */
function DefaultPipeline({ cluster, id, indices, users, onDone }: { cluster: SearchCluster; id: string; indices: string[]; users: { index: string; as: "default" | "final" }[]; onDone: () => Promise<void> }) {
  const [index, setIndex] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const defaults = users.filter((u) => u.as === "default").map((u) => u.index);
  const set = async (target: string, value: string | null) => {
    setBusy(target);
    setError(null);
    try {
      await esJson(cluster.id, "PUT", `${indexPath(target)}/_settings`, { "index.default_pipeline": value });
      setIndex("");
      await onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <section className="space-y-3 border-t border-line px-5 py-4">
      <h3 className="text-[13px] font-medium">Default pipeline of</h3>
      {users.length > 0 && (
        <ul className="flex flex-wrap gap-2">
          {users.map((u) => (
            <li key={`${u.index}.${u.as}`} className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-0.5 font-mono text-[12px]">
              {u.index}
              {u.as === "final" ? (
                <span className="font-sans text-[11px] text-subtle">final</span>
              ) : (
                <button onClick={() => set(u.index, null)} disabled={busy !== null} className="text-subtle transition hover:text-danger disabled:opacity-40" aria-label={`Stop ${u.index} using ${id}`} title="Stop using it by default">
                  <X className="size-3" />
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <select value={index} onChange={(e) => setIndex(e.target.value)} aria-label="Index" className={`${input} max-w-64 font-mono text-[12px]`}>
          <option value="">pick an index…</option>
          {indices
            .filter((i) => !defaults.includes(i))
            .map((i) => (
              <option key={i} value={i}>
                {i}
              </option>
            ))}
        </select>
        <button onClick={() => set(index, id)} disabled={!index || busy !== null} className={btn}>
          {busy === index && index ? <span className="shimmer">Setting…</span> : "Set as default"}
        </button>
        <span className="text-[12px] text-subtle">
          Documents indexed without a <span className="font-mono">pipeline</span> parameter run it.
        </span>
      </div>
      <ErrorNote error={error} />
    </section>
  );
}
