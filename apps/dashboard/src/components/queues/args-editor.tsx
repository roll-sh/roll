"use client";

import { Plus, X } from "lucide-react";
import type { QueueType } from "@/lib/queues-api";
import { Chips, ICON, iconBtn, input } from "./ui";

export type ArgType = "string" | "number" | "boolean" | "json";

export interface ArgRow {
  id: number;
  key: string;
  type: ArgType;
  value: string;
}

export interface ArgSuggestion {
  key: string;
  type: ArgType;
  value: string;
  title: string;
  /** Queue types the argument applies to; all when omitted. */
  for?: QueueType[];
}

export const QUEUE_ARGS: ArgSuggestion[] = [
  { key: "x-message-ttl", type: "number", value: "60000", title: "Per-message TTL in ms", for: ["classic", "quorum"] },
  { key: "x-dead-letter-exchange", type: "string", value: "dlx", title: "Exchange for rejected or expired messages", for: ["classic", "quorum"] },
  { key: "x-dead-letter-routing-key", type: "string", value: "", title: "Routing key used when dead-lettering", for: ["classic", "quorum"] },
  { key: "x-max-length", type: "number", value: "10000", title: "Maximum number of ready messages" },
  { key: "x-max-length-bytes", type: "number", value: "104857600", title: "Maximum total body size in bytes" },
  { key: "x-overflow", type: "string", value: "reject-publish", title: "drop-head | reject-publish | reject-publish-dlx", for: ["classic", "quorum"] },
  { key: "x-expires", type: "number", value: "1800000", title: "Delete the queue after it is unused for this many ms", for: ["classic", "quorum"] },
  { key: "x-max-priority", type: "number", value: "10", title: "Enable priorities up to this value", for: ["classic"] },
  { key: "x-single-active-consumer", type: "boolean", value: "true", title: "Deliver to one consumer at a time", for: ["classic", "quorum"] },
  { key: "x-delivery-limit", type: "number", value: "20", title: "Redeliveries before dead-lettering", for: ["quorum"] },
  { key: "x-dead-letter-strategy", type: "string", value: "at-least-once", title: "at-most-once | at-least-once", for: ["quorum"] },
  { key: "x-max-age", type: "string", value: "7D", title: "Stream retention by age (Y, M, D, h, m, s)", for: ["stream"] },
  { key: "x-stream-max-segment-size-bytes", type: "number", value: "500000000", title: "Stream segment size", for: ["stream"] },
  { key: "x-partitions", type: "number", value: "1", title: "roll: number of partitions (1 keeps exact ordering)" },
];

export const EXCHANGE_ARGS: ArgSuggestion[] = [
  { key: "alternate-exchange", type: "string", value: "unrouted", title: "Receives messages this exchange can't route" },
  { key: "x-delayed-type", type: "string", value: "direct", title: "Routing type of an x-delayed-message exchange" },
];

let nextId = 1;
export const argRow = (key = "", type: ArgType = "string", value = ""): ArgRow => ({ id: nextId++, key, type, value });

export function rowsFromArgs(args: Record<string, unknown> | null | undefined): ArgRow[] {
  return Object.entries(args ?? {}).map(([k, v]) =>
    typeof v === "number"
      ? argRow(k, "number", String(v))
      : typeof v === "boolean"
        ? argRow(k, "boolean", String(v))
        : typeof v === "string"
          ? argRow(k, "string", v)
          : argRow(k, "json", JSON.stringify(v)),
  );
}

/** Turn editor rows into an AMQP arguments table, or explain the first invalid row. */
export function argsFromRows(rows: ArgRow[]): { args: Record<string, unknown>; error: string | null } {
  const args: Record<string, unknown> = {};
  for (const r of rows) {
    const key = r.key.trim();
    if (!key) continue;
    if (key in args) return { args, error: `Duplicate argument ${key}` };
    if (r.type === "number") {
      const n = Number(r.value);
      if (r.value.trim() === "" || !Number.isFinite(n)) return { args, error: `${key} must be a number` };
      args[key] = n;
    } else if (r.type === "boolean") {
      args[key] = r.value === "true";
    } else if (r.type === "json") {
      try {
        args[key] = JSON.parse(r.value);
      } catch {
        return { args, error: `${key} isn't valid JSON` };
      }
    } else {
      args[key] = r.value;
    }
  }
  return { args, error: null };
}

