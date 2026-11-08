import { ChevronRight, Cloud, KeyRound, Laptop } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Link } from "@/components/link";
import { flushSync } from "react-dom";
import type { EndpointProfile, Machine } from "@/lib/agents-api";
import { NewSandboxGatewayNotice } from "./gateway/machine";
import { KIND_LABEL } from "@/lib/agents-endpoints";
import { formatHourly, type SandboxSize } from "@/lib/sandboxes-api";
import { SandboxesProblem } from "./sandbox";
import { CopyButton, primary, useCopy } from "../ui";
import { useAutomationsStore } from "./automations/shared";
import { apiFlag, useInstallLine } from "./connect";
import { applyWhenReady, type PendingApply } from "./endpoint-settings";
import { field } from "./settings-ui";
import { Select } from "./ui";
import { SlidePages } from "./slide-pages";
import { Dialog } from "./ui/dialog";
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
 * pulsing while it boots); with an endpoint profile picked, once the profile is set on it.
 * `dialog`: closing it is the user's (an endpoint still waiting for the sandbox is given up, with a
 * notice); elsewhere the wait goes on after the paths go away (the canvas replaces them).
 */
export function AddMachinePaths({ onCreated, dialog = false }: { onCreated?: (m: Machine) => void; dialog?: boolean }) {
  return (
    <div className="grid items-start gap-3 text-left sm:grid-cols-2">
      <ConnectComputer />
      <RentSandbox onCreated={onCreated} dialog={dialog} />
    </div>
  );
}

