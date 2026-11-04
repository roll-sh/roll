"use client";

import "@xterm/xterm/css/xterm.css";
import { SquareTerminal, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Machine, Terminal as TerminalRec } from "@/lib/agents-api";
import { Card } from "./nodes";
import { useAgents } from "./use-agents";

function decode(b64: string): Uint8Array {
  try {
    const bin = atob(b64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return new Uint8Array();
  }
}

function encode(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

const cssVar = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

/** A PTY on the machine as a canvas card. */
export function TerminalNode({ terminal, machine }: { terminal: TerminalRec; machine: Machine | undefined }) {
  return (
    <Card width={620} label={`Terminal ${terminal.title}`}>
      <TerminalView terminal={terminal} machine={machine} className="h-[340px]" />
    </Card>
  );
}

/**
 * A PTY on the machine: xterm.js fed by the stream's terminal deltas (and the scrollback when it
 * opens late), typing sent as `terminal.input`, the size as `terminal.resize`. `className` sizes
 * the screen (the canvas card's is fixed; a conversation's fills its panel).
 */
export function TerminalView({ terminal, machine, className = "", bare = false }: { terminal: TerminalRec; machine: Machine | undefined; className?: string; bare?: boolean }) {
  const { client } = useAgents();
  const host = useRef<HTMLDivElement>(null);
  const [error, setError] = useState<string | null>(null);
  const mid = machine?.id;
  const exited = terminal.status === "exited";

  useEffect(() => {
    const el = host.current;
    if (!el || !mid) return;
    let disposed = false;
    let cleanup = () => {};
    void (async () => {
      const [{ Terminal }, { FitAddon }] = await Promise.all([import("@xterm/xterm"), import("@xterm/addon-fit")]);
      if (disposed) return;
      const term = new Terminal({
        fontFamily: `${cssVar("--font-geist-mono") || "ui-monospace"}, ui-monospace, monospace`,
        fontSize: 12,
        lineHeight: 1.25,
        cursorBlink: false,
        scrollback: 5000,
        allowProposedApi: false,
        theme: {
          background: cssVar("--bg") || "#000",
          foreground: cssVar("--fg") || "#ededed",
          cursor: cssVar("--accent") || "#4c8dff",
          selectionBackground: "rgba(76,141,255,0.3)",
        },
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(el);
      try {
        fit.fit();
      } catch {}

      const sub = client.onTerminal(terminal.id, (b64) => term.write(decode(b64)));
      if (sub.backlog.length) for (const b of sub.backlog) term.write(decode(b));
      else
        client
          .run<{ type: "scrollback"; data: string }>(mid, { type: "terminal.scrollback", terminal: terminal.id }, "scrollback")
          .then((r) => r.data && term.write(decode(r.data)))
          .catch(() => {});

      // Typing: batch keystrokes into one command per few milliseconds.
      let pending = "";
      let timer: ReturnType<typeof setTimeout> | null = null;
      const flush = () => {
        timer = null;
        const data = pending;
        pending = "";
        if (data) client.run(mid, { type: "terminal.input", terminal: terminal.id, data: encode(data) }).catch((e: Error) => setError(e.message));
      };
      const onData = term.onData((d) => {
        pending += d;
        timer ??= setTimeout(flush, 12);
      });

      let last = { cols: term.cols, rows: term.rows };
      let resizeTimer: ReturnType<typeof setTimeout> | null = null;
      const ro = new ResizeObserver(() => {
        try {
          fit.fit();
        } catch {}
        if (term.cols === last.cols && term.rows === last.rows) return;
        last = { cols: term.cols, rows: term.rows };
        if (resizeTimer) clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
          client.run(mid, { type: "terminal.resize", terminal: terminal.id, cols: last.cols, rows: last.rows }).catch(() => {});
        }, 150);
      });
      ro.observe(el);

      cleanup = () => {
        ro.disconnect();
        onData.dispose();
        sub.off();
        if (timer) clearTimeout(timer);
        if (resizeTimer) clearTimeout(resizeTimer);
        term.dispose();
      };
    })();
    return () => {
      disposed = true;
      cleanup();
    };
  }, [client, mid, terminal.id]);

  return (
    <>
      {!bare && (
      <div className="flex items-center gap-2 border-b border-line px-3 py-2 text-[12px]" data-drag>
        <SquareTerminal className="size-3.5 text-subtle" />
        <span className="min-w-0 flex-1 truncate font-mono">{terminal.title}</span>
        <span className="truncate font-mono text-[11px] text-subtle">{terminal.cwd}</span>
        {exited && <span className="text-[11px] text-subtle">exited{terminal.exit_code !== null ? ` ${terminal.exit_code}` : ""}</span>}
        <button
          onClick={() => mid && client.run(mid, { type: "terminal.close", terminal: terminal.id }).catch((e: Error) => setError(e.message))}
          className="p-0.5 text-subtle hover:text-fg"
          aria-label="Close terminal"
          title="Close"
        >
          <X className="size-3.5" />
        </button>
      </div>
      )}
      <div ref={host} data-nodrag data-scroll className={`cursor-text overflow-hidden bg-bg px-2 py-1.5 ${className}`} />
      {error && <p className="border-t border-line px-3 py-1.5 text-[12px] text-danger">{error}</p>}
    </>
  );
}
