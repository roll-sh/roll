import { Pause, Play, Plus, SquareTerminal, X } from "lucide-react";
import { useMemo, useState } from "react";
import { esJson, type SearchCluster } from "@/lib/search-api";
import { describeCondition, describeCron, SM_SCHEDULE_PRESETS, smPolicyBody, smProblem, type Repository, type SmForm, type SmRow, type SmWorkflow } from "@/lib/search-snapshots";
import { Field } from "../redis/shared";
import { ago, btn, ErrorNote, input, Panel, primary } from "../ui";
import { Check, ConsoleButton, DeleteButton, useBusy } from "./actions";
import { consoleText, RequestPreview } from "./request-preview";

const enc = encodeURIComponent;

/**
 * OpenSearch's snapshot management policies (`_plugins/_sm/policies`): each one's creation
 * schedule and deletion condition in words, its workflows' states from `_explain` (the plugin
 * steps them once a minute) and their last outcome; start, stop, edit and delete.
 */
export function SmPolicies({
  cluster,
  repos,
  policies,
  reload,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  repos: Record<string, Repository>;
  policies: SmRow[];
  reload: () => Promise<void>;
  onOpenConsole: (text: string) => void;
}) {
  const { busy, error, act } = useBusy(reload);
  const [editing, setEditing] = useState<SmRow | "new" | null>(null);

  return (
    <>
      <Panel
        title="Policies"
        aside={
          !editing &&
          Object.keys(repos).length > 0 && (
            <button onClick={() => setEditing("new")} className={btn}>
              <Plus className="size-3.5" /> New policy
            </button>
          )
        }
      >
        {policies.length === 0 ? (
          <p className="px-5 py-4 text-[13px] text-muted">No policies. A snapshot management policy takes snapshots on a cron schedule and deletes the ones past its condition.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Policy</th>
                  <th className="px-3 py-2 font-normal">Creation</th>
                  <th className="px-3 py-2 font-normal">Deletion</th>
                  <th className="px-3 py-2 font-normal">Last outcome</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {policies.map((p) => {
                  const pol = p.doc.sm_policy;
                  const enabled = pol.enabled !== false;
                  return (
                    <tr key={p.name} className="border-b border-line align-top last:border-0">
                      <td className="max-w-64 px-5 py-2.5">
                        <button onClick={() => setEditing(p)} className="block max-w-full truncate text-left font-mono text-[12px] hover:underline">
                          {p.name}
                        </button>
                        <div className="truncate font-mono text-[11px] text-subtle">→ {pol.snapshot_config.repository}</div>
                        {!enabled && <div className="text-[11px] text-muted">stopped</div>}
                      </td>
                      <td className="px-3 py-2.5 whitespace-nowrap">
                        <div className="text-[12px]" title={pol.creation?.schedule.cron.expression}>
                          {pol.creation ? describeCron(pol.creation.schedule.cron) : "—"}
                        </div>
                        <State w={p.explain?.creation} />
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="text-[12px] text-muted">{describeCondition(pol.deletion)}</div>
                        <State w={p.explain?.deletion} />
                      </td>
                      <td className="max-w-80 px-3 py-2.5 text-[12px]">
                        <Outcome w={p.explain?.creation} />
                        <Outcome w={p.explain?.deletion} />
                        {!p.explain?.creation?.latest_execution && !p.explain?.deletion?.latest_execution && <span className="text-subtle">—</span>}
                      </td>
                      <td className="px-5 py-2 text-right whitespace-nowrap">
                        <span className="inline-flex items-center gap-1">
                          <button
                            onClick={() => act(`toggle:${p.name}`, () => esJson(cluster.id, "POST", `/_plugins/_sm/policies/${enc(p.name)}/${enabled ? "_stop" : "_start"}`))}
                            disabled={busy !== null}
                            className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[12px] text-muted transition hover:text-fg disabled:opacity-40"
                            title={enabled ? "Stop the policy: no snapshot is taken or deleted until it starts again" : "Start the policy"}
                          >
                            {busy === `toggle:${p.name}` ? <span className="shimmer">{enabled ? "Stopping…" : "Starting…"}</span> : enabled ? <><Pause className="size-3" /> Stop</> : <><Play className="size-3" /> Start</>}
                          </button>
                          <ConsoleButton text={`GET /_plugins/_sm/policies/${enc(p.name)}/_explain`} onOpenConsole={onOpenConsole} />
                          <DeleteButton label={p.name} busy={busy === `delete:${p.name}`} onDelete={() => act(`delete:${p.name}`, () => esJson(cluster.id, "DELETE", `/_plugins/_sm/policies/${enc(p.name)}`))} />
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
        <SmPolicyForm
          key={editing === "new" ? "new" : editing.name}
          cluster={cluster}
          repos={Object.keys(repos).sort()}
          existing={editing === "new" ? null : editing}
          taken={policies.map((p) => p.name)}
          onClose={() => setEditing(null)}
          onDone={async () => {
            setEditing(null);
            await reload();
          }}
          onOpenConsole={onOpenConsole}
        />
      )}
    </>
  );
}

/** A workflow's state and when it next fires. */
function State({ w }: { w: SmWorkflow | undefined }) {
  if (!w) return null;
  const at = w.trigger?.time;
  const busy = w.current_state === "CREATING" || w.current_state === "DELETING";
  return (
    <div className="text-[11px] text-subtle">
      <span className={`font-mono ${busy ? "text-accent" : ""}`}>{w.current_state.toLowerCase()}</span>
      {at ? <> · next {at > Date.now() ? `in ${until(at - Date.now())}` : "due"}</> : null}
    </div>
  );
}

/** A workflow's latest execution: its status, when, and the plugin's message. */
function Outcome({ w }: { w: SmWorkflow | undefined }) {
  const e = w?.latest_execution;
  if (!e) return null;
  const tone = e.status === "SUCCESS" ? "text-ok" : e.status === "FAILED" || e.status === "RETRYING" ? "text-danger" : "text-muted";
  const message = (e.info?.message ?? "").replace(/^\[[^\]]*\]:\s*/, "");
  return (
    <div className="mb-1">
      <span className={tone}>{e.status.toLowerCase().replace(/_/g, " ")}</span> <span className="text-subtle">{ago(Math.floor((e.end_time ?? e.start_time) / 1000))}</span>
      <div className="line-clamp-2 text-[11px] text-muted" title={e.info?.cause ?? message}>
        {message}
      </div>
    </div>
  );
}

function until(ms: number) {
  const m = Math.round(ms / 60_000);
  if (m < 60) return `${Math.max(1, m)}m`;
  if (m < 48 * 60) return `${Math.round(m / 60)}h`;
  return `${Math.round(m / 1440)}d`;
}

function SmPolicyForm({
  cluster,
  repos,
  existing,
  taken,
  onClose,
  onDone,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  repos: string[];
  existing: SmRow | null;
  taken: string[];
  onClose: () => void;
  onDone: () => Promise<void>;
  onOpenConsole: (text: string) => void;
}) {
  const [form, setForm] = useState<SmForm>(() => {
    const p = existing?.doc.sm_policy;
    const idx = p?.snapshot_config.indices;
    const indices = Array.isArray(idx) ? idx.join(", ") : (idx ?? "");
    const cond = p?.deletion?.condition;
    return {
      name: p?.name ?? "",
      description: p?.description ?? "",
      repository: p?.snapshot_config.repository ?? repos[0] ?? "",
      indices: indices === "*" ? "" : indices,
      includeGlobalState: String(p?.snapshot_config.include_global_state ?? "true") === "true",
      schedule: p?.creation?.schedule.cron.expression ?? SM_SCHEDULE_PRESETS[0].value,
      timezone: p?.creation?.schedule.cron.timezone ?? "UTC",
      deletes: existing ? !!p?.deletion : true,
      maxAge: cond?.max_age ?? "",
      minCount: cond?.min_count?.toString() ?? "",
      maxCount: cond?.max_count?.toString() ?? (existing ? "" : "14"),
    };
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const set = (patch: Partial<SmForm>) => setForm((f) => ({ ...f, ...patch }));
  const preset = SM_SCHEDULE_PRESETS.some((s) => s.value === form.schedule) ? form.schedule : "custom";
  const body = useMemo(() => smPolicyBody(form), [form]);
  const name = form.name.trim();
  // An update names the version it replaces, as the plugin requires.
  const path = existing
    ? `/_plugins/_sm/policies/${enc(existing.name)}?if_seq_no=${existing.doc._seq_no}&if_primary_term=${existing.doc._primary_term}`
    : `/_plugins/_sm/policies/${enc(name || "nightly")}`;
  const method = existing ? "PUT" : "POST";
  const problem = smProblem(form, taken, !!existing);

  return (
    <Panel
      title={existing ? `Edit ${existing.name}` : "New policy"}
      className="rise"
      aside={
        <button onClick={onClose} className={btn} aria-label="Close">
          <X className="size-3.5" />
        </button>
      }
    >
      <form
        className="space-y-4 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            await esJson(cluster.id, method, path, body);
            await onDone();
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="grid gap-3 sm:grid-cols-3">
          <Field label="Policy" hint="snapshots are named after it">
            <input value={form.name} onChange={(e) => set({ name: e.target.value })} disabled={!!existing} placeholder="nightly" className={`${input} w-full font-mono disabled:opacity-60`} spellCheck={false} autoFocus={!existing} />
          </Field>
          <Field label="Repository">
            <select value={form.repository} onChange={(e) => set({ repository: e.target.value })} className={`${input} w-full font-mono text-[12px]`}>
              {repos.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Description">
            <input value={form.description} onChange={(e) => set({ description: e.target.value })} className={`${input} w-full`} />
          </Field>
        </div>
        <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,1fr)]">
          <Field label="Schedule" hint={describeCron({ expression: form.schedule, timezone: form.timezone || "UTC" })}>
            <select value={preset} onChange={(e) => e.target.value !== "custom" && set({ schedule: e.target.value })} className={`${input} w-full`}>
              {SM_SCHEDULE_PRESETS.map((s) => (
                <option key={s.value} value={s.value}>
                  {s.label}
                </option>
              ))}
              <option value="custom">Custom</option>
            </select>
          </Field>
          <Field label="Cron" hint="min hour day month weekday">
            <input value={form.schedule} onChange={(e) => set({ schedule: e.target.value })} className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
          <Field label="Time zone">
            <input value={form.timezone} onChange={(e) => set({ timezone: e.target.value })} placeholder="UTC" className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
        </div>
        <Field label="Indices" hint="patterns; empty for every index">
          <input value={form.indices} onChange={(e) => set({ indices: e.target.value })} placeholder="logs-*, products" className={`${input} w-full font-mono`} spellCheck={false} />
        </Field>
        <Check checked={form.deletes} onChange={(v) => set({ deletes: v })}>
          Delete old snapshots (on the same schedule)
        </Check>
        {form.deletes && (
          <div className="grid gap-3 sm:grid-cols-3">
            <Field label="Older than">
              <input value={form.maxAge} onChange={(e) => set({ maxAge: e.target.value })} placeholder="30d" className={`${input} w-full font-mono`} spellCheck={false} />
            </Field>
            <Field label="Keep at least" hint="default 1">
              <input value={form.minCount} onChange={(e) => set({ minCount: e.target.value.replace(/\D/g, "") })} inputMode="numeric" className={`${input} w-full font-mono`} />
            </Field>
            <Field label="Keep at most">
              <input value={form.maxCount} onChange={(e) => set({ maxCount: e.target.value.replace(/\D/g, "") })} inputMode="numeric" className={`${input} w-full font-mono`} />
            </Field>
          </div>
        )}
        <Check checked={form.includeGlobalState} onChange={(v) => set({ includeGlobalState: v })}>
          Include global state
        </Check>
        <RequestPreview method={method} path={path} body={body} onOpenConsole={onOpenConsole} />
        <div className="flex flex-wrap items-center gap-3">
          <button type="submit" disabled={busy || !name || !!problem} className={primary}>
            {busy ? <span className="shimmer">Saving…</span> : existing ? "Save policy" : "Create policy"}
          </button>
          {problem && <span className="text-[12px] text-danger">{problem}</span>}
          {existing && (
            <button type="button" onClick={() => onOpenConsole(consoleText(method, path, body))} className={btn}>
              <SquareTerminal className="size-3.5" /> Edit as JSON
            </button>
          )}
        </div>
        <ErrorNote error={error} />
      </form>
    </Panel>
  );
}