/** One way to add a machine. `page` names what it shows: a change crossfades in place (loading, the form, setting up). */
function Path({ icon, title, lead, page, children }: { icon: React.ReactNode; title: string; lead: string; page?: string; children: React.ReactNode }) {
  return (
    <section className="flex min-w-0 flex-col gap-3 rounded-xl border border-line-2 bg-panel p-4 shadow-card inset-shadow-highlight" aria-label={title}>
      <div className="flex items-center gap-2.5">
        <span className="grid size-7 shrink-0 place-items-center rounded-lg border border-line bg-panel-2 text-muted inset-shadow-highlight [&>svg]:size-3.5">{icon}</span>
        <h2 className="text-[14px] font-[590] tracking-[-0.02em]">{title}</h2>
      </div>
      <p className="relative text-[12.5px] leading-relaxed text-muted">
        <SlidePages as="span" page={lead} direction={0} className="block">
          {lead}
        </SlidePages>
      </p>
      {page === undefined ? (
        children
      ) : (
        <div className="relative">
          <SlidePages page={page} direction={0} className="flex flex-col gap-3">
            {children}
          </SlidePages>
        </div>
      )}
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

function RentSandbox({ onCreated, dialog }: { onCreated?: (m: Machine) => void; dialog: boolean }) {
  const { owner, services, sandboxes, client } = useAgents();
  const notices = useAutomationsStore();
  const list = sandboxes?.list ?? null;
  const [size, setSize] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [showKeys, setShowKeys] = useState(false);
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const profiles = useProfiles();
  const [picked, setPicked] = useState<string | null>(null);
  const [endpointKey, setEndpointKey] = useState("");
  const [pending, setPending] = useState<{ machine: Machine; profile: EndpointProfile; state: PendingApply } | null>(null);
  const cancel = useRef<(() => boolean) | null>(null);
  const mounted = useRef(true);
  const pendingRef = useRef(pending);
  pendingRef.current = pending;
  const chosen = list?.sizes.find((s) => s.name === size) ?? list?.sizes[0];
  const used = list?.sandboxes.length ?? 0;
  const full = list ? used >= list.max : false;
  // None, or the profile marked the default for new sandboxes, until one is picked.
  const profileId = picked ?? profiles?.find((p) => p.sandbox_default)?.id ?? NONE;
  const profile = profiles?.find((p) => p.id === profileId) ?? null;

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      // Closing the dialog gives up an endpoint still waiting for its sandbox; say so, quietly.
      const p = pendingRef.current;
      if (dialog && p && cancel.current?.()) {
        notices?.local({ level: "info", title: `${p.profile.name} wasn't set on ${p.machine.name}`, body: "Apply it from the machine's settings once it's up.", link: `#agents/settings/m:${p.machine.id}` });
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Set the profile on the new sandbox once it says hello (`applyWhenReady`), the key sealed then. */
  const applyLater = (m: Machine, p: EndpointProfile, key: string | undefined) => {
    setPending({ machine: m, profile: p, state: { state: "waiting" } });
    cancel.current = applyWhenReady(client, m.id, p, key, (state) => {
      if (mounted.current) {
        setPending({ machine: m, profile: p, state });
        if (state.state === "done") onCreated?.(state.machine);
      } else if (state.state !== "waiting" && state.state !== "applying") {
        // The paths went away (the canvas took their place): the outcome is a notice.
        notices?.local(
          state.state === "done"
            ? { level: "success", title: `${p.name} is set on ${m.name}` }
            : { level: "warning", title: `${p.name} wasn't set on ${m.name}`, body: state.state === "failed" ? state.error : "It didn't connect in time: apply it from the machine's settings.", link: `#agents/settings/m:${m.id}` },
        );
      }
    });
  };

  const lead = "A machine roll hosts for you, with Claude Code and Codex installed. It runs while its agents work, sleeps when they're idle (not billed asleep) and wakes when one needs it. Log the providers in from the canvas, or give it API keys.";
  if (pending)
    return (
      <Path icon={<Cloud className="size-4 text-muted" />} title="Rent a sandbox" lead={`${pending.machine.name} is on the canvas, starting up.`} page="applying">
        <Applying pending={pending} onDone={() => onCreated?.(pending.machine)} />
      </Path>
    );
  if (!services || (!list && !sandboxes?.error))
    return (
      <Path icon={<Cloud className="size-4 text-muted" />} title="Rent a sandbox" lead={lead} page="loading">
        <p className="text-[12.5px] text-subtle">
          <span className="shimmer">Loading sizes…</span>
        </p>
      </Path>
    );
  if (!list || !list.backend)
    return (
      <Path icon={<Cloud className="size-4 text-muted" />} title="Rent a sandbox" lead={lead} page="off">
        {!list ? (
          <SandboxesProblem error={sandboxes?.error ?? { message: "Only the owner can rent sandboxes.", forbidden: true }} onRetry={() => sandboxes?.reload()} />
        ) : (
          <p className="rounded-lg border border-line bg-bg px-3 py-2 text-[12.5px] text-muted">Sandboxes aren&apos;t on for this roll server.</p>
        )}
      </Path>
    );

  return (
    <Path icon={<Cloud className="size-4 text-muted" />} title="Rent a sandbox" lead={lead} page="form">
      <form
        className="flex flex-col gap-3"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!chosen) return;
          setBusy(true);
          setError(null);
          try {
            const r = await services.sandboxes.create(owner, { name: name.trim() || undefined, size: chosen.name, env: sandboxEnv(keys) });
            // The key waits in the wait's closure only, until the sandbox's seal key comes.
            const key = profile && !profile.key_ref ? endpointKey.trim() || undefined : undefined;
            // Emptied in a commit of their own before the form gives way: it stays drawn while it
            // fades out, and it mustn't hold the keys then.
            flushSync(() => {
              setKeys({});
              setEndpointKey("");
            });
            // On the canvas at once; the stream keeps it up to date from here.
            client.enqueue({ type: "machine", cursor: 0, machine: r.machine });
            sandboxes?.reload();
            if (profile) applyLater(r.machine, profile, key);
            else onCreated?.(r.machine);
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
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Name (optional)" aria-label="Sandbox name" className={`${field} w-full`} />
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
                className={`${field} w-full font-mono text-[12px] placeholder-shown:font-sans placeholder-shown:text-[12.5px]`}
              />
            ))}
            <p className="text-[11.5px] text-subtle">Sent to the sandbox only; roll never shows them again.</p>
          </div>
        ) : (
          <button type="button" onClick={() => setShowKeys(true)} className="inline-flex items-center gap-1.5 self-start text-[12px] text-muted hover:text-fg">
            <KeyRound className="size-3.5" /> Add provider API keys (optional)
          </button>
        )}
        {profiles && profiles.length > 0 && (
          <div className="space-y-1.5" role="group" aria-label="Endpoint">
            <div className="flex items-center gap-2">
              <span className="shrink-0 text-[12px] text-muted">Endpoint</span>
              <Select
                label="Endpoint"
                className="min-w-0 flex-1"
                value={profileId}
                onChange={setPicked}
                options={[{ value: NONE, label: "None" }, ...profiles.map((p) => ({ value: p.id, label: p.name, hint: p.sandbox_default ? "default" : KIND_LABEL[p.kind] }))]}
              />
            </div>
            {profile && !profile.key_ref && (
              <input
                type="password"
                autoComplete="off"
                spellCheck={false}
                value={endpointKey}
                onChange={(e) => setEndpointKey(e.target.value)}
                placeholder={`Key for ${profile.name} (optional)`}
                aria-label="Endpoint key"
                data-1p-ignore
                className={`${field} w-full font-mono text-[12px] placeholder-shown:font-sans placeholder-shown:text-[12.5px]`}
              />
            )}
            {profile && <p className="text-[11.5px] text-subtle">Set once the sandbox connects{profile.key_ref ? ", its key read from 1Password there" : ", the key sealed to it in your browser"}.</p>}
          </div>
        )}
        <NewSandboxGatewayNotice backend={list.backend} />
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

