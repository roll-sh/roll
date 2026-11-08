"use client";

/**
 * The request log (docs/code/gateway.md, "Usage and cost"): a live tail of `gateway.log {after}`,
 * polled every 2 s while it's on screen and the page is in front, newest first, at most
 * `TAIL_CAP` kept; an errors filter (failed requests and failovers); "Older" pages back with
 * `before`. A failover shows as its chain of attempts; a request whose bodies were kept
 * (`log_bodies`) opens them (`gateway.log.body`).
 */

import { ArrowRight, ChevronRight, FileText, Shuffle } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import type { LogBody, LogEntry } from "@/lib/agents-api";
import { compact, isFailover, mergeLog, usd, type GatewayCommands } from "@/lib/agents-gateway";
import { cn, Switch } from "../ui";
import { message, Problem, textBtn, usePoll, useVisible } from "./shared";

/** How often the tail asks for what's new. */
export const TAIL_MS = 2000;

const FORMAT: Record<LogEntry["format"], string> = { messages: "Messages", chat: "Chat", responses: "Responses" };

export function RequestLog({
  gw,
  logBodies,
  upstreamName,
  machineName,
}: {
  gw: GatewayCommands;
  logBodies: boolean;
  upstreamName: (id: string) => string;
  machineName: (id: string | undefined) => string;
}) {
  const box = useRef<HTMLDivElement>(null);
  const visible = useVisible(box);
  const [entries, setEntries] = useState<LogEntry[] | null>(null);
  const [errors, setErrors] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [open, setOpen] = useState<number | null>(null);
  const [older, setOlder] = useState<"idle" | "busy" | "end">("idle");
  // What the tail has: a ref, so a slow answer for the old filter can't land in the new one.
  const shown = useRef<{ errors: boolean; entries: LogEntry[] }>({ errors: false, entries: [] });

  const tail = useCallback(async () => {
    const want = errors;
    const newest = shown.current.entries[0]?.seq;
    try {
      const r = await gw.log({ ...(newest !== undefined ? { after: newest } : {}), errors: want, limit: 100 });
      // The filter changed while this was on its way.
      if (shown.current.errors !== want) return;
      const next = mergeLog(shown.current.entries, r.entries);
      shown.current = { errors: want, entries: next };
      setEntries(next);
      setProblem(null);
    } catch (e) {
      setProblem(message(e));
    }
  }, [gw, errors]);
  usePoll(visible, TAIL_MS, tail);

  const filter = (on: boolean) => {
    setErrors(on);
    setEntries(null);
    setOlder("idle");
    shown.current = { errors: on, entries: [] };
  };
  const loadOlder = async () => {
    const oldest = shown.current.entries[shown.current.entries.length - 1]?.seq;
    if (oldest === undefined) return;
    setOlder("busy");
    try {
      const r = await gw.log({ before: oldest, errors, limit: 100 });
      const next = mergeLog(shown.current.entries, r.entries, Number.MAX_SAFE_INTEGER);
      shown.current = { errors, entries: next };
      setEntries(next);
      setOlder(r.entries.length ? "idle" : "end");
    } catch (e) {
      setProblem(message(e));
      setOlder("idle");
    }
  };

  return (
    <div ref={box} className="space-y-2">
      <div className="flex flex-wrap items-center gap-3 text-[12px]">
        <span className="inline-flex items-center gap-1.5 text-muted" role="status">
          <span className={cn("size-1.5 rounded-full", visible ? "bg-accent motion-safe:animate-pulse" : "bg-faint")} aria-hidden />
          {visible ? "Live" : "Paused while hidden"}
        </span>
        <label className="ml-auto flex items-center gap-2 text-muted">
          <Switch size="sm" checked={errors} onCheckedChange={filter} aria-label="Only errors and failovers" />
          Only errors and failovers
        </label>
      </div>
      {problem && <Problem>{problem}</Problem>}
      <div className="overflow-hidden rounded-lg border border-line-2 bg-panel-2/40">
        {entries === null ? (
          <p className="px-3 py-3 text-[12.5px] text-subtle">
            <span className="shimmer">Loading requests…</span>
          </p>
        ) : entries.length === 0 ? (
          <p className="px-3 py-3 text-[12.5px] text-subtle">{errors ? "No errors or failovers in the last 7 days." : "No requests yet. They show here as they happen."}</p>
        ) : (
          <ul className="max-h-[420px] divide-y divide-line overflow-y-auto" aria-label="Requests">
            {entries.map((e) => (
              <LogRow key={e.seq} e={e} open={open === e.seq} onToggle={() => setOpen((o) => (o === e.seq ? null : e.seq))} gw={gw} upstreamName={upstreamName} machineName={machineName} />
            ))}
          </ul>
        )}
      </div>
      <div className="flex items-center justify-between text-[11.5px] text-subtle">
        <span>{logBodies ? "Bodies are kept a day (64 KiB each)." : "Bodies aren't kept (Settings below)."} The log keeps 7 days.</span>
        {entries && entries.length > 0 && older !== "end" && (
          <button type="button" disabled={older === "busy"} onClick={() => void loadOlder()} className={textBtn}>
            {older === "busy" ? <span className="shimmer">Loading…</span> : "Older"}
          </button>
        )}
      </div>
    </div>
  );
}

