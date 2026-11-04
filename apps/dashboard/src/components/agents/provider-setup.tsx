/**
 * What to do when an agent can't run on a machine yet (`lib/agents-setup.ts`): no provider
 * installed, or none logged in. The machine card, the launcher and a project's actions show it
 * instead of a launch that would fail: log in (a terminal on the machine with the provider's
 * login), install one from the ACP registry or use an endpoint (machine settings, opened at that
 * section), or check again after doing it elsewhere.
 */

import { Cable, LogIn, Plus } from "lucide-react";
import { useState } from "react";
import type { Machine, ProviderInfo } from "@/lib/agents-api";
import { readiness } from "@/lib/agents-setup";
import { setAgentsView } from "@/lib/agents-surface";
import { primary } from "../ui";
import { loginCommand } from "./shared";
import { useAgents } from "./use-agents";

/**
 * Machine settings beside the canvas, scrolled to one of its sections (`Providers`, `Endpoints`);
 * `then` runs on the section once it's there (opening "Add agent").
 */
export function openMachineSettings(machine: string, section?: string, then?: (el: HTMLElement) => void) {
  openSettings(`m:${machine}`, section, then);
}

/** Settings beside the canvas (`owner`, `m:…`, `p:…`), scrolled to one of its sections. */
export function openSettings(target: string, section?: string, then?: (el: HTMLElement) => void) {
  setAgentsView({ kind: "settings", target });
  if (!section || typeof document === "undefined") return;
  let tries = 0;
  const find = () => {
    // The page went away while it looked (a test's document torn down).
    if (typeof document === "undefined") return;
    const el = document.querySelector<HTMLElement>(`[data-panel] section[aria-label="${CSS.escape(section)}"], section[aria-label="${CSS.escape(section)}"]`);
    if (el) {
      const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
      el.scrollIntoView?.({ block: "start", behavior: reduce ? "auto" : "smooth" });
      then?.(el);
    } else if (++tries < 20) setTimeout(find, 50);
  };
  setTimeout(find, 0);
}

/** "Add agent" (the ACP registry) in the machine's Providers section. */
export const openRegistry = (machine: string) =>
  openMachineSettings(machine, "Providers", (el) => [...el.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent?.trim() === "Add agent")?.click());

/** The panel, or nothing when an agent can run there (or it's offline: the card says so). */
export function ProviderSetup({ machine, className = "" }: { machine: Machine; className?: string }) {
  const { client, focus, readOnly } = useAgents();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const r = readiness(machine);
  if (readOnly || (r.state !== "none" && r.state !== "login")) return null;
  const run = async (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  const login = (p: ProviderInfo) =>
    run(`login:${p.id}`, async () => {
      const t = await client.run<{ type: "created"; id: string }>(machine.id, { type: "terminal.open", command: loginCommand(p, !!machine.sandbox), cols: 100, rows: 26 }, "created");
      focus(`x:${t.id}`);
    });
  const refresh = () => run("refresh", () => client.run(machine.id, { type: "provider.refresh" }));
  const link = "inline-flex items-center gap-1 text-[11.5px] text-muted underline-offset-2 hover:text-fg hover:underline disabled:opacity-40";
  return (
    <div data-nodrag role="group" aria-label={r.state === "none" ? "Set up a provider" : "Log in to a provider"} className={`space-y-2 rounded-lg border border-accent/35 px-3 py-2.5 ${className}`}>
      {r.state === "none" ? (
        <>
          <p className="text-[12px]">No agent can run on {machine.name} yet: it has no provider.</p>
          <div className="flex flex-wrap gap-1.5">
            <button type="button" onClick={() => openRegistry(machine.id)} className={primary}>
              <Plus className="size-3.5" /> Add from the ACP registry
            </button>
            <button type="button" onClick={() => openMachineSettings(machine.id, "Endpoints")} className="inline-flex h-7 items-center gap-1.5 rounded-md border border-line-2 px-2.5 text-[12px] text-muted transition-colors hover:bg-fg/[0.05] hover:text-fg">
              <Cable className="size-3.5" /> Use an endpoint or gateway
            </button>
          </div>
          <p className="text-[11px] text-subtle">
            Or install Claude Code or Codex on it, then{" "}
            <button type="button" disabled={busy !== null} onClick={refresh} className={link}>
              {busy === "refresh" ? <span className="shimmer">checking…</span> : "check again"}
            </button>
            .
          </p>
        </>
      ) : (
        <>
          <p className="text-[12px]">Log in to a provider to start working on {machine.name}.</p>
          <div className="flex flex-wrap gap-1.5">
            {r.missing.map((p) => (
              <button key={p.id} type="button" disabled={busy !== null || !machine.online} onClick={() => login(p)} className={primary}>
                <LogIn className="size-3.5" /> {busy === `login:${p.id}` ? <span className="shimmer">Opening…</span> : `Log in to ${p.name}`}
              </button>
            ))}
          </div>
          <p className="text-[11px] text-subtle">
            It opens a terminal: follow the link it prints, then paste the code. Or{" "}
            <button type="button" onClick={() => openMachineSettings(machine.id, "Endpoints")} className={link}>
              use an endpoint
            </button>
            ,{" "}
            <button type="button" onClick={() => openRegistry(machine.id)} className={link}>
              add another agent
            </button>
            , or{" "}
            <button type="button" disabled={busy !== null} onClick={refresh} className={link}>
              {busy === "refresh" ? <span className="shimmer">check again</span> : "check again"}
            </button>{" "}
            after logging in elsewhere.
          </p>
        </>
      )}
      {error && (
        <p role="alert" className="text-[12px] text-danger">
          {error}
        </p>
      )}
    </div>
  );
}
