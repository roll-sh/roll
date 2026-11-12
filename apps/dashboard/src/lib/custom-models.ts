/**
 * Custom models' options (T3 Code's `CustomModelEditor.tsx` and `customModelEditor.logic.ts`,
 * MIT): what the composer offers for a model the owner added by slug. A draft is what the editor
 * edits; `validateDraft` says what's wrong in T3's words (numbered as the editor shows them), and
 * `modelFromDraft` makes the `CustomModel` the machine's settings keep (agent-proto's
 * `check_custom_options` checks it again). Presets are roll's ids for each provider's options
 * (the reasoning levels are always `effort`, the thread's effort).
 */

import type { CustomModel, ModelInfo, ModelOption, OptionRole } from "./agents-api";

export interface DraftChoice {
  key: string;
  id: string;
  label: string;
}

export interface DraftOption {
  key: string;
  id: string;
  label: string;
  type: "select" | "boolean";
  choices: DraftChoice[];
  /** A select's default choice (its `key`), or a switch's default. */
  defaultChoice: string | null;
  defaultOn: boolean;
  role: OptionRole | null;
}

export interface CustomModelDraft {
  slug: string;
  name: string;
  options: DraftOption[];
}

let next = 0;
const key = () => `k${++next}`;

const choice = (id: string, label: string): DraftChoice => ({ key: key(), id, label });

/** One option to start from, by provider (T3's `DESCRIPTOR_PRESETS_BY_KIND`, in roll's ids). */
export interface Preset {
  id: string;
  label: string;
  make: () => DraftOption;
}

const select = (id: string, label: string, choices: [string, string][], def: string, role: OptionRole | null = null): (() => DraftOption) => () => {
  const cs = choices.map(([i, l]) => choice(i, l));
  return { key: key(), id, label, type: "select", choices: cs, defaultChoice: cs.find((c) => c.id === def)?.key ?? null, defaultOn: false, role };
};
const toggle = (id: string, label: string, role: OptionRole | null = null): (() => DraftOption) => () => ({ key: key(), id, label, type: "boolean", choices: [], defaultChoice: null, defaultOn: false, role });

const EFFORTS: [string, string][] = [
  ["low", "Low"],
  ["medium", "Medium"],
  ["high", "High"],
  ["xhigh", "Extra High"],
];

export function presetsFor(provider: string): Preset[] {
  if (provider === "claude") {
    return [
      { id: "effort", label: "Reasoning", make: select("effort", "Reasoning", [...EFFORTS, ["max", "Max"]], "high", "reasoning") },
      { id: "fastMode", label: "Fast Mode", make: toggle("fastMode", "Fast Mode", "speed") },
      { id: "thinking", label: "Thinking", make: toggle("thinking", "Thinking") },
    ];
  }
  if (provider === "codex") {
    return [
      { id: "effort", label: "Reasoning", make: select("effort", "Reasoning", EFFORTS, "medium", "reasoning") },
      { id: "serviceTier", label: "Speed", make: select("serviceTier", "Speed", [["default", "Standard"], ["fast", "Fast"]], "default", "speed") },
    ];
  }
  // An ACP agent: its reasoning option (as the thread's effort); anything else by its own id.
  return [{ id: "effort", label: "Reasoning", make: select("effort", "Reasoning", EFFORTS, "medium", "reasoning") }];
}

/** A described option as the editor edits it. */
export function draftOption(o: ModelOption): DraftOption {
  if (o.type === "boolean") return { key: key(), id: o.id, label: o.label, type: "boolean", choices: [], defaultChoice: null, defaultOn: o.default, role: o.role ?? null };
  const choices = o.choices.map((c) => choice(c.id, c.label));
  return { key: key(), id: o.id, label: o.label, type: "select", choices, defaultChoice: choices.find((c) => c.id === o.default)?.key ?? null, defaultOn: false, role: o.role ?? null };
}

/** What "Copy from…" copies: a model's options, without Claude's context window (T3 leaves it out). */
export function copyFrom(model: ModelInfo): DraftOption[] {
  return model.options.filter((o) => o.role !== "context_window").map(draftOption);
}

export function draftOf(m: CustomModel): CustomModelDraft {
  return { slug: m.id, name: m.name ?? "", options: (m.options ?? []).map(draftOption) };
}

/** T3's `validateDraft`. */
export function validateDraft(d: CustomModelDraft): string | null {
  const ids = new Set<string>();
  for (const [i, o] of d.options.entries()) {
    const at = `Option ${i + 1}`;
    const id = o.id.trim();
    if (!id) return `${at} needs an id.`;
    if (ids.has(id)) return `${at}: id "${id}" is used twice.`;
    ids.add(id);
    if (!o.label.trim()) return `${at} needs a label.`;
    if (id === "effort" && o.type !== "select") return `${at}: the reasoning levels are a list of choices with the id effort.`;
    if (o.type !== "select") continue;
    if (o.choices.length === 0) return `${at} needs at least one choice.`;
    const seen = new Set<string>();
    for (const c of o.choices) {
      const v = c.id.trim();
      if (!v) return `${at} has a choice without a value.`;
      if (seen.has(v)) return `${at}: choice "${v}" is used twice.`;
      seen.add(v);
    }
  }
  return null;
}

/** T3's `definitionFromDraft`: trimmed, a choice without a label shows its value, no options is the provider's. */
export function modelFromDraft(d: CustomModelDraft): CustomModel {
  const options: ModelOption[] = d.options.map((o) => {
    const id = o.id.trim();
    // The reasoning levels are the thread's effort; nothing else is.
    const role = id === "effort" ? "reasoning" : o.role === "reasoning" ? null : o.role;
    if (o.type === "boolean") return { type: "boolean", id, label: o.label.trim(), default: o.defaultOn, ...(role ? { role } : {}) };
    const choices = o.choices.map((c) => ({ id: c.id.trim(), label: c.label.trim() || c.id.trim() }));
    const def = o.choices.find((c) => c.key === o.defaultChoice)?.id.trim();
    return { type: "select", id, label: o.label.trim(), choices, ...(def ? { default: def } : {}), ...(role ? { role } : {}) };
  });
  const name = d.name.trim();
  return { id: d.slug, ...(name ? { name } : {}), ...(options.length ? { options } : {}) };
}

/** A new, empty option of the owner's own. */
export function customOption(): DraftOption {
  return { key: key(), id: "", label: "", type: "select", choices: [choice("", "")], defaultChoice: null, defaultOn: false, role: null };
}

export const newChoice = () => choice("", "");
