import { Check, ChevronRight, Hand } from "lucide-react";
import { Fragment, useEffect, useState } from "react";
import {
  accessApi,
  ACTION_HELP,
  ACTIONS,
  describeRule,
  draftOf,
  needsProject,
  nextEffect,
  presetRules,
  productName,
  rulesOf,
  type Action,
  type Catalog,
  type Draft,
  type Effect,
  type Preset,
  type Rule,
} from "@/lib/access-api";
import { loadInventory, resourcesByProduct } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";
import { input } from "../ui";

const ENVIRONMENTS = ["development", "staging", "production", "preview"];

/**
 * The permission builder (docs/permissions.md): start from a preset, then choose where it applies
 * (everything, or one workspace project, optionally only some environments) and, per product,
 * which actions run (✓) and which wait for your approval (ask). The new token and the OAuth
 * consent screen both use it.
 */
export function Builder({
  catalog,
  rules,
  onChange,
  askOtherwise = false,
}: {
  catalog: Catalog;
  rules: Rule[];
  onChange: (rules: Rule[], preset: string | null) => void;
  /** Whether what the rules don't allow asks instead (the summary's last line). */
  askOtherwise?: boolean;
}) {
  const ws = useWorkspace();
  const draft = draftOf(rules);
  const [preset, setPreset] = useState<string | null>(null);
  const products = ["*", ...catalog.products];
  // Every product's resources by name, for "only these" (what this rolld runs).
  const [inventory, setInventory] = useState<Record<string, { id: string; name: string }[]>>({});
  useEffect(() => {
    loadInventory()
      .then((inv) => setInventory(resourcesByProduct(inv)))
      .catch(() => {});
  }, []);

  const apply = (p: Preset) => {
    setPreset(p.id);
    onChange(presetRules(p, draft?.project ?? undefined), p.id);
  };
  const edit = (d: Draft) => {
    setPreset(null);
    onChange(rulesOf(d), null);
  };

  return (
    <div className="space-y-5">
      <fieldset>
        <legend className="mb-2 text-[12px] text-subtle">Start from</legend>
        <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
          {catalog.presets.map((p) => (
            <button
              key={p.id}
              type="button"
              onClick={() => apply(p)}
              aria-pressed={preset === p.id}
              className={`rounded-lg border p-3 text-left transition-colors duration-150 ${
                preset === p.id ? "border-fg bg-white/[0.03]" : "border-line hover:border-line-2"
              }`}
            >
              <span className="block text-[13px] font-medium">{p.name}</span>
              <span className="mt-1 block text-[12px] leading-snug text-muted">{p.description}</span>
            </button>
          ))}
        </div>
      </fieldset>

      {draft === null ? (
        <div className="space-y-1.5">
          <p className="text-[12px] text-subtle">These rules are more specific than the builder shows; pick a preset to start over.</p>
          {rules.map((r, i) => (
            <p key={i} className="rounded-lg border border-line bg-bg px-3 py-2 text-[12px] text-muted">
              {describeRule(r)}
            </p>
          ))}
        </div>
      ) : (
        <>
          <fieldset className="grid gap-4 sm:grid-cols-2">
            <label className="block space-y-1.5">
              <span className="block text-[12px] text-subtle">Where</span>
              <select
                value={draft.project ?? ""}
                onChange={(e) => edit({ ...draft, project: e.target.value || null })}
                className={`${input} w-full`}
              >
                <option value="">Every resource, including new ones</option>
                {ws.projects.map((p) => (
                  <option key={p.id} value={p.id}>
                    Only project {p.name}
                  </option>
                ))}
              </select>
              {draft.project === null && preset && catalog.presets.find((p) => p.id === preset && needsProject(p)) && (
                <span className="block text-[11px] text-subtle">Pick a project for this preset to mean "this project only".</span>
              )}
            </label>
            <div className="space-y-1.5">
              <span className="block text-[12px] text-subtle">Environments</span>
              <div className="flex flex-wrap gap-1.5">
                {ENVIRONMENTS.map((env) => {
                  const on = draft.environments.includes(env);
                  return (
                    <button
                      key={env}
                      type="button"
                      aria-pressed={on}
                      onClick={() => edit({ ...draft, environments: on ? draft.environments.filter((e) => e !== env) : [...draft.environments, env] })}
                      className={`rounded-full border px-2.5 py-1 text-[12px] transition-colors duration-150 ${
                        on ? "border-fg text-fg" : "border-line text-muted hover:border-line-2"
                      }`}
                    >
                      {env}
                    </button>
                  );
                })}
              </div>
              <span className="block text-[11px] text-subtle">
                {draft.environments.length ? "Only resources in these environments." : "Any environment."}
              </span>
            </div>
          </fieldset>

          <Matrix draft={draft} products={products} inventory={inventory} onChange={edit} />
        </>
      )}
      <Summary rules={rules} askOtherwise={askOtherwise} inventory={inventory} projectName={(id) => ws.projects.find((p) => p.id === id)?.name ?? id} />
    </div>
  );
}