const NONE = "__none";

/** The owner's endpoint profiles, for the new-sandbox form (`null` while loading or when they can't be read). */
function useProfiles(): EndpointProfile[] | null {
  const { owner, services } = useAgents();
  const [profiles, setProfiles] = useState<EndpointProfile[] | null>(null);
  useEffect(() => {
    let live = true;
    services?.endpoints
      .list(owner)
      .then((ps) => live && setProfiles(ps))
      .catch(() => live && setProfiles(null));
    return () => {
      live = false;
    };
  }, [services, owner]);
  return profiles;
}

/** A new sandbox waiting to take its endpoint: "Setting up CLIProxyAPI…", then how it went. */
function Applying({ pending: { machine, profile, state }, onDone }: { pending: { machine: Machine; profile: EndpointProfile; state: PendingApply }; onDone: () => void }) {
  const stuck = state.state === "failed" || state.state === "timeout";
  return (
    <>
      <div className="space-y-2 rounded-lg border border-line bg-panel-2 px-3 py-2.5 text-[12.5px]" role="status" aria-label="Setting up the endpoint">
        {stuck ? (
          <p className="text-danger">{state.state === "failed" ? state.error : `${machine.name} didn't connect within 3 minutes.`}</p>
        ) : (
          <p className="flex items-center gap-2">
            <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-accent motion-reduce:animate-none" aria-hidden />
            <span className="shimmer">Setting up {profile.name}…</span>
          </p>
        )}
        <p className="text-[11.5px] text-subtle">
          {stuck
            ? `Apply ${profile.name} from ${machine.name}'s settings once it's up.`
            : state.state === "waiting"
              ? `Waiting for ${machine.name} to connect (up to 3 minutes), then the endpoint is set on it.`
              : `Sealing the key to ${machine.name} and setting ${profile.name}.`}
        </p>
      </div>
      {stuck && (
        <button type="button" onClick={onDone} className={`fade-in ${primary} self-start`}>
          Go to {machine.name}
        </button>
      )}
    </>
  );
}

export function SizeOption({ size, checked, onChange }: { size: SandboxSize; checked: boolean; onChange: () => void }) {
  return (
    <label
      className={`flex min-w-0 cursor-pointer items-center gap-2.5 rounded-lg border px-3 py-2 text-[12.5px] transition-colors duration-150 has-focus-visible:outline-2 has-focus-visible:outline-accent/70 ${checked ? "border-accent/45 bg-accent/[0.06]" : "border-line hover:border-line-2 hover:bg-fg/[0.03]"}`}
    >
      <input type="radio" name="sandbox-size" checked={checked} onChange={onChange} className="sr-only" />
      <span aria-hidden className={`grid size-3.5 shrink-0 place-items-center rounded-full border transition-colors duration-150 ${checked ? "border-accent bg-accent" : "border-line-3"}`}>
        <span className={`size-1.5 rounded-full bg-white transition-transform duration-150 ${checked ? "scale-100" : "scale-0"}`} />
      </span>
      {/* The name, and the whole spec under it: one line cut the disk off ("20 GB di…"). */}
      <span className="min-w-0 flex-1">
        <span className="block font-[510]">{size.name}</span>
        <span className="block font-mono text-[11px] text-subtle">
          {size.cpu} vCPU · {size.memory_gb} GB · {size.disk_gb} GB disk
        </span>
      </span>
      <span className="shrink-0 font-mono text-[11.5px]">{formatHourly(size.hourly_cents)}</span>
    </label>
  );
}

/**
 * "Add machine" on the canvas: both paths in the shared dialog over it (focus kept inside and given
 * back; Escape or a press outside closes it, and it shrinks back out before it unmounts).
 */
export function AddMachineDialog({ open = true, onExited, onClose, onCreated }: { open?: boolean; onExited?: () => void; onClose: () => void; onCreated: (m: Machine) => void }) {
  return (
    <Dialog open={open} onClose={onClose} onExited={onExited} className="sm:w-[min(760px,100%)]" title="Add a machine" lead="Run agents on your own computer, or on a sandbox roll hosts for you.">
      <AddMachinePaths onCreated={onCreated} dialog />
    </Dialog>
  );
}
