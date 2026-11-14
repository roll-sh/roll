import "@xterm/xterm/css/xterm.css";
import { resolveColor } from "@roll/brand";
import { SquareTerminal, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { appsApi, followShell, type Instance } from "@/lib/apps-api";
import { btn, EmptyPanel, input, Panel, primary } from "../ui";
import { message } from "./shared";

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

function encode(text: string): string {
  let bin = "";
  for (const b of new TextEncoder().encode(text)) bin += String.fromCharCode(b);
  return btoa(bin);
}

/**
 * A shell in a running instance (as `roll apps ssh`): xterm.js on an HTTP session, the screen over
 * SSE, keystrokes batched into POSTs, the size sent when the panel changes. A web service asleep is
 * woken and kept up while the shell is open.
 */
export function ShellTab({ service, env, instances = [] }: { service: string; env: string; instances?: Instance[] }) {
  const [session, setSession] = useState<string | null>(null);
  // An instance of its own (empty: one that's running, or a woken one).
  const [instance, setInstance] = useState("");
  const running = instances.filter((i) => i.state === "running");
  const [state, setState] = useState<"idle" | "opening" | "open" | "ended">("idle");
  const [error, setError] = useState<string | null>(null);

  const open = async () => {
    setError(null);
    setState("opening");
    try {
      const { id } = await appsApi.openShell(service, env, { cols: 100, rows: 30, ...(instance && running.some((i) => i.id === instance) ? { instance } : {}) });
      setSession(id);
      setState("open");
    } catch (e) {
      setError(message(e));
      setState("idle");
    }
  };

  const close = () => {
    if (session) appsApi.closeShell(service, env, session).catch(() => {});
    setSession(null);
    setState("idle");
  };

  return (
    <div className="space-y-4">
      <Panel
        title="Shell"
        aside={
          session ? (
            <button type="button" className={btn} onClick={close}>
              <X className="size-3.5" /> Close
            </button>
          ) : (
            <span className="flex items-center gap-2">
              {running.length > 1 && (
                <select value={instance} onChange={(e) => setInstance(e.target.value)} aria-label="Instance" className={`${input} !w-auto font-mono text-[12px]`}>
                  <option value="">Any instance</option>
                  {running.map((i) => (
                    <option key={i.id} value={i.id}>
                      {i.id}
                    </option>
                  ))}
                </select>
              )}
            <button type="button" className={primary} disabled={state === "opening"} onClick={() => void open()}>
              <SquareTerminal className="size-3.5" /> {state === "opening" ? "Opening…" : state === "ended" ? "Open another" : "Open a shell"}
            </button>
            </span>
          )
        }
      >
        {session ? (
          <Screen key={session} service={service} env={env} session={session} onEnded={() => {
            setSession(null);
            setState("ended");
          }} />
        ) : (
          <div className="px-5 py-4 text-[13px] text-muted">
            {state === "ended" ? "The shell ended." : "bash (or sh) in a running instance, with the environment's variables. A service that's asleep is woken while it's open."}{" "}
            From a terminal: <code className="font-mono text-fg">roll apps ssh -s {service} -e {env}</code>.
          </div>
        )}
      </Panel>
      {error && <EmptyPanel>{error}</EmptyPanel>}
    </div>
  );
}

function Screen({ service, env, session, onEnded }: { service: string; env: string; session: string; onEnded: () => void }) {
  const host = useRef<HTMLDivElement>(null);
  const ended = useRef(onEnded);
  ended.current = onEnded;

  useEffect(() => {
    const el = host.current;
    if (!el) return;
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

      const stop = followShell(service, env, session, {
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
        if (data) sending = sending.then(() => appsApi.shellKeys(service, env, session, encode(data)).catch(() => term.write("\r\n\x1b[31m[a keystroke didn't reach the shell: type it again]\x1b[0m\r\n")));
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
          appsApi.resizeShell(service, env, session, last).catch(() => {});
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
      // Leaving the page closes the shell (and lets the instance sleep again).
      appsApi.closeShell(service, env, session).catch(() => {});
    };
  }, [service, env, session]);

  return <div ref={host} className="h-[440px] rounded-b-xl bg-panel px-2 py-2" />;
}
