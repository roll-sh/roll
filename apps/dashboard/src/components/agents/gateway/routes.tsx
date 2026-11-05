"use client";

/**
 * Routes and aliases (docs/code/gateway.md, "Routing"): where requests for a model go, in order,
 * the next target taking over on failover. A route whose targets ask for another name is an alias
 * ("fast" → Anthropic's claude-haiku-4-5, then Bedrock's). Without a route, a model goes to the
 * first enabled upstream that serves it, in the upstreams' order.
 */

import { ArrowRight, Pencil, Plus, Trash2, X } from "lucide-react";
import { useState } from "react";
import { GATEWAY_LIMITS, type RouteSpec, type RouteTarget, type UpstreamSpec } from "@/lib/agents-api";
import { useEscapeLayer } from "../escape-layers";
import { field, Note } from "../settings-ui";
import { cn, Select } from "../ui";
import { iconBtn, Labeled, primaryBtn, Problem, smallBtn } from "./shared";

/** An alias names a model of the owner's own: no target asks for it by that name. */
export const isAlias = (r: RouteSpec) => r.targets.every((t) => t.model && t.model !== r.model);

export function RoutesList({ routes, upstreams, locked, onSave }: { routes: RouteSpec[]; upstreams: UpstreamSpec[]; locked: boolean; onSave: (routes: RouteSpec[]) => Promise<string | null> }) {
  const [editing, setEditing] = useState<number | "new" | null>(null);
  const name = (id: string) => upstreams.find((u) => u.id === id)?.name ?? id;
  const put = async (at: number | "new", r: RouteSpec | null) => {
    const next = at === "new" ? [...routes, r!] : r ? routes.map((x, i) => (i === at ? r : x)) : routes.filter((_, i) => i !== at);
    const refused = await onSave(next);
    if (!refused) setEditing(null);
    return refused;
  };
  return (
    <div>
      {routes.length === 0 && editing !== "new" && <p className="px-3.5 pb-3 text-[12.5px] text-subtle">No routes: each model goes to the first upstream that serves it, in the order above.</p>}
      <ul className="divide-y divide-line">
        {routes.map((r, i) =>
          editing === i ? (
            <li key={r.model}>
              <RouteEditor initial={r} upstreams={upstreams} onSave={(x) => put(i, x)} onCancel={() => setEditing(null)} />
            </li>
          ) : (
            <li key={r.model} className="flex items-center gap-3 px-3.5 py-2.5 text-[12.5px]" aria-label={`Route ${r.model}`}>
              <div className="min-w-0 flex-1">
                <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                  <span className="font-mono text-[12px] text-fg">{r.model}</span>
                  {isAlias(r) && <Note>alias</Note>}
                </div>
                <p className="mt-0.5 flex min-w-0 flex-wrap items-center gap-1 text-[12px] text-muted">
                  {r.targets.map((t, j) => (
                    <span key={j} className="inline-flex min-w-0 flex-wrap items-center gap-1">
                      {j > 0 && <ArrowRight className="size-3 text-faint" aria-label="then" />}
                      <span className="whitespace-nowrap">{name(t.upstream)}</span>
                      {t.model && t.model !== r.model && <span className="font-mono text-[11px] break-all text-subtle">({t.model})</span>}
                    </span>
                  ))}
                </p>
              </div>
              <button type="button" disabled={locked} onClick={() => setEditing(i)} className={iconBtn} aria-label={`Edit the route for ${r.model}`}>
                <Pencil className="size-3.5" />
              </button>
              <button type="button" disabled={locked} onClick={() => void put(i, null)} className={cn(iconBtn, "hover:text-danger")} aria-label={`Remove the route for ${r.model}`}>
                <Trash2 className="size-3.5" />
              </button>
            </li>
          ),
        )}
      </ul>
      {editing === "new" ? (
        <RouteEditor upstreams={upstreams} onSave={(x) => put("new", x)} onCancel={() => setEditing(null)} />
      ) : (
        <div className="px-3.5 pb-3">
          <button type="button" disabled={locked || !upstreams.length || routes.length >= GATEWAY_LIMITS.routes} onClick={() => setEditing("new")} className={smallBtn}>
            <Plus className="size-3.5" /> Add a route or alias
          </button>
        </div>
      )}
    </div>
  );
}

