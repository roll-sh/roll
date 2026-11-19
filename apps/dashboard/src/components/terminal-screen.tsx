import "@xterm/xterm/css/xterm.css";
import { resolveColor } from "@roll/brand";
import { useEffect, useRef } from "react";
import type { ScreenHandlers } from "@/lib/sse";

/** A shell session as a terminal drives it: its screen, its keys (base64), its size, its end. */
export interface ShellLink {
  follow: (opts: ScreenHandlers) => () => void;
  keys: (base64: string) => Promise<unknown>;
  resize: (size: { cols: number; rows: number }) => Promise<unknown>;
  close: () => Promise<unknown>;
}

const cssVar = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

/** The terminal's colours from what its host paints (resolved, so oklch works). */
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

/** Sessions whose terminal went, closed a moment later unless a terminal on them comes back (a
 * parent that re-renders a new tree remounts it: closing at once ended the shell it had just
 * opened). */
const closing = new Map<string, ReturnType<typeof setTimeout>>();
const CLOSE_AFTER_MS = 1500;

/** Text as the base64 of its UTF-8, as a shell's keys are sent. */
export function encodeKeys(text: string): string {
  let bin = "";
  for (const b of new TextEncoder().encode(text)) bin += String.fromCharCode(b);
  return btoa(bin);
}

/**
 * An open shell session as a terminal (roll apps' instances, previews' services): xterm.js, the
 * screen over SSE, keystrokes batched into POSTs in order, the size sent when the panel changes.
 * Unmounting closes the session a moment later (and lets what it held awake sleep again), unless a
 * terminal on it mounts again meanwhile. Keyed by `session`.
 */
export function TerminalScreen({ session, link, onEnded, className = "h-[440px] rounded-b-xl bg-panel px-2 py-2" }: { session: string; link: ShellLink; onEnded: () => void; className?: string }) {
  const host = useRef<HTMLDivElement>(null);
  const ended = useRef(onEnded);
  ended.current = onEnded;
  const linked = useRef(link);
  linked.current = link;

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const l = linked.current;
    const again = closing.get(session);
    if (again) {
      clearTimeout(again);
      closing.delete(session);
    }
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
        theme: themeOf(el),
      });
      const scheme = window.matchMedia("(prefers-color-scheme: light)");
      const onScheme = () => (term.options.theme = themeOf(el));
      scheme.addEventListener("change", onScheme);
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(el);
      term.focus();
      try {
        fit.fit();
      } catch {}

      const stop = l.follow({
        onChunk: (b) => term.write(b),
        onReset: () => term.reset(),
        onClosed: () => ended.current(),
      });

      // Keys: one POST per few milliseconds, in order.
      let pending = "";
      let sending: Promise<unknown> = Promise.resolve();
      let timer: ReturnType<typeof setTimeout> | null = null;
      const flush = () => {
        timer = null;
        const data = pending;
        pending = "";
        // A keystroke that didn't get there is said, not lost without a trace.
        if (data) sending = sending.then(() => l.keys(encodeKeys(data)).catch(() => term.write("\r\n\x1b[31m[a keystroke didn't reach the shell: type it again]\x1b[0m\r\n")));
      };
      const onData = term.onData((d) => {
        pending += d;
        timer ??= setTimeout(flush, 10);
      });

      let last = { cols: 0, rows: 0 };
      const resize = () => {
        try {
          fit.fit();
        } catch {}
        if (term.cols !== last.cols || term.rows !== last.rows) {
          last = { cols: term.cols, rows: term.rows };
          l.resize(last).catch(() => {});
        }
      };
      const ro = new ResizeObserver(resize);
      ro.observe(el);
      resize();

      cleanup = () => {
        stop();
        ro.disconnect();
        onData.dispose();
        scheme.removeEventListener("change", onScheme);
        if (timer) clearTimeout(timer);
        term.dispose();
      };
    })();
    return () => {
      disposed = true;
      cleanup();
      closing.set(
        session,
        setTimeout(() => {
          closing.delete(session);
          l.close().catch(() => {});
        }, CLOSE_AFTER_MS),
      );
    };
  }, [session]);

  return <div ref={host} className={className} />;
}
