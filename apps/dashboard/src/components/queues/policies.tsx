"use client";

import { Pencil, Plus, Trash2 } from "lucide-react";
import { useMemo, useState } from "react";
import { type Policy, qapi } from "@/lib/queues-api";
import { JsonEditor, parseObject } from "./args-editor";
import { useAction, usePoll } from "./hooks";
import { QueuesShell } from "./shell";
import { Badge, btn, dangerIconBtn, Empty, ErrorNote, Field, ICON, iconBtn, input, JsonInline, Panel, Pending, primary, td, th, VhostSelect } from "./ui";

const APPLY_TO = ["all", "queues", "classic_queues", "quorum_queues", "streams", "exchanges"];

const SUGGESTED: { key: string; value: unknown; title: string }[] = [
  { key: "max-length", value: 100000, title: "Maximum ready messages" },
  { key: "max-length-bytes", value: 1073741824, title: "Maximum total body bytes" },
  { key: "overflow", value: "reject-publish", title: "drop-head | reject-publish | reject-publish-dlx" },
  { key: "message-ttl", value: 86400000, title: "Per-message TTL in ms" },
  { key: "expires", value: 3600000, title: "Delete unused queues after ms" },
  { key: "dead-letter-exchange", value: "dlx", title: "Dead-letter exchange" },
  { key: "dead-letter-routing-key", value: "dead", title: "Dead-letter routing key" },
  { key: "delivery-limit", value: 20, title: "Quorum: redeliveries before dead-lettering" },
  { key: "max-age", value: "7D", title: "Streams: retention by age" },
  { key: "stream-max-segment-size-bytes", value: 500000000, title: "Streams: segment size" },
  { key: "queue-leader-locator", value: "balanced", title: "client-local | balanced" },
  { key: "alternate-exchange", value: "unrouted", title: "Exchanges: unroutable messages go here" },
  { key: "federation-upstream-set", value: "all", title: "Federate from upstreams" },
];

interface Draft {
  vhost: string;
  name: string;
  pattern: string;
  apply_to: string;
  priority: string;
  definition: string;
  editing: boolean;
}

const blank = (vhost: string): Draft => ({
  vhost,
  name: "",
  pattern: "^",
  apply_to: "queues",
  priority: "0",
  definition: JSON.stringify({ "max-length": 100000, overflow: "reject-publish" }, null, 2),
  editing: false,
});

