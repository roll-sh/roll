"use client";

import { ChevronRight, Cloud, KeyRound, Laptop, X } from "lucide-react";
import Link from "next/link";
import { useEffect, useRef, useState } from "react";
import type { Machine } from "@/lib/agents-api";
import { formatHourly, type SandboxSize } from "@/lib/sandboxes-api";
import { CopyButton, input, primary, useCopy } from "../ui";
import { apiFlag, useInstallLine } from "./connect";
import { useAgents } from "./use-agents";

/** Provider keys a sandbox can be given at create (passed to the sandbox only, never shown again). */
export const PROVIDER_KEYS = [
  { name: "ANTHROPIC_API_KEY", label: "Anthropic API key", for: "Claude Code" },
  { name: "OPENAI_API_KEY", label: "OpenAI API key", for: "Codex" },
] as const;

/** The non-empty keys, trimmed: what goes in `env`. */
export function sandboxEnv(keys: Record<string, string>): Record<string, string> | undefined {
  const env = Object.fromEntries(
    Object.entries(keys)
      .map(([k, v]) => [k, v.trim()] as const)
      .filter(([, v]) => v),
  );
  return Object.keys(env).length ? env : undefined;
}

/**
 * The two ways to get a machine on the canvas, side by side: connect your own computer, or rent a
 * sandbox roll hosts. `onCreated` gets a new sandbox's machine (it's on the canvas at once,
 * pulsing while it boots).
 */
export function AddMachinePaths({ onCreated }: { onCreated?: (m: Machine) => void }) {
  return (
    <div className="grid gap-3 text-left sm:grid-cols-2">
      <ConnectComputer />
      <RentSandbox onCreated={onCreated} />
    </div>
  );
}

function Path({ icon, title, lead, children }: { icon: React.ReactNode; title: string; lead: string; children: React.ReactNode }) {
  return (
    <section className="flex min-w-0 flex-col gap-3 rounded-xl border border-line-2 bg-panel p-4 shadow-card inset-shadow-highlight" aria-label={title}>
      <div className="flex items-center gap-2.5">
        <span className="grid size-7 shrink-0 place-items-center rounded-lg border border-line bg-panel-2 text-muted inset-shadow-highlight [&>svg]:size-3.5">{icon}</span>
        <h2 className="text-[14px] font-[590] tracking-[-0.02em]">{title}</h2>
      </div>
      <p className="text-[12.5px] leading-relaxed text-muted">{lead}</p>
      {children}
    </section>
  );
}

function ConnectComputer() {
  const { copied, copy } = useCopy();
  const steps = [useInstallLine(), `roll agent login${apiFlag()}`, "roll agent up"];
  return (
    <Path
      icon={<Laptop className="size-4 text-muted" />}
      title="Connect your computer"
      lead="Agents run on your machine with the logins already there (claude, codex). It connects out to roll: no ports, no tunnel."
    >
      <ol className="space-y-1.5">
        {steps.map((cmd, i) => (
          <li key={cmd} className="flex items-center gap-1 rounded-lg border border-line bg-fg/[0.03] py-1.5 pr-1 pl-2.5">
            <span className="w-3 shrink-0 font-mono text-[10.5px] text-subtle">{i + 1}</span>
            <code className="min-w-0 flex-1 truncate font-mono text-[11.5px]" title={cmd}>
              {cmd}
            </code>
            <CopyButton id={cmd} text={cmd} copied={copied} onCopy={copy} />
          </li>
        ))}
      </ol>
      <p className="text-[12px] text-subtle">
        <code className="font-mono">roll agent login</code> prints a code and opens a page to approve it. The machine shows up here when it&apos;s up.
      </p>
      <Link href="/dashboard/agents/connect" className="mt-auto inline-flex items-center gap-0.5 text-[12.5px] text-accent hover:underline">
        A code, or a machine token <ChevronRight className="size-3.5" />
      </Link>
    </Path>
  );
}

