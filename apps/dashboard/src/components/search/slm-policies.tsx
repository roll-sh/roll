import { Play, Plus, SquareTerminal, X } from "lucide-react";
import { useMemo, useState } from "react";
import { esJson, type SearchCluster } from "@/lib/search-api";
import { describeRetention, describeSchedule, failureReason, SCHEDULE_PRESETS, slmPolicyBody, type Repository, type SlmForm, type SlmRow } from "@/lib/search-snapshots";
import { Field } from "../redis/shared";
import { ago, btn, ErrorNote, input, Panel, primary } from "../ui";
import { Check, ConsoleButton, DeleteButton, useBusy } from "./actions";
import { consoleText, RequestPreview } from "./request-preview";

const enc = encodeURIComponent;

/**
 * Snapshot lifecycle policies (`_slm/policy`): each one's schedule in words, its retention, next
 * run, last success and last failure; run now, edit, delete, and start a stopped SLM.
 */
export function Policies({
  cluster,
  repos,
  policies,
  mode,
  reload,
  onOpenConsole,
}: {
  cluster: SearchCluster;
  repos: Record<string, Repository>;
  policies: SlmRow[];
  mode: string | null;
  reload: () => Promise<void>;
  onOpenConsole: (text: string) => void;
}) {
  const { busy, error, act } = useBusy(reload);
  const [editing, setEditing] = useState<SlmRow | "new" | null>(null);
  const [ran, setRan] = useState<Record<string, string>>({});
  const now = Date.now();

  return (
    <>
      <Panel
        title="Policies"
        aside={
          <span className="flex items-center gap-2">
            {mode && mode !== "RUNNING" && (
              <button onClick={() => act("start", () => esJson(cluster.id, "POST", "/_slm/start"))} className={btn} title="Snapshot lifecycle management is stopped: no policy runs">
                {busy === "start" ? <span className="shimmer">Starting…</span> : <>SLM {mode.toLowerCase()} · start</>}
              </button>
            )}
            {!editing && Object.keys(repos).length > 0 && (
              <button onClick={() => setEditing("new")} className={btn}>
                <Plus className="size-3.5" /> New policy
              </button>
            )}
          </span>
        }
      >
        {policies.length === 0 ? (
          <p className="px-5 py-4 text-[13px] text-muted">No policies. A policy takes snapshots on a schedule and deletes the ones its retention no longer keeps.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Policy</th>
                  <th className="px-3 py-2 font-normal">Schedule</th>
                  <th className="px-3 py-2 font-normal">Retention</th>
                  <th className="px-3 py-2 font-normal">Last success</th>
                  <th className="px-3 py-2 font-normal">Last failure</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {policies.map((p) => {
                  const pol = p.info.policy;
                  const next = p.info.next_execution_millis;
                  return (
                    <tr key={p.id} className="border-b border-line align-top last:border-0">
                      <td className="max-w-64 px-5 py-2.5">
                        <button onClick={() => setEditing(p)} className="block max-w-full truncate text-left font-mono text-[12px] hover:underline">
                          {p.id}
                        </button>
                        <div className="truncate font-mono text-[11px] text-subtle">
                          {pol.name} → {pol.repository}
                        </div>
                        {ran[p.id] && <div className="truncate font-mono text-[11px] text-accent">started {ran[p.id]}</div>}
                      </td>
                      <td className="px-3 py-2.5 whitespace-nowrap">
                        <div className="text-[12px]" title={pol.schedule}>
                          {describeSchedule(pol.schedule)}
                        </div>
                        {next !== undefined && <div className="text-[11px] text-subtle">next {next > now ? `in ${until(next - now)}` : "due"}</div>}
                      </td>
                      <td className="px-3 py-2.5 text-[12px] text-muted">{describeRetention(pol.retention) || "keeps all"}</td>
                      <td className="px-3 py-2.5 text-[12px] whitespace-nowrap">
                        {p.info.last_success ? (
                          <span className={p.failing ? "text-muted" : "text-ok"} title={p.info.last_success.snapshot_name}>
                            {ago(Math.floor(p.info.last_success.time / 1000))}
                          </span>
                        ) : (
                          <span className="text-subtle">never</span>
                        )}
                      </td>
                      <td className="max-w-72 px-3 py-2.5 text-[12px]">
                        {p.info.last_failure ? (
                          <div className={p.failing ? "text-danger" : "text-muted"}>
                            {ago(Math.floor(p.info.last_failure.time / 1000))}
                            <div className="line-clamp-2 text-[11px]" title={failureReason(p.info.last_failure.details)}>
                              {failureReason(p.info.last_failure.details)}
                            </div>
                          </div>
                        ) : (
                          <span className="text-subtle">—</span>
                        )}
                      </td>
                      <td className="px-5 py-2 text-right whitespace-nowrap">
                        <span className="inline-flex items-center gap-1">
                          <button
                            onClick={() =>
                              act(`run:${p.id}`, async () => {
                                const r = await esJson<{ snapshot_name: string }>(cluster.id, "PUT", `/_slm/policy/${enc(p.id)}/_execute`);
                                setRan((m) => ({ ...m, [p.id]: r.snapshot_name }));
                              })
                            }
                            disabled={busy !== null}
                            className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[12px] text-muted transition hover:text-fg disabled:opacity-40"
                            title="Take the policy's snapshot now"
                          >
                            {busy === `run:${p.id}` ? <span className="shimmer">Running…</span> : <><Play className="size-3" /> Run now</>}
                          </button>
                          <ConsoleButton text={`GET /_slm/policy/${enc(p.id)}?human`} onOpenConsole={onOpenConsole} />
                          <DeleteButton label={p.id} busy={busy === `delete:${p.id}`} onDelete={() => act(`delete:${p.id}`, () => esJson(cluster.id, "DELETE", `/_slm/policy/${enc(p.id)}`))} />
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
        <PolicyForm
          key={editing === "new" ? "new" : editing.id}
          cluster={cluster}
          repos={Object.keys(repos).sort()}
          existing={editing === "new" ? null : editing}
          taken={policies.map((p) => p.id)}
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

function until(ms: number) {
  const m = Math.round(ms / 60_000);
  if (m < 60) return `${Math.max(1, m)}m`;
  if (m < 48 * 60) return `${Math.round(m / 60)}h`;
  return `${Math.round(m / 1440)}d`;
}

function PolicyForm({
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
  existing: SlmRow | null;
  taken: string[];
  onClose: () => void;
  onDone: () => Promise<void>;
  onOpenConsole: (text: string) => void;
}) {
  const [form, setForm] = useState<SlmForm>(() => {
    const p = existing?.info.policy;
    const idx = p?.config?.indices;
    return {
      id: existing?.id ?? "",
      name: p?.name ?? "<nightly-snap-{now/d}>",
      schedule: p?.schedule ?? SCHEDULE_PRESETS[0].value,
      repository: p?.repository ?? repos[0] ?? "",
      indices: Array.isArray(idx) ? idx.filter((i) => i !== "*").join(", ") : idx && idx !== "*" ? idx : "",
      includeGlobalState: p?.config?.include_global_state ?? true,
      expireAfter: p?.retention?.expire_after ?? "30d",
      minCount: p?.retention?.min_count?.toString() ?? "5",
      maxCount: p?.retention?.max_count?.toString() ?? "50",
    };
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const set = (patch: Partial<SlmForm>) => setForm((f) => ({ ...f, ...patch }));
  const preset = SCHEDULE_PRESETS.some((s) => s.value === form.schedule) ? form.schedule : "custom";
  const body = useMemo(() => slmPolicyBody(form), [form]);
  const id = form.id.trim();
  const path = `/_slm/policy/${enc(id || "nightly-snapshots")}`;
  const problem = !id ? null : !existing && taken.includes(id) ? `${id} exists already.` : !form.name.trim() ? "Name the snapshots." : !form.schedule.trim() ? "Give a schedule." : null;

  return (
    <Panel
      title={existing ? `Edit ${existing.id}` : "New policy"}
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
            await esJson(cluster.id, "PUT", path, body);
            await onDone();
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="grid gap-3 sm:grid-cols-3">
          <Field label="Policy">
            <input value={form.id} onChange={(e) => set({ id: e.target.value })} disabled={!!existing} placeholder="nightly-snapshots" className={`${input} w-full font-mono disabled:opacity-60`} spellCheck={false} autoFocus={!existing} />
          </Field>
          <Field label="Snapshot name" hint="date math: <nightly-{now/d}>">
            <input value={form.name} onChange={(e) => set({ name: e.target.value })} className={`${input} w-full font-mono`} spellCheck={false} />
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
        </div>
        <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
          <Field label="Schedule" hint={preset === "custom" ? describeSchedule(form.schedule) : "times are UTC"}>
            <select value={preset} onChange={(e) => e.target.value !== "custom" && set({ schedule: e.target.value })} className={`${input} w-full`}>
              {SCHEDULE_PRESETS.map((s) => (
                <option key={s.value} value={s.value}>
                  {s.label}
                </option>
              ))}
              <option value="custom">Custom</option>
            </select>
          </Field>
          <Field label="Cron or interval" hint="sec min hour day month weekday, or 30m">
            <input value={form.schedule} onChange={(e) => set({ schedule: e.target.value })} className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
        </div>
        <div className="grid gap-3 sm:grid-cols-[minmax(0,2fr)_repeat(3,minmax(0,1fr))]">
          <Field label="Indices" hint="patterns; empty for every index">
            <input value={form.indices} onChange={(e) => set({ indices: e.target.value })} placeholder="logs-*, products" className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
          <Field label="Expire after">
            <input value={form.expireAfter} onChange={(e) => set({ expireAfter: e.target.value })} placeholder="30d" className={`${input} w-full font-mono`} spellCheck={false} />
          </Field>
          <Field label="Keep at least">
            <input value={form.minCount} onChange={(e) => set({ minCount: e.target.value.replace(/\D/g, "") })} inputMode="numeric" className={`${input} w-full font-mono`} />
          </Field>
          <Field label="Keep at most">
            <input value={form.maxCount} onChange={(e) => set({ maxCount: e.target.value.replace(/\D/g, "") })} inputMode="numeric" className={`${input} w-full font-mono`} />
          </Field>
        </div>
        <Check checked={form.includeGlobalState} onChange={(v) => set({ includeGlobalState: v })}>
          Include global state
        </Check>
        <RequestPreview method="PUT" path={path} body={body} onOpenConsole={onOpenConsole} />
        <div className="flex flex-wrap items-center gap-3">
          <button type="submit" disabled={busy || !id || !!problem} className={primary}>
            {busy ? <span className="shimmer">Saving…</span> : existing ? "Save policy" : "Create policy"}
          </button>
          {problem && <span className="text-[12px] text-danger">{problem}</span>}
          {existing && (
            <button type="button" onClick={() => onOpenConsole(consoleText("PUT", path, body))} className={btn}>
              <SquareTerminal className="size-3.5" /> Edit as JSON
            </button>
          )}
        </div>
        <ErrorNote error={error} />
      </form>
    </Panel>
  );
}
