"use client";

import "@xterm/xterm/css/xterm.css";
import { SquareTerminal, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Machine, Terminal as TerminalRec } from "@/lib/agents-api";
import { Card } from "./nodes";
import { resolveColor } from "@roll/brand";
import { TerminalKeys, withCtrl } from "./terminal-keys";
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

/** A PTY on the machine as a canvas card (its Close is in the card's floating tools). */
export function TerminalNode({ terminal, machine }: { terminal: TerminalRec; machine: Machine | undefined }) {
  return (
    <Card width={620} label={`Terminal ${terminal.title}`}>
      <TerminalView terminal={terminal} machine={machine} className="h-[340px] rounded-b-[11px]" />
    </Card>
  );
}

/** Closes a terminal (hangs up its session): a tool in the terminal card's floating toolbar. */
export function CloseTerminal({ terminal, machine }: { terminal: TerminalRec; machine: Machine | undefined }) {
  const { client } = useAgents();
  return (
    <button
      type="button"
      onClick={(e) => {
        e.stopPropagation();
        if (machine) client.run(machine.id, { type: "terminal.close", terminal: terminal.id }).catch(() => {});
      }}
      aria-label="Close terminal"
      title="Close terminal"
      className="grid size-6 place-items-center rounded-md transition-colors duration-150 hover:bg-panel-3 hover:text-fg [&>svg]:size-3.5 [&>svg]:stroke-[1.75]"
    >
      <X />
    </button>
  );
}

/** The terminal's colours, from what its host paints (resolved, so oklch works), in xterm's terms. */
function themeOf(el: HTMLElement) {
  const s = getComputedStyle(el);
  const rgb = (css: string, fallback: string) => `rgb(${resolveColor(css.trim() || fallback).join(",")})`;
  const accent = resolveColor(s.getPropertyValue("--accent").trim() || "#4c8dff");
  return {
    background: rgb(s.backgroundColor, "#08090a"),
    foreground: rgb(s.getPropertyValue("--fg"), "#ededed"),
    cursor: `rgb(${accent.join(",")})`,
    selectionBackground: `rgba(${accent.join(",")},0.3)`,
  };
}

/**
 * A PTY on the machine: xterm.js fed by the stream's terminal deltas (and the scrollback when it
 * opens late), typing sent as `terminal.input`, the size as `terminal.resize`. `className` sizes
 * the screen (the canvas card's is fixed; a conversation's fills its panel).
 */
export function TerminalView({ terminal, machine, className = "", bare = false }: { terminal: TerminalRec; machine: Machine | undefined; className?: string; bare?: boolean }) {
  const { client, phone } = useAgents();
  const host = useRef<HTMLDivElement>(null);
  const [error, setError] = useState<string | null>(null);
  const mid = machine?.id;
  const exited = terminal.status === "exited";
  // A phone's key row writes through the same batching as typing; a held Ctrl applies to what's typed next.
  const send = useRef<(data: string) => void>(() => {});
  const [ctrl, setCtrl] = useState(false);
  const ctrlHeld = useRef(false);
  ctrlHeld.current = ctrl;

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
        theme: themeOf(el),
      });
      // Follow light/dark switches, like the canvas engine.
      const scheme = window.matchMedia("(prefers-color-scheme: light)");
      const onScheme = () => (term.options.theme = themeOf(el));
      scheme.addEventListener("change", onScheme);
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
      send.current = (d) => {
        pending += d;
        timer ??= setTimeout(flush, 12);
      };
      const onData = term.onData((d) => {
        const held = ctrlHeld.current;
        if (held) {
          ctrlHeld.current = false;
          setCtrl(false);
        }
        send.current(held ? withCtrl(d) : d);
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
        scheme.removeEventListener("change", onScheme);
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
      </div>
      )}
      <div ref={host} data-nodrag data-scroll className={`cursor-text overflow-hidden bg-bg px-2 py-1.5 ${className}`} />
      {phone && !exited && <TerminalKeys send={(d) => send.current(d)} ctrl={ctrl} setCtrl={setCtrl} />}
      {error && <p className="border-t border-line px-3 py-1.5 text-[12px] text-danger">{error}</p>}
    </>
  );
}