export function Policies() {
  const poll = usePoll("policies", qapi.policies);
  const vhostsPoll = usePoll("vhosts", qapi.vhosts, 15_000);
  const names = useMemo(() => vhostsPoll.data?.map((v) => v.name) ?? [], [vhostsPoll.data]);
  const { busy, error, setError, run } = useAction();
  const [draft, setDraft] = useState<Draft | null>(null);
  const policies = poll.data ?? [];

  const edit = (p: Policy) =>
    setDraft({
      vhost: p.vhost,
      name: p.name,
      pattern: p.pattern,
      apply_to: p.apply_to,
      priority: String(p.priority),
      definition: JSON.stringify(p.definition, null, 2),
      editing: true,
    });

  const remove = (p: Policy) => {
    if (!confirm(`Delete policy ${p.name} on vhost ${p.vhost}?`)) return;
    const before = poll.data;
    poll.mutate((d) => d.filter((x) => !(x.vhost === p.vhost && x.name === p.name)));
    run(`del-${p.vhost}-${p.name}`, async () => {
      await qapi.deletePolicy(p.vhost, p.name);
      await poll.refresh();
    }, { rollback: () => before && poll.mutate(() => before) });
  };

  const save = async (d: Draft) => {
    const def = parseObject(d.definition);
    if (def.error || !def.value) return setError(`Definition: ${def.error}`);
    if (!Object.keys(def.value).length) return setError("Definition is empty");
    try {
      new RegExp(d.pattern);
    } catch {
      return setError("Pattern isn't a valid regular expression");
    }
    const priority = Number(d.priority);
    if (!Number.isInteger(priority)) return setError("Priority must be a whole number");
    const out = await run("save", async () => {
      await qapi.putPolicy(d.vhost, d.name.trim(), { pattern: d.pattern, apply_to: d.apply_to, definition: def.value!, priority });
      await poll.refresh();
      return true;
    });
    if (out) setDraft(null);
  };

  return (
    <QueuesShell
      title="Policies"
      crumb="policies"
      description="Apply limits, TTLs and dead-lettering to every queue or exchange whose name matches a pattern."
      loading={poll.loading && !poll.error}
      offline={poll.offline && !poll.data}
      error={poll.error}
      actions={
        <button onClick={() => setDraft(draft && !draft.editing ? null : blank(names[0] ?? "/"))} className={primary}>
          <Plus {...ICON} /> New policy
        </button>
      }
    >
      {draft && (
        <PolicyEditor
          key={draft.editing ? `${draft.vhost}\u0000${draft.name}` : "new"}
          draft={draft}
          vhosts={names.length ? names : [draft.vhost]}
          busy={busy === "save"}
          onChange={setDraft}
          onCancel={() => setDraft(null)}
          onSave={save}
        />
      )}
      <ErrorNote error={error} onClose={() => setError(null)} />

      <Panel title="Policies" meta={poll.data ? String(policies.length) : undefined}>
        {policies.length === 0 ? (
          <Empty>No policies. Policies change queues without redeclaring them, so clients don&apos;t need to know.</Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr>
                  <th className={th}>Name</th>
                  <th className={th}>Pattern</th>
                  <th className={th}>Applies to</th>
                  <th className={th}>Definition</th>
                  <th className={`${th} text-right`}>Priority</th>
                  <th className={th} />
                </tr>
              </thead>
              <tbody>
                {[...policies]
                  .sort((a, b) => a.vhost.localeCompare(b.vhost) || b.priority - a.priority || a.name.localeCompare(b.name))
                  .map((p) => (
                    <tr key={`${p.vhost}\u0000${p.name}`} className="transition hover:bg-panel-2/60">
                      <td className={`${td} whitespace-nowrap`}>
                        <span className="flex items-center gap-2">
                          <span className="font-mono text-[12px]">{p.name}</span>
                          {p.operator && <Badge title="Operator policy: set by the operator, merged with user policies">operator</Badge>}
                        </span>
                        <span className="font-mono text-[11px] text-subtle">vhost {p.vhost}</span>
                      </td>
                      <td className={`${td} font-mono text-[12px] text-muted`}>{p.pattern}</td>
                      <td className={`${td} text-[12px] text-muted`}>{p.apply_to.replace(/_/g, " ")}</td>
                      <td className={`${td} max-w-0 w-full`}>
                        <span className="block truncate">
                          <JsonInline value={p.definition} />
                        </span>
                      </td>
                      <td className={`${td} text-right font-mono text-[12px]`}>{p.priority}</td>
                      <td className={`${td} text-right whitespace-nowrap`}>
                        {!p.operator && (
                          <>
                            <button className={iconBtn} disabled={busy !== null} onClick={() => edit(p)} title="Edit">
                              <Pencil {...ICON} />
                            </button>
                            <button className={dangerIconBtn} disabled={busy !== null} onClick={() => remove(p)} title="Delete">
                              <Trash2 {...ICON} />
                            </button>
                          </>
                        )}
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </QueuesShell>
  );
}

function PolicyEditor({
  draft,
  vhosts,
  busy,
  onChange,
  onCancel,
  onSave,
}: {
  draft: Draft;
  vhosts: string[];
  busy: boolean;
  onChange: (d: Draft) => void;
  onCancel: () => void;
  onSave: (d: Draft) => void;
}) {
  const set = (patch: Partial<Draft>) => onChange({ ...draft, ...patch });
  const valid = draft.name.trim() && !parseObject(draft.definition).error;
  return (
    <form
      className="rise grid gap-4 rounded-2xl border border-line bg-panel p-4 lg:grid-cols-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (valid) onSave(draft);
      }}
    >
      <div className="space-y-3">
        <p className="text-[13px] font-medium">{draft.editing ? <>Edit <span className="font-mono">{draft.name}</span></> : "New policy"}</p>
        <div className="grid grid-cols-2 gap-2">
          <Field label="Vhost">
            <VhostSelect value={draft.vhost} vhosts={vhosts} onChange={(vhost) => set({ vhost })} className="w-full" />
          </Field>
          <Field label="Name">
            <input
              value={draft.name}
              onChange={(e) => set({ name: e.target.value })}
              disabled={draft.editing}
              autoFocus={!draft.editing}
              placeholder="limit-orders"
              spellCheck={false}
              className={`${input} w-full font-mono text-[12px] disabled:opacity-60`}
            />
          </Field>
          <Field label="Pattern (regex)" hint="^ matches every name">
            <input value={draft.pattern} onChange={(e) => set({ pattern: e.target.value })} spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Priority" hint="Highest wins when several match">
            <input value={draft.priority} onChange={(e) => set({ priority: e.target.value })} inputMode="numeric" className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Apply to" className="col-span-2">
            <select value={draft.apply_to} onChange={(e) => set({ apply_to: e.target.value })} className={`${input} w-full`}>
              {APPLY_TO.map((a) => (
                <option key={a} value={a}>
                  {a.replace(/_/g, " ")}
                </option>
              ))}
            </select>
          </Field>
        </div>
      </div>
      <div className="flex flex-col gap-3">
        <JsonEditor label="Definition" value={draft.definition} onChange={(definition) => set({ definition })} suggestions={SUGGESTED} rows={7} />
        <div className="mt-auto flex justify-end gap-2">
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
          <button type="submit" disabled={busy || !valid} className={primary}>
            {busy ? <Pending>Saving…</Pending> : draft.editing ? "Save policy" : "Create policy"}
          </button>
        </div>
      </div>
    </form>
  );
}