function RouteEditor({ initial, upstreams, onSave, onCancel }: { initial?: RouteSpec; upstreams: UpstreamSpec[]; onSave: (r: RouteSpec) => Promise<string | null>; onCancel: () => void }) {
  const [model, setModel] = useState(initial?.model ?? "");
  const [targets, setTargets] = useState<RouteTarget[]>(initial?.targets ?? [{ upstream: upstreams[0]?.id ?? "" }]);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  useEscapeLayer(true, onCancel);
  const set = (i: number, t: Partial<RouteTarget>) => setTargets((ts) => ts.map((x, j) => (j === i ? { ...x, ...t } : x)));
  const save = async () => {
    if (!model.trim()) return setProblem("Name the model clients ask for (or a prefix ending in *).");
    setBusy(true);
    setProblem(null);
    const refused = await onSave({ model: model.trim(), targets: targets.map((t) => (t.model?.trim() ? { upstream: t.upstream, model: t.model.trim() } : { upstream: t.upstream })) });
    setBusy(false);
    if (refused) setProblem(refused);
  };
  return (
    <form
      aria-label={initial ? `Edit the route for ${initial.model}` : "New route"}
      className="space-y-3 bg-panel-2/60 px-3.5 py-3"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <Labeled label="Clients ask for" hint="A model name or an alias of your own (fast), exact or a prefix ending in *.">
        <input aria-label="Model" value={model} onChange={(e) => setModel(e.target.value)} placeholder="fast" spellCheck={false} className={cn(field, "w-full font-mono text-[12px] sm:w-72")} />
      </Labeled>
      <Labeled label="Goes to, in order" hint="The first healthy one serves; the next takes over on failover. A model name here renames it for that upstream.">
        <ol className="space-y-1.5">
          {targets.map((t, i) => (
            <li key={i} className="flex flex-wrap items-center gap-2">
              <span className="w-4 text-right font-mono text-[11px] tabular-nums text-subtle">{i + 1}</span>
              <Select label={`Target ${i + 1} upstream`} className="w-44" value={t.upstream || null} onChange={(v) => set(i, { upstream: v })} options={upstreams.map((u) => ({ value: u.id, label: u.name }))} />
              <input aria-label={`Target ${i + 1} model`} value={t.model ?? ""} onChange={(e) => set(i, { model: e.target.value })} placeholder="same name" spellCheck={false} className={cn(field, "min-w-36 flex-1 font-mono text-[12px]")} />
              <button type="button" disabled={targets.length === 1} onClick={() => setTargets((ts) => ts.filter((_, j) => j !== i))} className={iconBtn} aria-label={`Remove target ${i + 1}`}>
                <X className="size-3.5" />
              </button>
            </li>
          ))}
        </ol>
        <button type="button" disabled={targets.length >= GATEWAY_LIMITS.targets} onClick={() => setTargets((ts) => [...ts, { upstream: upstreams.find((u) => !ts.some((x) => x.upstream === u.id))?.id ?? upstreams[0]?.id ?? "" }])} className={cn(smallBtn, "mt-1")}>
          <Plus className="size-3.5" /> Then
        </button>
      </Labeled>
      {problem && <Problem>{problem}</Problem>}
      <div className="flex items-center justify-end gap-2">
        <button type="button" onClick={onCancel} className="h-8 rounded-lg px-3 text-[12.5px] text-muted hover:text-fg">
          Cancel
        </button>
        <button type="submit" disabled={busy} className={primaryBtn}>
          {busy ? <span className="shimmer">Saving…</span> : initial ? "Save" : "Add route"}
        </button>
      </div>
    </form>
  );
}
