"use client";

/**
 * A custom model's name and options (T3 Code's `CustomModelEditor.tsx`, MIT), opened from its chip
 * in machine settings ("Edit name and options"): the display name, then the options the composer
 * shows for it: copied from one of the provider's models, added from the provider's presets, or
 * the owner's own (an id the provider takes, a label, choices or a switch, a default). None: the
 * composer uses the provider's own options for a slug like it. The logic is `lib/custom-models.ts`.
 */

import { Plus, X } from "lucide-react";
import { useState } from "react";
import type { CustomModel, ProviderInfo } from "@/lib/agents-api";
import { copyFrom, customOption, draftOf, modelFromDraft, newChoice, presetsFor, validateDraft, type CustomModelDraft, type DraftOption } from "@/lib/custom-models";
import { field } from "./settings-ui";
import { cn, Select, Switch } from "./ui";

const small = "inline-flex h-7 items-center gap-1 rounded-md border border-line-2 bg-raised px-2 text-[12px] transition-colors hover:bg-fg/[0.06] disabled:opacity-50";
const remove = "grid size-6 shrink-0 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-danger";

export function CustomModelEditor({
  provider,
  info,
  model,
  onSave,
  onCancel,
}: {
  /** The provider's id (its presets). */
  provider: string;
  info: ProviderInfo | undefined;
  model: CustomModel;
  /** Saves the machine's settings with it; false when that failed (the panel says why). */
  onSave: (model: CustomModel) => Promise<boolean>;
  onCancel: () => void;
}) {
  const [draft, setDraft] = useState<CustomModelDraft>(() => draftOf(model));
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const set = (patch: Partial<CustomModelDraft>) => {
    setDraft((d) => ({ ...d, ...patch }));
    setProblem(null);
  };
  const setOption = (key: string, patch: Partial<DraftOption>) => set({ options: draft.options.map((o) => (o.key === key ? { ...o, ...patch } : o)) });
  const presets = presetsFor(provider).filter((p) => !draft.options.some((o) => o.id.trim() === p.id));
  const sources = (info?.models ?? []).filter((m) => !m.custom && m.options.some((o) => o.role !== "context_window"));
  const save = async () => {
    const e = validateDraft(draft);
    if (e) return setProblem(e);
    setBusy(true);
    try {
      if (await onSave(modelFromDraft(draft))) onCancel();
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      aria-label={`Edit ${model.id}`}
      className="rise space-y-3 rounded-lg border border-line-2 bg-raised p-3"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          onCancel();
        }
      }}
    >
      <label className="block">
        <span className="mb-1 block text-[12px] text-muted">Display name</span>
        <input value={draft.name} onChange={(e) => set({ name: e.target.value })} placeholder={model.id} aria-label="Display name" className={cn(field, "w-full")} />
      </label>
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <span className="text-[12px] font-medium text-fg">Options shown in the composer</span>
          {sources.length > 0 && (
            <Select
              label="Copy from…"
              variant="ghost"
              className="ms-auto"
              placeholder="Copy from…"
              value={null}
              onChange={(id) => {
                const m = sources.find((x) => x.id === id);
                if (m) set({ options: copyFrom(m) });
              }}
              options={sources.map((m) => ({ value: m.id, label: m.name }))}
            />
          )}
        </div>
        {draft.options.length === 0 && <p className="text-[12px] text-subtle">No custom options. The composer uses the provider&apos;s default options.</p>}
        {draft.options.map((o, i) => (
          <fieldset key={o.key} aria-label={`Option ${i + 1}`} className="space-y-2 rounded-md border border-line bg-bg/40 p-2.5">
            <div className="flex flex-wrap items-center gap-1.5">
              <input value={o.id} onChange={(e) => setOption(o.key, { id: e.target.value })} placeholder="id" aria-label={`Option ${i + 1} id`} spellCheck={false} className={cn(field, "w-32 font-mono text-[12px]")} />
              <input value={o.label} onChange={(e) => setOption(o.key, { label: e.target.value })} placeholder="Label" aria-label={`Option ${i + 1} label`} className={cn(field, "min-w-24 flex-1")} />
              <Select
                label={`Option ${i + 1} type`}
                className="w-28"
                value={o.type}
                onChange={(t) => setOption(o.key, { type: t, ...(t === "select" && o.choices.length === 0 ? { choices: [newChoice()] } : {}) })}
                options={[
                  { value: "select", label: "Choices" },
                  { value: "boolean", label: "Toggle" },
                ]}
              />
              <button type="button" aria-label={`Remove option ${i + 1}`} title="Remove" className={remove} onClick={() => set({ options: draft.options.filter((x) => x.key !== o.key) })}>
                <X className="size-3.5" />
              </button>
            </div>
            {o.type === "boolean" ? (
              <label className="flex items-center gap-2 text-[12px] text-muted">
                <Switch size="sm" checked={o.defaultOn} onCheckedChange={(on) => setOption(o.key, { defaultOn: on })} aria-label={`Option ${i + 1} on by default`} />
                On by default
              </label>
            ) : (
              <div role="radiogroup" aria-label={`Option ${i + 1} choices`} className="space-y-1">
                {o.choices.map((c, j) => (
                  <div key={c.key} className="flex items-center gap-1.5">
                    <input
                      value={c.id}
                      onChange={(e) => setOption(o.key, { choices: o.choices.map((x) => (x.key === c.key ? { ...x, id: e.target.value } : x)) })}
                      placeholder="value"
                      aria-label={`Option ${i + 1} choice ${j + 1} value`}
                      spellCheck={false}
                      className={cn(field, "w-32 font-mono text-[12px]")}
                    />
                    <input
                      value={c.label}
                      onChange={(e) => setOption(o.key, { choices: o.choices.map((x) => (x.key === c.key ? { ...x, label: e.target.value } : x)) })}
                      placeholder="Label"
                      aria-label={`Option ${i + 1} choice ${j + 1} label`}
                      className={cn(field, "min-w-20 flex-1")}
                    />
                    <label className="flex shrink-0 items-center gap-1 text-[11.5px] text-muted">
                      <input type="radio" name={`default-${o.key}`} checked={o.defaultChoice === c.key} onChange={() => setOption(o.key, { defaultChoice: c.key })} className="accent-accent" />
                      Default
                    </label>
                    <button
                      type="button"
                      aria-label={`Remove choice ${j + 1} of option ${i + 1}`}
                      title="Remove"
                      className={remove}
                      onClick={() => setOption(o.key, { choices: o.choices.filter((x) => x.key !== c.key), ...(o.defaultChoice === c.key ? { defaultChoice: null } : {}) })}
                    >
                      <X className="size-3" />
                    </button>
                  </div>
                ))}
                <button type="button" className={small} onClick={() => setOption(o.key, { choices: [...o.choices, newChoice()] })}>
                  <Plus className="size-3" /> Add choice
                </button>
              </div>
            )}
          </fieldset>
        ))}
        <div className="flex flex-wrap gap-1.5">
          {presets.map((p) => (
            <button key={p.id} type="button" className={small} onClick={() => set({ options: [...draft.options, p.make()] })}>
              <Plus className="size-3" /> {p.label}
            </button>
          ))}
          <button type="button" className={small} onClick={() => set({ options: [...draft.options, customOption()] })}>
            <Plus className="size-3" /> Custom option
          </button>
        </div>
      </div>
      {problem && (
        <p role="alert" className="text-[12px] text-danger">
          {problem}
        </p>
      )}
      <div className="flex justify-end gap-1.5">
        <button type="button" onClick={onCancel} className="h-8 rounded-md px-3 text-[12.5px] text-muted hover:bg-fg/[0.06] hover:text-fg">
          Cancel
        </button>
        <button type="submit" disabled={busy} className="h-8 rounded-md bg-fg px-3 text-[12.5px] font-medium text-bg hover:opacity-90 disabled:opacity-60">
          {busy ? "Saving…" : "Save"}
        </button>
      </div>
    </form>
  );
}