function Matrix({
  draft,
  products,
  inventory,
  onChange,
}: {
  draft: Draft;
  products: string[];
  inventory: Record<string, { id: string; name: string }[]>;
  onChange: (d: Draft) => void;
}) {
  const [open, setOpen] = useState<string | null>(null);
  const setIds = (product: string, ids: string[]) => {
    const next = { ...draft.ids };
    if (ids.length) next[product] = ids;
    else delete next[product];
    onChange({ ...draft, ids: next });
  };
  const set = (product: string, action: Action, effect: Effect | undefined) => {
    const row = { ...draft.matrix[product] };
    if (effect) row[action] = effect;
    else delete row[action];
    onChange({ ...draft, matrix: { ...draft.matrix, [product]: row } });
  };
  return (
    <div className="overflow-x-auto rounded-lg border border-line">
      <table className="w-full min-w-[560px] border-collapse text-[12px]">
        <thead>
          <tr className="border-b border-line text-subtle">
            <th className="px-3 py-2 text-left font-normal">Product</th>
            <th className="px-2 py-2 text-left font-normal">Which</th>
            {ACTIONS.map((a) => (
              <th key={a} className="px-2 py-2 text-center font-normal" title={ACTION_HELP[a]}>
                {a}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {products.map((p) => (
            <Fragment key={p}>
            <tr className={`border-b border-line last:border-0 ${p === "*" ? "bg-white/[0.02]" : ""}`}>
              <td className="px-3 py-1.5 text-muted">{productName(p)}</td>
              <td className="px-2 py-1">
                {p === "*" ? (
                  <span className="text-subtle">{draft.project ? "the project" : "all"}</span>
                ) : (
                  <button
                    type="button"
                    onClick={() => setOpen(open === p ? null : p)}
                    aria-expanded={open === p}
                    className="flex items-center gap-1 rounded-md px-1.5 py-0.5 text-muted hover:bg-white/[0.04] hover:text-fg"
                  >
                    <ChevronRight className={`size-3 transition-transform duration-150 ${open === p ? "rotate-90" : ""}`} />
                    {draft.ids[p]?.length ? `${draft.ids[p].length} chosen` : draft.project ? "the project" : "all"}
                  </button>
                )}
              </td>
              {ACTIONS.map((a) => {
                const e = draft.matrix[p]?.[a];
                return (
                  <td key={a} className="px-2 py-1 text-center">
                    <button
                      type="button"
                      onClick={() => set(p, a, nextEffect(e))}
                      aria-label={`${productName(p)} ${a}: ${e ?? "off"}`}
                      title={`${ACTION_HELP[a]}: ${e === "allow" ? "allowed" : e === "ask" ? "asks you first" : "not allowed"}. Click to change.`}
                      className={`inline-grid h-6 w-12 place-items-center rounded-md border transition-colors duration-150 ${
                        e === "allow"
                          ? "border-fg/40 bg-fg/10 text-fg"
                          : e === "ask"
                            ? "border-accent/50 text-accent"
                            : "border-line text-subtle hover:border-line-2"
                      }`}
                    >
                      {e === "allow" ? <Check className="size-3.5" /> : e === "ask" ? <Hand className="size-3.5" /> : "–"}
                    </button>
                  </td>
                );
              })}
            </tr>
            {open === p && (
              <tr className="border-b border-line bg-white/[0.015]">
                <td colSpan={ACTIONS.length + 2} className="px-3 py-2.5">
                  <Pick product={p} chosen={draft.ids[p] ?? []} known={inventory[p] ?? []} onChange={(ids) => setIds(p, ids)} />
                </td>
              </tr>
            )}
            </Fragment>
          ))}
        </tbody>
      </table>
      <p className="flex flex-wrap gap-x-4 gap-y-1 border-t border-line px-3 py-2 text-[11px] text-subtle">
        <span className="flex items-center gap-1">
          <Check className="size-3" /> runs
        </span>
        <span className="flex items-center gap-1 text-accent">
          <Hand className="size-3" /> waits for your approval
        </span>
        <span>– not allowed. "Everything" covers every product, including ones added later.</span>
      </p>
    </div>
  );
}

/** "Only these": a product's resources to tick by name, or ids typed (products the inventory doesn't list). */
function Pick({ product, chosen, known, onChange }: { product: string; chosen: string[]; known: { id: string; name: string }[]; onChange: (ids: string[]) => void }) {
  const [text, setText] = useState("");
  const toggle = (id: string) => onChange(chosen.includes(id) ? chosen.filter((c) => c !== id) : [...chosen, id]);
  const extra = chosen.filter((c) => !known.some((k) => k.id === c));
  return (
    <div className="space-y-2">
      <p className="text-[11px] text-subtle">
        {chosen.length ? `Only these ${productName(product)} resources, wherever they are.` : `Every ${productName(product)} resource the place above covers. Tick some to narrow it.`}
      </p>
      {known.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {known.map((k) => (
            <label key={k.id} className={`flex cursor-pointer items-center gap-1.5 rounded-md border px-2 py-1 ${chosen.includes(k.id) ? "border-fg/40 text-fg" : "border-line text-muted"}`}>
              <input type="checkbox" checked={chosen.includes(k.id)} onChange={() => toggle(k.id)} className="size-3" />
              {k.name}
              {k.name !== k.id && <span className="font-mono text-[10px] text-subtle">{k.id}</span>}
            </label>
          ))}
        </div>
      )}
      {product === "search" && known.map((k) => <Indices key={k.id} cluster={k} chosen={chosen} toggle={toggle} />)}
      <form
        className="flex flex-wrap items-center gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          const ids = text.split(/[\s,]+/).filter(Boolean);
          if (ids.length) onChange([...new Set([...chosen, ...ids])]);
          setText("");
        }}
      >
        {extra.map((id) => (
          <button key={id} type="button" onClick={() => toggle(id)} className="rounded-md border border-fg/40 px-2 py-0.5 font-mono text-[11px] text-fg" title="Remove">
            {id} ×
          </button>
        ))}
        <input
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder={product === "search" ? "cl_… or cl_…/logs-*" : "Add by id"}
          title={product === "search" ? "A cluster, or some of its indices: cl_…/logs-* (only through their index routes)" : undefined}
          className={`${input} h-7 w-44 font-mono text-[11px]`}
          aria-label={`${productName(product)} resource ids`}
        />
      </form>
    </div>
  );
}

