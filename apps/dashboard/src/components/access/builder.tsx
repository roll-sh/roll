import { Check, Hand } from "lucide-react";
import { useState } from "react";
import {
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
}: {
  catalog: Catalog;
  rules: Rule[];
  onChange: (rules: Rule[], preset: string | null) => void;
}) {
  const ws = useWorkspace();
  const draft = draftOf(rules);
  const [preset, setPreset] = useState<string | null>(null);
  const products = ["*", ...catalog.products];

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

          <Matrix draft={draft} products={products} onChange={edit} />
        </>
      )}
    </div>
  );
}

function Matrix({ draft, products, onChange }: { draft: Draft; products: string[]; onChange: (d: Draft) => void }) {
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
            {ACTIONS.map((a) => (
              <th key={a} className="px-2 py-2 text-center font-normal" title={ACTION_HELP[a]}>
                {a}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {products.map((p) => (
            <tr key={p} className={`border-b border-line last:border-0 ${p === "*" ? "bg-white/[0.02]" : ""}`}>
              <td className="px-3 py-1.5 text-muted">{productName(p)}</td>
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