function LogRow({
  e,
  open,
  onToggle,
  gw,
  upstreamName,
  machineName,
}: {
  e: LogEntry;
  open: boolean;
  onToggle: () => void;
  gw: GatewayCommands;
  upstreamName: (id: string) => string;
  machineName: (id: string | undefined) => string;
}) {
  const failed = e.status >= 400 || !!e.error;
  const time = new Date(e.at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  const served = e.attempts[e.attempts.length - 1];
  return (
    <li className="text-[12px]" aria-label={`Request ${e.seq}`}>
      <button type="button" onClick={onToggle} aria-expanded={open} className="flex w-full min-w-0 flex-wrap items-center gap-x-2.5 gap-y-0.5 px-3 py-1.5 text-left transition-colors hover:bg-fg/[0.04] sm:flex-nowrap">
        <ChevronRight className={cn("size-3 shrink-0 text-subtle transition-transform duration-150 motion-reduce:transition-none", open && "rotate-90")} aria-hidden />
        <span className="shrink-0 font-mono text-[11px] tabular-nums text-subtle">{time}</span>
        <span className={cn("w-8 shrink-0 font-mono text-[11px] tabular-nums", failed ? "text-danger" : "text-muted")}>{e.status}</span>
        {/* On a phone the row wraps rather than cutting the model, the machine and the upstream. */}
        <span className="min-w-0 font-mono text-[11.5px] break-all text-fg sm:truncate sm:break-normal">{e.model}</span>
        {isFailover(e) && (
          <span className="inline-flex shrink-0 items-center gap-1 rounded border border-line-2 px-1 text-[10.5px] text-muted" title="Failover: another upstream took over">
            <Shuffle className="size-2.5" aria-hidden /> failover
          </span>
        )}
        <span className="min-w-0 truncate text-subtle">{machineName(e.machine)}</span>
        <span className="ml-auto shrink-0 font-mono text-[11px] tabular-nums text-subtle">
          {served ? upstreamName(served.upstream) : "—"} · {e.ms >= 1000 ? `${(e.ms / 1000).toFixed(1)} s` : `${e.ms} ms`}
        </span>
      </button>
      {open && <LogDetail e={e} gw={gw} upstreamName={upstreamName} machineName={machineName} />}
    </li>
  );
}

function LogDetail({ e, gw, upstreamName, machineName }: { e: LogEntry; gw: GatewayCommands; upstreamName: (id: string) => string; machineName: (id: string | undefined) => string }) {
  const [body, setBody] = useState<LogBody | "busy" | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const loadBody = async () => {
    setBody("busy");
    try {
      setBody(await gw.body(e.seq));
    } catch (err) {
      setBody(null);
      setProblem(message(err));
    }
  };
  return (
    <div className="space-y-2 bg-panel-2/60 px-3 pt-1 pb-3 pl-8">
      <ol className="flex flex-wrap items-center gap-1.5" aria-label="Attempts">
        {e.attempts.length === 0 && <li className="text-subtle">No upstream was tried.</li>}
        {e.attempts.map((a, i) => (
          <li key={i} className="flex items-center gap-1.5">
            {i > 0 && <ArrowRight className="size-3 text-faint" aria-label="then" />}
            <span className={cn("rounded-md border px-1.5 py-0.5", a.status >= 200 && a.status < 300 ? "border-line-2 text-fg" : "border-danger/30 text-danger")} title={a.error}>
              {upstreamName(a.upstream)} <span className="font-mono text-[11px]">{a.model}</span> · <span className="font-mono tabular-nums">{a.status || "no answer"}</span> · {a.ms} ms
              {a.error ? ` · ${a.error}` : ""}
            </span>
          </li>
        ))}
      </ol>
      {e.error && <p className="text-danger">{e.error}</p>}
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-[11.5px]">
        <dt className="text-subtle">From</dt>
        <dd className="min-w-0 truncate text-muted">
          {machineName(e.machine)} · {FORMAT[e.format]}
          {e.stream ? ", streamed" : ""}
        </dd>
        {(e.project || e.thread) && (
          <>
            <dt className="text-subtle">For</dt>
            <dd className="min-w-0 truncate font-mono text-muted">{[e.project, e.thread].filter(Boolean).join(" · ")}</dd>
          </>
        )}
        <dt className="text-subtle">Tokens</dt>
        <dd className="font-mono tabular-nums text-muted">
          {compact(e.input_tokens)} in · {compact(e.output_tokens)} out · {compact(e.cache_read_tokens)} cached · {usd(e.cost_micros)}
        </dd>
        {e.first_byte_ms !== undefined && (
          <>
            <dt className="text-subtle">First byte</dt>
            <dd className="font-mono tabular-nums text-muted">{e.first_byte_ms} ms</dd>
          </>
        )}
      </dl>
      {e.has_body && body === null && (
        <button type="button" onClick={() => void loadBody()} className="flex h-6 items-center gap-1.5 rounded-md px-1.5 text-[11.5px] text-muted hover:bg-fg/[0.06] hover:text-fg">
          <FileText className="size-3" /> Show the bodies
        </button>
      )}
      {body === "busy" && <p className="shimmer text-[11.5px] text-subtle">Loading the bodies…</p>}
      {body && body !== "busy" && (
        <div className="grid gap-2 lg:grid-cols-2">
          {(["request", "response"] as const).map((k) => (
            <div key={k} className="min-w-0">
              <p className="mb-1 text-[11px] font-[510] text-muted">{k === "request" ? "Request" : "Response"}</p>
              <pre className="max-h-56 overflow-auto rounded-md border border-line-2 bg-raised p-2 font-mono text-[11px] leading-relaxed whitespace-pre-wrap break-all text-soft">{body[k]}</pre>
            </div>
          ))}
          {body.truncated && <p className="text-[11px] text-subtle">Cut at 64 KiB.</p>}
        </div>
      )}
      {problem && <Problem>{problem}</Problem>}
    </div>
  );
}