/**
 * Some indices of a search cluster, ticked by name (`cl_1/logs-a`): loaded when opened, since
 * listing them wakes the cluster. A pattern (`cl_1/logs-*`) is typed in "Add by id".
 */
function Indices({ cluster, chosen, toggle }: { cluster: { id: string; name: string }; chosen: string[]; toggle: (id: string) => void }) {
  const [names, setNames] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = () => {
    setError(null);
    accessApi
      .searchIndices(cluster.id)
      .then((l) => setNames(l.map((i) => i.index).filter((n) => !n.startsWith(".")).sort()))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  };
  if (names === null) {
    return (
      <p className="text-[11px] text-subtle">
        <button type="button" onClick={load} className="text-accent hover:underline">
          Only some indices of {cluster.name}…
        </button>
        {error && <span className="text-danger"> {error}</span>}
      </p>
    );
  }
  return (
    <div className="space-y-1">
      <p className="text-[11px] text-subtle">
        Indices of {cluster.name}: a token with only these reaches them through their index routes, never the cluster itself.
      </p>
      <div className="flex flex-wrap gap-1.5">
        {names.length === 0 && <span className="text-[11px] text-subtle">No indices yet.</span>}
        {names.map((n) => {
          const id = `${cluster.id}/${n}`;
          return (
            <label key={id} className={`flex cursor-pointer items-center gap-1.5 rounded-md border px-2 py-1 font-mono text-[11px] ${chosen.includes(id) ? "border-fg/40 text-fg" : "border-line text-muted"}`}>
              <input type="checkbox" checked={chosen.includes(id)} onChange={() => toggle(id)} className="size-3" aria-label={`${cluster.name} index ${n}`} />
              {n}
            </label>
          );
        })}
      </div>
    </div>
  );
}

/** What the token can do, in words, with resources by name; and what happens to the rest. */
function Summary({
  rules,
  askOtherwise,
  inventory,
  projectName,
}: {
  rules: Rule[];
  askOtherwise: boolean;
  inventory: Record<string, { id: string; name: string }[]>;
  projectName: (id: string) => string;
}) {
  const resourceName = (product: string, id: string) => inventory[product]?.find((r) => r.id === id)?.name ?? id;
  return (
    <div className="space-y-1 rounded-lg border border-line px-3 py-2.5 text-[12px]">
      <p className="text-subtle">This token can</p>
      {rules.length === 0 ? (
        <p className="text-muted">do nothing yet.</p>
      ) : (
        rules.map((r, i) => (
          <p key={i} className="text-muted">
            {describeRule(r, projectName, resourceName)}
          </p>
        ))
      )}
      <p className="text-subtle">{askOtherwise ? "Anything else waits for your approval." : "Anything else is refused."}</p>
    </div>
  );
}