export function ArgumentsEditor({
  rows,
  onChange,
  suggestions = [],
  label = "Arguments",
}: {
  rows: ArgRow[];
  onChange: (rows: ArgRow[]) => void;
  suggestions?: ArgSuggestion[];
  label?: string;
}) {
  const used = new Set(rows.map((r) => r.key.trim()));
  const set = (id: number, patch: Partial<ArgRow>) => onChange(rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));
  const free = suggestions.filter((s) => !used.has(s.key));

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <span className="text-[11px] text-subtle">{label}</span>
        <button type="button" onClick={() => onChange([...rows, argRow()])} className="inline-flex items-center gap-1 text-[11px] text-subtle hover:text-fg">
          <Plus {...ICON} className="size-3" /> Add
        </button>
      </div>
      {rows.length > 0 && (
        <div className="space-y-1.5">
          {rows.map((r) => (
            <div key={r.id} className="flex items-center gap-1.5">
              <input
                value={r.key}
                onChange={(e) => set(r.id, { key: e.target.value })}
                placeholder="x-argument"
                spellCheck={false}
                className={`${input} min-w-0 flex-[1.2] font-mono text-[12px]`}
              />
              <select
                value={r.type}
                onChange={(e) => {
                  const type = e.target.value as ArgType;
                  set(r.id, { type, value: type === "boolean" ? "true" : r.value });
                }}
                className={`${input} w-24 text-[12px]`}
                aria-label="Type"
              >
                <option value="string">string</option>
                <option value="number">number</option>
                <option value="boolean">boolean</option>
                <option value="json">json</option>
              </select>
              {r.type === "boolean" ? (
                <select value={r.value} onChange={(e) => set(r.id, { value: e.target.value })} className={`${input} min-w-0 flex-1 font-mono text-[12px]`}>
                  <option value="true">true</option>
                  <option value="false">false</option>
                </select>
              ) : (
                <input
                  value={r.value}
                  onChange={(e) => set(r.id, { value: e.target.value })}
                  placeholder="value"
                  inputMode={r.type === "number" ? "numeric" : undefined}
                  spellCheck={false}
                  className={`${input} min-w-0 flex-1 font-mono text-[12px]`}
                />
              )}
              <button type="button" onClick={() => onChange(rows.filter((x) => x.id !== r.id))} className={iconBtn} aria-label="Remove argument">
                <X {...ICON} />
              </button>
            </div>
          ))}
        </div>
      )}
      {free.length > 0 && (
        <Chips
          items={free.map((s) => ({ key: s.key, title: s.title }))}
          onPick={(k) => {
            const s = suggestions.find((x) => x.key === k)!;
            onChange([...rows.filter((r) => r.key.trim() || r.value), argRow(s.key, s.type, s.value)]);
          }}
        />
      )}
    </div>
  );
}

/** Parse a JSON object, or explain why it isn't one. */
export function parseObject(text: string): { value: Record<string, unknown> | null; error: string | null } {
  if (!text.trim()) return { value: {}, error: null };
  try {
    const v = JSON.parse(text);
    if (!v || typeof v !== "object" || Array.isArray(v)) return { value: null, error: "Must be a JSON object" };
    return { value: v as Record<string, unknown>, error: null };
  } catch (err) {
    return { value: null, error: err instanceof Error ? err.message : "Invalid JSON" };
  }
}

/** A JSON object editor with live validation and one-click suggested keys. */
export function JsonEditor({
  value,
  onChange,
  suggestions = [],
  rows = 6,
  label,
}: {
  value: string;
  onChange: (v: string) => void;
  suggestions?: { key: string; value: unknown; title?: string }[];
  rows?: number;
  label?: string;
}) {
  const { value: parsed, error } = parseObject(value);
  const free = parsed ? suggestions.filter((s) => !(s.key in parsed)) : suggestions;
  return (
    <div className="space-y-2">
      {label && (
        <div className="flex items-center justify-between">
          <span className="text-[11px] text-subtle">{label}</span>
          {error ? <span className="truncate font-mono text-[11px] text-danger">{error}</span> : <span className="font-mono text-[11px] text-subtle">valid JSON</span>}
        </div>
      )}
      <textarea
        value={value}
        onChange={(e) => onChange(e.target.value)}
        rows={rows}
        spellCheck={false}
        className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed ${error ? "border-danger/50 focus:border-danger/70" : ""}`}
      />
      {free.length > 0 && (
        <Chips
          items={free.map((s) => ({ key: s.key, title: s.title }))}
          onPick={(k) => {
            const s = suggestions.find((x) => x.key === k)!;
            const base = parsed ?? {};
            onChange(JSON.stringify({ ...base, [s.key]: s.value }, null, 2));
          }}
        />
      )}
    </div>
  );
}
