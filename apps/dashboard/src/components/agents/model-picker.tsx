"use client";

import { ChevronDown } from "lucide-react";
import type { ReactNode } from "react";
import type { ModelInfo, ProviderInfo } from "@/lib/agents-api";
import { input } from "../ui";

/** The model a provider runs when none (or an unknown one) is picked: its default, else its first. */
export function resolveModel(info: ProviderInfo | undefined, model: string | null | undefined): ModelInfo | undefined {
  const models = info?.models ?? [];
  return models.find((m) => m.id === model) ?? models.find((m) => m.default) ?? models[0];
}

/**
 * Provider, model and effort for a thread, from the machine's installed providers. The owner keeps
 * the choice; picking a provider or a model resets what depends on it.
 */
export function ModelPicker({
  providers,
  provider,
  model,
  effort,
  onProvider,
  onModel,
  onEffort,
}: {
  providers: readonly ProviderInfo[];
  provider: string;
  model: string | null;
  effort: string | null;
  onProvider: (id: string) => void;
  onModel: (id: string) => void;
  onEffort: (id: string | null) => void;
}) {
  const info = providers.find((p) => p.id === provider);
  const models = info?.models ?? [];
  const modelInfo = resolveModel(info, model);
  const efforts = modelInfo?.efforts ?? [];
  return (
    <>
      <Picker label="Provider" value={provider} onChange={onProvider} options={providers.map((p) => ({ id: p.id, label: p.name, hint: p.auth === "missing" ? "log in first" : undefined }))} />
      {models.length > 0 && <Picker label="Model" value={modelInfo?.id ?? ""} onChange={onModel} options={models.map((m) => ({ id: m.id, label: m.name }))} />}
      {efforts.length > 0 && (
        <Picker label="Effort" value={effort ?? ""} onChange={(id) => onEffort(id || null)} options={[{ id: "", label: "default effort" }, ...efforts.map((x) => ({ id: x, label: x }))]} />
      )}
    </>
  );
}

/** A compact select that looks like a chip. */
export function Picker({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string;
  options: { id: string; label: string; hint?: string }[];
  onChange: (id: string) => void;
}): ReactNode {
  const current = options.find((o) => o.id === value);
  return (
    <label className="relative inline-flex items-center gap-0.5 rounded-md px-1.5 py-1 text-[11.5px] text-muted hover:bg-panel-2 hover:text-fg" title={current?.hint ?? label}>
      <span className="max-w-32 truncate">{current?.label ?? label}</span>
      <ChevronDown className="size-3 text-subtle" />
      <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)} className={`${input} absolute inset-0 cursor-pointer opacity-0`}>
        {options.map((o) => (
          <option key={o.id} value={o.id}>
            {o.label}
            {o.hint && o.id !== value ? ` (${o.hint})` : ""}
          </option>
        ))}
      </select>
    </label>
  );
}
