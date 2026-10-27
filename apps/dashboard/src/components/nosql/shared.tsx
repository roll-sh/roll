"use client";

import { Clock, Lock } from "lucide-react";
import { useEffect, useState } from "react";
import {
  DIALECT_LABEL,
  formatCountdown,
  formatSpan,
  IDLE_OPTIONS,
  RETENTION_OPTIONS,
  type Dialect,
  type NosqlDb,
} from "@/lib/nosql-api";
import { withCurrent } from "@/lib/redis-api";
import { input } from "../ui";
// Generic form pieces and the action runner are shared with the Redis pages.
export { Field, formatTime, fromLocalInput, now, Segmented, toLocalInput, useAction, type Run } from "../redis/shared";
import { Field, Segmented } from "../redis/shared";

export const POLL_MS = 2500;

/** Loaded (accent: it is serving) or scaled to zero (hollow). */
export function NosqlStateDot({ state }: { state: NosqlDb["state"] | undefined }) {
  return (
    <span
      title={state === "running" ? "loaded" : "scaled to zero"}
      className={`inline-block size-2 shrink-0 rounded-full ${
        state === "running" ? "bg-accent" : "border border-line-2 bg-transparent"
      }`}
    />
  );
}

export function stateLabel(state: NosqlDb["state"] | undefined) {
  return state === "running" ? "loaded" : "scaled to zero";
}

export function DialectBadge({ dialect }: { dialect: Dialect }) {
  return (
    <span className="inline-flex shrink-0 items-center rounded-md border border-line px-1.5 py-px font-mono text-[10px] text-muted">
      {DIALECT_LABEL[dialect] ?? dialect}
    </span>
  );
}

export function ReadOnlyBadge() {
  return (
    <span className="inline-flex shrink-0 items-center gap-1 text-[11px] text-subtle" title="Writes are refused">
      <Lock className="size-3" /> read-only
    </span>
  );
}

/** The current Unix second, ticking (only mount it where a countdown is shown). */
export function useNow(every = 1000) {
  const [t, setT] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    const id = setInterval(() => setT(Math.floor(Date.now() / 1000)), every);
    return () => clearInterval(id);
  }, [every]);
  return t;
}

export function Expiry({ at, compact = false }: { at: number; compact?: boolean }) {
  const t = useNow();
  const left = formatCountdown(at, t);
  return (
    <span
      className="inline-flex shrink-0 items-center gap-1 font-mono text-[11px] text-subtle tabular-nums"
      title={`Deleted at ${new Date(at * 1000).toLocaleString()}`}
    >
      <Clock className="size-3" />
      {left ? (compact ? left : `expires in ${left}`) : "expiring…"}
    </span>
  );
}

export interface NosqlSettings {
  durability: "async" | "sync";
  idle_timeout: number;
  history_retention: number;
}

export function settingsOf(d: NosqlDb): NosqlSettings {
  return { durability: d.durability, idle_timeout: d.idle_timeout, history_retention: d.history_retention };
}

/** Durability, scale-to-zero and the restore window (create form and settings). */
export function SettingsFields({ value, onChange }: { value: NosqlSettings; onChange: (v: NosqlSettings) => void }) {
  const set = <K extends keyof NosqlSettings>(k: K, v: NosqlSettings[K]) => onChange({ ...value, [k]: v });
  return (
    <div className="grid gap-x-3 gap-y-4 sm:grid-cols-3">
      <Field
        label="Durability"
        hint={value.durability === "sync" ? "writes wait for object storage" : "commit log shipped within ~1s"}
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
      <Field label="Scale to zero after" hint={value.idle_timeout === 0 ? "never unloads" : "with no clients connected"}>
        <select
          value={value.idle_timeout}
          onChange={(e) => set("idle_timeout", Number(e.target.value))}
          className={`${input} w-full`}
        >
          {withCurrent(IDLE_OPTIONS, value.idle_timeout, formatSpan).map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      </Field>
      <Field label="Point-in-time history" hint="branch and restore window, up to 35 days">
        <select
          value={value.history_retention}
          onChange={(e) => set("history_retention", Number(e.target.value))}
          className={`${input} w-full`}
        >
          {withCurrent(RETENTION_OPTIONS, value.history_retention, formatSpan).map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      </Field>
    </div>
  );
}

/** A small stand-in for a value that isn't there. */
export function Dash() {
  return <span className="text-subtle">—</span>;
}

/** Earliest restorable second: the history window, never before the database existed. */
export function earliest(d: Pick<NosqlDb, "created_at" | "history_retention">, nowS: number) {
  return Math.max(d.created_at, nowS - d.history_retention);
}
