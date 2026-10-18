"use client";

import { useCallback, useState, type ReactNode } from "react";
import {
  EVICTION_POLICIES,
  formatDuration,
  formatMb,
  IDLE_OPTIONS,
  MEMORY_STEPS,
  RETENTION_OPTIONS,
  withCurrent,
  type RedisSettings,
  type RedisStatus,
} from "@/lib/redis-api";
import { useMarkState } from "../mark-state";
import { input } from "../ui";

export const POLL_MS = 2500;

/** Run a control-plane action with every mark reflecting it: deploying → success | error. */
export function useAction(refresh: () => Promise<unknown>) {
  const { play, setState } = useMarkState();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = useCallback(
    async <T,>(key: string, fn: () => Promise<T>): Promise<T | undefined> => {
      setBusy(key);
      setError(null);
      setState("deploying");
      try {
        const out = await fn();
        play([["success", 1400]]);
        await refresh();
        return out;
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        play([["error", 1600]]);
        return undefined;
      } finally {
        setBusy(null);
      }
    },
    [play, refresh, setState],
  );

  return { busy, error, setError, run };
}

export type Run = ReturnType<typeof useAction>["run"];

/** Active (in memory) or scaled to zero. */
export function RedisStateDot({ state }: { state: RedisStatus["state"] | undefined }) {
  return (
    <span
      className={`inline-block size-2 shrink-0 rounded-full ${
        state === "active" ? "bg-ok" : "border border-line-2 bg-transparent"
      }`}
    />
  );
}

export function stateLabel(state: RedisStatus["state"] | undefined) {
  return state === "active" ? "active" : "scaled to zero";
}

/** The editable settings shared by the create form and the settings tab. */
export function SettingsFields({
  value,
  onChange,
}: {
  value: RedisSettings;
  onChange: (v: RedisSettings) => void;
}) {
  const set = <K extends keyof RedisSettings>(k: K, v: RedisSettings[K]) => onChange({ ...value, [k]: v });
  const memory = MEMORY_STEPS.includes(value.max_memory_mb)
    ? MEMORY_STEPS
    : [...MEMORY_STEPS, value.max_memory_mb].sort((a, b) => a - b);
  return (
    <div className="grid gap-x-3 gap-y-4 sm:grid-cols-2 lg:grid-cols-3">
      <Field label="Memory limit" hint="maxmemory, applied live">
        <select
          value={value.max_memory_mb}
          onChange={(e) => set("max_memory_mb", Number(e.target.value))}
          className={`${input} w-full`}
        >
          {memory.map((m) => (
            <option key={m} value={m}>
              {formatMb(m)}
            </option>
          ))}
        </select>
      </Field>
      <Field label="Eviction policy" hint="maxmemory-policy">
        <select
          value={value.eviction_policy}
          onChange={(e) => set("eviction_policy", e.target.value)}
          className={`${input} w-full font-mono text-[12px]`}
        >
          {EVICTION_POLICIES.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
      </Field>
      <Field
        label="Durability"
        hint={value.durability === "sync" ? "replies wait for object storage" : "durable within ~1s, like everysec"}
      >
        <Segmented
          value={value.durability}
          options={[
            { label: "async", value: "async" },
            { label: "sync", value: "sync" },
          ]}
          onChange={(v) => set("durability", v)}
        />
      </Field>
      <Field label="Scale to zero after" hint={value.idle_timeout_s === 0 ? "never unloads" : "with no clients connected"}>
        <select
          value={value.idle_timeout_s}
          onChange={(e) => set("idle_timeout_s", Number(e.target.value))}
          className={`${input} w-full`}
        >
          {withCurrent(IDLE_OPTIONS, value.idle_timeout_s, formatDuration).map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      </Field>
      <Field label="Point-in-time restore" hint="restore and branch window">
        <select
          value={value.history_retention_s}
          onChange={(e) => set("history_retention_s", Number(e.target.value))}
          className={`${input} w-full`}
        >
          {withCurrent(RETENTION_OPTIONS, value.history_retention_s, formatDuration).map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      </Field>
    </div>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="block min-w-0 space-y-1">
      <span className="block text-[11px] text-subtle">{label}</span>
      {children}
      {hint && <span className="block truncate text-[11px] text-subtle">{hint}</span>}
    </label>
  );
}

export function Segmented<T extends string | number>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { label: string; value: T }[];
  onChange: (v: T) => void;
}) {
  return (
    <div className="flex rounded-lg border border-line bg-panel p-0.5" role="radiogroup">
      {options.map((o) => (
        <button
          key={String(o.value)}
          type="button"
          role="radio"
          aria-checked={o.value === value}
          onClick={() => onChange(o.value)}
          className={`flex-1 rounded-md px-2.5 py-1 font-mono text-[12px] transition ${
            o.value === value ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"
          }`}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** `<input type="datetime-local">` value for Unix seconds, in local time. */
export function toLocalInput(ts: number) {
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export function fromLocalInput(v: string) {
  const t = new Date(v).getTime();
  return Number.isFinite(t) ? Math.floor(t / 1000) : null;
}

export function formatTime(ts: number) {
  return new Date(ts * 1000).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

export function now() {
  return Math.floor(Date.now() / 1000);
}
