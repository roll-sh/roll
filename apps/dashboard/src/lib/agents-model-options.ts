/**
 * A model's options as the daemon describes them (agent-proto `ModelOption`, T3 Code's
 * `ProviderOptionDescriptor`), and what the picker shows for a choice. Ported from T3's
 * `packages/client-runtime/src/modelTraits.ts` (`buildTraitsTriggerDisplay`, the reasoning step of
 * `getReasoningTraitStep`) and `ComposerModelEffort.tsx` (`describeComposerEffort`, `splitSpeed`),
 * with the speed and context window found by their role rather than by provider and id.
 *
 * The reasoning option's value is a thread's `effort`; every other option's is in its `options`.
 * Nothing picked means the model's default, so only what the user picks is sent.
 */

import { effortLabel, EFFORT_OPTION, type ModelInfo, type ModelOption, type ModelOptions, type OptionValue } from "./agents-api";

export type SelectOption = Extract<ModelOption, { type: "select" }>;
export type BooleanOption = Extract<ModelOption, { type: "boolean" }>;

/** The model's reasoning levels, if it has a choice of them. */
export function reasoningOf(m: ModelInfo | undefined): SelectOption | undefined {
  const o = m?.options.find((x) => x.role === "reasoning");
  return o?.type === "select" ? o : undefined;
}

/** The reasoning efforts a model takes, lowest first. */
export const modelEfforts = (m: ModelInfo | undefined): string[] => reasoningOf(m)?.choices.map((c) => c.id) ?? [];

/** Its speed (Claude's Fast Mode, Codex's service tiers). */
export const speedOf = (m: ModelInfo | undefined): ModelOption | undefined => m?.options.find((x) => x.role === "speed");

/** Its context window choice. */
export function contextOf(m: ModelInfo | undefined): SelectOption | undefined {
  const o = m?.options.find((x) => x.role === "context_window");
  return o?.type === "select" ? o : undefined;
}

/** Everything but the reasoning and the speed: the picker's column rows under the levels. */
export const otherOptions = (m: ModelInfo | undefined): ModelOption[] => m?.options.filter((o) => o.role !== "reasoning" && o.role !== "speed") ?? [];

/** Whether `value` is one the option takes. */
export function accepts(o: ModelOption, value: OptionValue): boolean {
  return o.type === "select" ? typeof value === "string" && o.choices.some((c) => c.id === value) : typeof value === "boolean";
}

/** The value an option runs with: the one picked if it takes it, else its default. */
export function optionValue(o: ModelOption, picked: ModelOptions | undefined): OptionValue | undefined {
  const v = picked?.[o.id];
  if (v !== undefined && accepts(o, v)) return v;
  return o.default;
}

/** The effort a model runs with: the one picked if it offers it, else its default. */
export function effortValue(m: ModelInfo | undefined, effort: string | null | undefined): string | undefined {
  const r = reasoningOf(m);
  if (!r) return undefined;
  return effort && r.choices.some((c) => c.id === effort) ? effort : r.default;
}

export const choiceLabel = (o: SelectOption, id: string | undefined) => o.choices.find((c) => c.id === id)?.label;

/** Whether a speed option is past its standard: Fast Mode on, or a tier other than the first ("Standard"). */
export function isFast(o: ModelOption, value: OptionValue | undefined): boolean {
  if (o.type === "boolean") return value === true;
  return typeof value === "string" && value !== o.choices[0]?.id;
}

/** The options of `wanted` that `m` takes (agent-proto `ModelInfo::accepted`): a model switch keeps those. */
export function acceptedOptions(m: ModelInfo | undefined, wanted: ModelOptions | undefined): ModelOptions {
  const out: ModelOptions = {};
  for (const [id, v] of Object.entries(wanted ?? {})) {
    const o = m?.options.find((x) => x.id === id);
    if (o && o.role !== "reasoning" && accepts(o, v)) out[id] = v;
  }
  return out;
}

/** What the trigger shows after the model's name (T3's `describeComposerEffort`). */
export interface Traits {
  /** The reasoning level's label ("High"). */
  effort: string | null;
  /** The speed is past standard; its label ("Fast"). */
  fast: string | null;
  /** The context window's label ("1M"), for a model with a choice of them. */
  context: string | null;
  /** Everything, for a title and screen readers: "High · Fast · 1M". */
  detail: string;
}

export function traits(m: ModelInfo | undefined, effort: string | null | undefined, options: ModelOptions | undefined): Traits {
  const r = reasoningOf(m);
  const effortText = r ? (choiceLabel(r, effortValue(m, effort)) ?? null) : null;
  const s = speedOf(m);
  const sv = s ? optionValue(s, options) : undefined;
  const fast = s && isFast(s, sv) ? (s.type === "select" ? (choiceLabel(s, sv as string) ?? "Fast") : "Fast") : null;
  const c = contextOf(m);
  const context = c ? (choiceLabel(c, optionValue(c, options) as string | undefined) ?? null) : null;
  return { effort: effortText, fast, context, detail: [effortText, fast, context].filter(Boolean).join(" · ") };
}

/** The next reasoning level, wrapping (T3's ⌘⇧E step through `getReasoningTraitStep`). */
export function nextEffort(m: ModelInfo | undefined, effort: string | null | undefined, by: 1 | -1 = 1): string | null {
  const r = reasoningOf(m);
  if (!r || r.choices.length < 2) return null;
  const at = r.choices.findIndex((c) => c.id === effortValue(m, effort));
  return r.choices[(at + by + r.choices.length) % r.choices.length].id;
}

/** Reasoning levels as a model option (for models written by hand: the demo, tests). */
export function reasoningLevels(levels: string[], def?: string): SelectOption {
  return { type: "select", id: EFFORT_OPTION, label: "Reasoning", choices: levels.map((e) => ({ id: e, label: effortLabel(e) })), ...(def ? { default: def } : {}), role: "reasoning" };
}
