"use client";

import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp, RefreshCw, Skull } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { type BrowsedMessage, errorMessage, exchangeLabel, formatBytes, qapi } from "@/lib/queues-api";
import { useMarkState } from "../mark-state";
import { Badge, btn, CopyButton, Empty, ErrorNote, ICON, iconBtn, Pending, Segmented } from "./ui";

const PAGE = 25;

type View = "pretty" | "text" | "base64" | "hex";

interface Decoded {
  text: string | null;
  base64: string;
  bytes: Uint8Array;
  json: unknown;
  isJson: boolean;
}

function toBase64(bytes: Uint8Array) {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

export function decodePayload(m: BrowsedMessage): Decoded {
  let bytes: Uint8Array;
  let text: string | null;
  if (m.payload_encoding === "base64") {
    try {
      const bin = atob(m.payload);
      bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
    } catch {
      bytes = new Uint8Array();
    }
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch {
      text = null;
    }
  } else {
    text = m.payload;
    bytes = new TextEncoder().encode(m.payload);
  }
  let json: unknown = undefined;
  let isJson = false;
  if (text !== null) {
    const t = text.trim();
    if (t.startsWith("{") || t.startsWith("[") || String(m.properties.content_type ?? "").includes("json")) {
      try {
        json = JSON.parse(t);
        isJson = true;
      } catch {}
    }
  }
  return { text, base64: m.payload_encoding === "base64" ? m.payload : toBase64(bytes), bytes, json, isJson };
}

function hexDump(bytes: Uint8Array, limit = 4096) {
  const lines: string[] = [];
  const n = Math.min(bytes.length, limit);
  for (let i = 0; i < n; i += 16) {
    const row = bytes.subarray(i, Math.min(i + 16, n));
    const hex = Array.from(row, (b) => b.toString(16).padStart(2, "0")).join(" ");
    const ascii = Array.from(row, (b) => (b >= 32 && b < 127 ? String.fromCharCode(b) : ".")).join("");
    lines.push(`${i.toString(16).padStart(8, "0")}  ${hex.padEnd(47)}  ${ascii}`);
  }
  if (bytes.length > limit) lines.push(`… ${bytes.length - limit} more bytes`);
  return lines.join("\n");
}

function fmtValue(v: unknown) {
  if (typeof v === "string") return v;
  return JSON.stringify(v, null, 2);
}

/** Non-destructive queue browser: peeks at messages without consuming or requeueing them. */
export function MessageBrowser({ vhost, queue, total }: { vhost: string; queue: string; total: number }) {
  const { setState, play } = useMarkState();
  const [offset, setOffset] = useState(0);
  const [messages, setMessages] = useState<BrowsedMessage[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<number | null>(null);

  const load = useCallback(
    async (at: number) => {
      setLoading(true);
      setError(null);
      setState("thinking");
      try {
        const ms = await qapi.messages(vhost, queue, at, PAGE);
        setMessages(ms);
        setOffset(at);
        setOpen(ms.length === 1 ? ms[0].seq : null);
        setState("idle");
      } catch (err) {
        setError(errorMessage(err));
        play([["error", 1400]]);
      } finally {
        setLoading(false);
      }
    },
    [vhost, queue, setState, play],
  );

  useEffect(() => {
    load(0);
  }, [load]);

  const end = offset + (messages?.length ?? 0);

  return (
    <div>
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-2.5">
        <span className="font-mono text-[11px] text-subtle">
          {messages?.length ? `${offset + 1}–${end}` : "0"} of {total.toLocaleString("en-US")}
        </span>
        <span className="text-[11px] text-subtle">· peek only, nothing is consumed</span>
        <div className="ml-auto flex items-center gap-1">
          <button className={iconBtn} disabled={loading || offset === 0} onClick={() => load(Math.max(0, offset - PAGE))} aria-label="Previous page">
            <ChevronLeft {...ICON} />
          </button>
          <button className={iconBtn} disabled={loading || !messages || messages.length < PAGE} onClick={() => load(offset + PAGE)} aria-label="Next page">
            <ChevronRight {...ICON} />
          </button>
          <button className={btn} disabled={loading} onClick={() => load(offset)}>
            <RefreshCw {...ICON} className={`size-3.5 ${loading ? "animate-spin motion-reduce:animate-none" : ""}`} /> Refresh
          </button>
        </div>
      </div>
      {error && (
        <div className="px-5 pt-3">
          <ErrorNote error={error} onClose={() => setError(null)} />
        </div>
      )}
      {messages === null ? (
        <Empty>{loading ? <Pending>Peeking at messages…</Pending> : "—"}</Empty>
      ) : messages.length === 0 ? (
        <Empty>No messages{offset > 0 ? " at this offset" : " in this queue"}.</Empty>
      ) : (
        <ul className={loading ? "opacity-60 transition-opacity" : "transition-opacity"}>
          {messages.map((m) => (
            <MessageRow key={m.seq} m={m} open={open === m.seq} onToggle={() => setOpen((o) => (o === m.seq ? null : m.seq))} />
          ))}
        </ul>
      )}
    </div>
  );
}

function MessageRow({ m, open, onToggle }: { m: BrowsedMessage; open: boolean; onToggle: () => void }) {
  const death = m.x_death?.[0];
  return (
    <li className="border-b border-line last:border-0">
      <button onClick={onToggle} className={`flex w-full items-center gap-3 px-5 py-2.5 text-left transition ${open ? "bg-panel-2" : "hover:bg-panel-2/60"}`}>
        <span className="w-14 shrink-0 font-mono text-[11px] text-subtle tabular-nums">#{m.seq}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-[12px]">
          {m.routing_key || <span className="text-subtle">(empty key)</span>}
          <span className="text-subtle"> · {exchangeLabel(m.exchange)}</span>
        </span>
        {death && (
          <Badge tone="danger" title={`dead-lettered from ${death.queue} (${death.reason}) ×${death.count}`}>
            {death.reason}
          </Badge>
        )}
        {m.redelivered && <Badge>redelivered</Badge>}
        {m.delivery_count > 0 && <span className="hidden font-mono text-[11px] text-subtle sm:inline">×{m.delivery_count}</span>}
        <span className="w-16 shrink-0 text-right font-mono text-[11px] text-subtle">{formatBytes(m.payload_bytes)}</span>
        {open ? <ChevronUp {...ICON} className="size-3.5 text-subtle" /> : <ChevronDown {...ICON} className="size-3.5 text-subtle" />}
      </button>
      {open && <MessageDetail m={m} />}
    </li>
  );
}

function MessageDetail({ m }: { m: BrowsedMessage }) {
  const d = decodePayload(m);
  const views: View[] = d.isJson ? ["pretty", "text", "base64", "hex"] : d.text !== null ? ["text", "base64", "hex"] : ["hex", "base64"];
  const [view, setView] = useState<View>(views[0]);
  const shown =
    view === "pretty"
      ? JSON.stringify(d.json, null, 2)
      : view === "text"
        ? (d.text ?? "")
        : view === "base64"
          ? d.base64
          : hexDump(d.bytes);
  const props = Object.entries(m.properties ?? {}).filter(([, v]) => v !== null && v !== undefined);

  return (
    <div className="rise grid gap-4 border-t border-line bg-bg/40 px-5 py-4 lg:grid-cols-5">
      <div className="min-w-0 lg:col-span-3">
        <div className="mb-2 flex items-center gap-2">
          <Segmented value={view} options={views} onChange={setView} />
          {d.text === null && <span className="text-[11px] text-subtle">binary payload</span>}
          <span className="ml-auto">
            <CopyButton text={shown} label="Copy payload" />
          </span>
        </div>
        <pre className="max-h-80 overflow-auto rounded-lg border border-line bg-panel px-3 py-2 font-mono text-[12px] leading-relaxed break-words whitespace-pre-wrap">
          {shown || <span className="text-subtle">(empty body)</span>}
        </pre>
      </div>
      <div className="min-w-0 space-y-4 lg:col-span-2">
        <div>
          <p className="mb-1.5 text-[11px] text-subtle">Properties</p>
          <dl className="space-y-1">
              {props.map(([k, v]) => (
                <div key={k} className="grid grid-cols-[8.5rem_1fr] gap-2 font-mono text-[11px]">
                  <dt className="truncate text-subtle">{k}</dt>
                  <dd className="min-w-0 break-words whitespace-pre-wrap">{fmtValue(v)}</dd>
                </div>
              ))}
              <div className="grid grid-cols-[8.5rem_1fr] gap-2 font-mono text-[11px]">
                <dt className="text-subtle">delivery_count</dt>
                <dd>{m.delivery_count}</dd>
              </div>
              {m.received_ms !== undefined && (
                <div className="grid grid-cols-[8.5rem_1fr] gap-2 font-mono text-[11px]">
                  <dt className="text-subtle">received</dt>
                  <dd>{new Date(m.received_ms).toLocaleString()}</dd>
                </div>
              )}
            </dl>
        </div>
        {m.x_death && m.x_death.length > 0 && (
          <div>
            <p className="mb-1.5 flex items-center gap-1.5 text-[11px] text-subtle">
              <Skull {...ICON} className="size-3" /> x-death
            </p>
            <table className="w-full font-mono text-[11px]">
              <thead>
                <tr className="text-left text-subtle">
                  <th className="pb-1 font-normal">queue</th>
                  <th className="pb-1 font-normal">reason</th>
                  <th className="pb-1 font-normal">from</th>
                  <th className="pb-1 text-right font-normal">count</th>
                </tr>
              </thead>
              <tbody>
                {m.x_death.map((x, i) => (
                  <tr key={i} className="border-t border-line">
                    <td className="py-1 pr-2 break-all">{x.queue}</td>
                    <td className="py-1 pr-2 text-danger">{x.reason}</td>
                    <td className="py-1 pr-2 break-all text-muted">
                      {exchangeLabel(x.exchange)}
                      {x.routing_keys.length > 0 && <span className="text-subtle"> / {x.routing_keys.join(", ")}</span>}
                    </td>
                    <td className="py-1 text-right tabular-nums">{x.count}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