function RentSandbox({ onCreated }: { onCreated?: (m: Machine) => void }) {
  const { owner, services, sandboxes, client } = useAgents();
  const list = sandboxes?.list ?? null;
  const [size, setSize] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [showKeys, setShowKeys] = useState(false);
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const chosen = list?.sizes.find((s) => s.name === size) ?? list?.sizes[0];
  const used = list?.sandboxes.length ?? 0;
  const full = list ? used >= list.max : false;

  const lead = "An always-on machine roll hosts for you, with Claude Code and Codex installed. Log the providers in from the canvas, or give it API keys.";
  if (!services || (!list && !sandboxes?.error))
    return (
      <Path icon={<Cloud className="size-4 text-muted" />} title="Rent a sandbox" lead={lead}>
        <p className="text-[12.5px] text-subtle">
          <span className="shimmer">Loading sizes…</span>
        </p>
      </Path>
    );
  if (!list || !list.backend)
    return (
      <Path icon={<Cloud className="size-4 text-muted" />} title="Rent a sandbox" lead={lead}>
        <p className="rounded-lg border border-line bg-bg px-3 py-2 text-[12.5px] text-muted">
          {!list ? "Only the owner can rent sandboxes." : "Sandboxes aren't on for this roll server."}
        </p>
      </Path>
    );

  return (
    <Path icon={<Cloud className="size-4 text-muted" />} title="Rent a sandbox" lead={lead}>
      <form
        className="flex flex-col gap-3"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!chosen) return;
          setBusy(true);
          setError(null);
          try {
            const r = await services.sandboxes.create(owner, { name: name.trim() || undefined, size: chosen.name, env: sandboxEnv(keys) });
            setKeys({});
            // On the canvas at once; the stream keeps it up to date from here.
            client.enqueue({ type: "machine", cursor: 0, machine: r.machine });
            sandboxes?.reload();
            onCreated?.(r.machine);
          } catch (err) {
            setError(err instanceof Error ? err.message : String(err));
          } finally {
            setBusy(false);
          }
        }}
      >
        <fieldset className="min-w-0 space-y-1.5">
          <legend className="sr-only">Size</legend>
          {list.sizes.map((s) => (
            <SizeOption key={s.name} size={s} checked={s.name === chosen?.name} onChange={() => setSize(s.name)} />
          ))}
        </fieldset>
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Name (optional)" aria-label="Sandbox name" className={`${input} w-full`} />
        {showKeys ? (
          <div className="space-y-1.5" role="group" aria-label="Provider API keys">
            {PROVIDER_KEYS.map((k) => (
              <input
                key={k.name}
                type="password"
                autoComplete="off"
                value={keys[k.name] ?? ""}
                onChange={(e) => setKeys((all) => ({ ...all, [k.name]: e.target.value }))}
                placeholder={`${k.label} (${k.for})`}
                aria-label={k.label}
                className={`${input} w-full font-mono text-[12px]`}
              />
            ))}
            <p className="text-[11.5px] text-subtle">Sent to the sandbox only; roll never shows them again.</p>
          </div>
        ) : (
          <button type="button" onClick={() => setShowKeys(true)} className="inline-flex items-center gap-1.5 self-start text-[12px] text-muted hover:text-fg">
            <KeyRound className="size-3.5" /> Add provider API keys (optional)
          </button>
        )}
        <div className="flex items-center gap-2">
          <button type="submit" className={primary} disabled={busy || full || !chosen}>
            {busy ? <span className="shimmer">Renting…</span> : `Rent for ${formatHourly(chosen?.hourly_cents ?? 0)}`}
          </button>
          <span className="text-[11.5px] text-subtle">
            {used} of {list.max} in use
          </span>
        </div>
        {full && <p className="text-[12px] text-muted">You have as many sandboxes as you can. Delete one to rent another.</p>}
        {error && <p className="text-[12px] text-danger">{error}</p>}
      </form>
    </Path>
  );
}

function SizeOption({ size, checked, onChange }: { size: SandboxSize; checked: boolean; onChange: () => void }) {
  return (
    <label
      className={`flex min-w-0 cursor-pointer items-center gap-2.5 rounded-lg border px-3 py-2 text-[12.5px] transition-colors duration-150 has-focus-visible:outline-2 has-focus-visible:outline-accent/70 ${checked ? "border-accent/45 bg-accent/[0.06]" : "border-line hover:border-line-2 hover:bg-fg/[0.03]"}`}
    >
      <input type="radio" name="sandbox-size" checked={checked} onChange={onChange} className="sr-only" />
      <span aria-hidden className={`grid size-3.5 shrink-0 place-items-center rounded-full border transition-colors duration-150 ${checked ? "border-accent bg-accent" : "border-line-3"}`}>
        <span className={`size-1.5 rounded-full bg-white transition-transform duration-150 ${checked ? "scale-100" : "scale-0"}`} />
      </span>
      <span className="font-[510]">{size.name}</span>
      <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-subtle">
        {size.cpu} vCPU · {size.memory_gb} GB · {size.disk_gb} GB disk
      </span>
      <span className="font-mono text-[11.5px]">{formatHourly(size.hourly_cents)}</span>
    </label>
  );
}

/** "Add machine" on the canvas: both paths in a sheet over it. Esc or the backdrop closes it. */
export function AddMachineDialog({ onClose, onCreated }: { onClose: () => void; onCreated: (m: Machine) => void }) {
  const panel = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    panel.current?.focus();
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="fade-in absolute inset-0 z-30 overflow-y-auto bg-(--scrim) backdrop-blur-[3px]" onPointerDown={(e) => e.target === e.currentTarget && onClose()}>
      <div
        ref={panel}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label="Add a machine"
        className="sheet-in mx-auto my-4 w-[min(760px,calc(100%-1.5rem))] space-y-4 rounded-2xl border border-line-2 bg-sheet p-5 shadow-sheet inset-shadow-highlight outline-none sm:my-[10vh]"
      >
        <div className="flex items-start gap-2">
          <div className="space-y-1">
            <h2 className="text-[17px] font-[590] tracking-[-0.025em]">Add a machine</h2>
            <p className="text-[12.5px] text-muted">Run agents on your own computer, or on a sandbox roll hosts for you.</p>
          </div>
          <button onClick={onClose} aria-label="Close" className="-mt-1 -mr-1 ml-auto grid size-8 place-items-center rounded-lg text-subtle transition-colors duration-150 hover:bg-fg/[0.06] hover:text-fg">
            <X className="size-4" strokeWidth={1.75} />
          </button>
        </div>
        <AddMachinePaths onCreated={onCreated} />
      </div>
    </div>
  );
}
