/**
 * Adding an agent from the ACP registry to a machine, in its settings' Providers section: T3
 * Code's registry step (`AcpRegistrySearchStep.tsx`, its `AcpRegistryAgentRow`, and
 * `AcpRegistryIcon.tsx` for the icons) as a popover from "Add agent". The machine's daemon lists
 * the registry (`registry.list`) and installs (`registry.install`); a row says what installing it
 * would do here. Installed agents' provider cards get "Remove" (`RegistryRemoveRow`).
 */

import { ExternalLink, Plus, Search, Trash2, X } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { reachable, type Machine, type ProviderBase, type RegistryAgent, type RegistryList, type Reply } from "@/lib/agents-api";
import { aboutUrl, installAgent, installedFor, registryIcon, registryUnavailable, rowLabel, rowState, searchAgents } from "@/lib/agents-registry";
import { AcpRegistryLogo } from "./provider-logos";
import { ago } from "./shared";
import { SettingRow, type Setting } from "./settings-ui";
import { cn, Popover, PopoverPopup, PopoverTrigger } from "./ui";
import { useAgents } from "./use-agents";
import { useEscapeLayer } from "./escape-layers";

const trigger = "flex h-6 items-center gap-1 rounded-md px-1.5 text-[11.5px] text-subtle hover:bg-fg/[0.06] hover:text-fg disabled:opacity-50";

/** "Add agent" and its popover. */
export function AddAgent({ machine }: { machine: Machine }) {
  const [open, setOpen] = useState(false);
  const why = registryUnavailable(machine, reachable(machine));
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger disabled={!!why} title={why ?? "Install an agent from the ACP registry on this machine"} className={trigger}>
        <Plus className="size-3" /> Add agent
      </PopoverTrigger>
      <PopoverPopup side="bottom" align="end" sideOffset={6} className="w-[min(440px,calc(100vw-24px))] flex-col overflow-hidden rounded-xl">
        {open && <RegistrySheet machine={machine} />}
      </PopoverPopup>
    </Popover>
  );
}

type Listing = { state: "loading" } | { state: "ready"; list: RegistryList } | { state: "failed"; error: string };

const errorText = (r: Reply) => (r.type === "error" ? r.message : `unexpected reply ${r.type}`);

/** The search and the rows: what's in the popover. */
export function RegistrySheet({ machine }: { machine: Machine }) {
  const { client } = useAgents();
  const [listing, setListing] = useState<Listing>({ state: "loading" });
  const [query, setQuery] = useState("");
  const [installing, setInstalling] = useState<string | null>(null);
  const [rowError, setRowError] = useState<{ id: string; message: string } | null>(null);
  const live = useRef(true);
  // Set on every mount: React's development double mount runs the cleanup once in between.
  useEffect(() => {
    live.current = true;
    return () => void (live.current = false);
  }, []);

  const load = async () => {
    const r = await client.send(machine.id, { type: "registry.list" }).catch((e: unknown) => ({ type: "error", code: "internal", message: e instanceof Error ? e.message : String(e) }) as Reply);
    if (!live.current) return;
    setListing(r.type === "registry" ? { state: "ready", list: r } : { state: "failed", error: errorText(r) });
  };
  useEffect(() => {
    void load();
    // Once per opening.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [machine.id]);

  const install = async (agent: RegistryAgent) => {
    setRowError(null);
    setInstalling(agent.id);
    const r = await installAgent((m, c, id) => client.send(m, c, id), machine.id, agent.id).catch((e: unknown) => ({ type: "error", code: "internal", message: e instanceof Error ? e.message : String(e) }) as Reply);
    if (!live.current) return;
    if (r.type === "error") setRowError({ id: agent.id, message: r.message });
    await load();
    if (live.current) setInstalling(null);
  };

  const agents = listing.state === "ready" ? listing.list.agents : [];
  const shown = useMemo(() => searchAgents(agents, query), [agents, query]);
  return (
    <div className="flex min-h-0 flex-col">
      <div className="flex items-center gap-2 border-b border-line px-3 py-2.5">
        <Search className="size-3.5 shrink-0 text-subtle" aria-hidden />
        <input
          autoFocus
          type="search"
          aria-label="Search the ACP registry"
          placeholder="Search agents…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          spellCheck={false}
          className="min-w-0 flex-1 bg-transparent text-[13px] outline-none placeholder:text-subtle"
        />
        <span className="shrink-0 text-[11px] text-subtle">ACP registry</span>
      </div>
      <div className="sr-only" role="status">
        {listing.state === "loading" ? "Loading the ACP registry." : listing.state === "ready" ? `${shown.length} ${shown.length === 1 ? "agent" : "agents"}.` : ""}
      </div>
      {listing.state === "ready" && listing.list.error && (
        <p className="border-b border-line px-3 py-2 text-[12px] text-subtle">
          {listing.list.agents.length ? `Couldn't reach the registry: showing the list fetched ${ago(listing.list.fetched_at)}.` : "Couldn't reach the registry."}{" "}
          <span className="text-muted">{listing.list.error}</span>
        </p>
      )}
      <div className="max-h-[min(420px,60vh)] overflow-y-auto overscroll-contain px-1.5 py-1">
        {listing.state === "loading" && <p className="shimmer px-2 py-6 text-center text-[12.5px] text-subtle">Looking at the registry…</p>}
        {listing.state === "failed" && (
          <p role="alert" className="px-2 py-4 text-[12.5px] text-danger">
            {listing.error}
          </p>
        )}
        {listing.state === "ready" && shown.length === 0 && (
          <p className="px-2 py-6 text-center text-[12.5px] text-subtle">{agents.length ? "No agent matches." : "The registry lists no agents."}</p>
        )}
        {shown.map((a) => (
          <AgentRow
            key={a.id}
            agent={a}
            installing={installing === a.id}
            busy={installing !== null}
            error={rowError?.id === a.id ? rowError.message : null}
            onInstall={() => void install(a)}
          />
        ))}
      </div>
    </div>
  );
}

const button = "flex h-7 shrink-0 items-center rounded-md px-2.5 text-[12px] transition-colors disabled:cursor-default";

function AgentRow({ agent, installing, busy, error, onInstall }: { agent: RegistryAgent; installing: boolean; busy: boolean; error: string | null; onInstall: () => void }) {
  const state = rowState(agent, installing);
  const label = rowLabel(state);
  const about = aboutUrl(agent);
  const actionable = state.kind === "install" || state.kind === "update";
  const title =
    state.kind === "here"
      ? `Already on this machine as ${state.provider}`
      : state.kind === "needs"
        ? `Install ${state.tool} on the machine to install ${agent.name} (its package goes into roll's folder, not globally)`
        : state.kind === "unavailable"
          ? `The registry has no ${agent.name} for this machine's system`
          : agent.integrity === "sha256"
            ? "Downloaded and checked against the registry's SHA-256"
            : agent.integrity === "registry"
              ? "An exact package version, installed into roll's folder"
              : "The registry lists no checksum for this download";
  return (
    <article className="rounded-lg px-1.5 py-2 hover:bg-fg/[0.03]" data-agent={agent.id}>
      <div className="flex min-w-0 items-center gap-2.5">
        <RegistryIcon icon={agent.icon} />
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-baseline gap-1.5">
            <h4 className="min-w-0 truncate text-[13px] font-medium tracking-[-0.01em]">{agent.name}</h4>
            <span className="shrink-0 font-mono text-[10.5px] text-subtle tabular-nums">{agent.version}</span>
          </div>
          {agent.description && <p className="truncate text-[12px] text-subtle">{agent.description}</p>}
        </div>
        {about && (
          <a href={about} target="_blank" rel="noreferrer" aria-label={`About ${agent.name}`} title={`About ${agent.name}`} className="flex h-7 shrink-0 items-center gap-1 rounded-md px-1.5 text-[11.5px] text-subtle hover:bg-fg/[0.06] hover:text-fg">
            About
            <ExternalLink aria-hidden className="size-3" />
          </a>
        )}
        <button
          type="button"
          disabled={!actionable || busy}
          onClick={onInstall}
          aria-label={`${label} ${agent.name}`}
          title={title}
          className={cn(
            button,
            actionable && !busy && "border border-line-2 bg-raised text-fg shadow-card inset-shadow-highlight hover:bg-fg/[0.06]",
            actionable && busy && "border border-line-2 text-subtle opacity-60",
            state.kind === "installing" && "border border-accent/40 text-fg",
            (state.kind === "installed" || state.kind === "here") && "text-muted",
            (state.kind === "needs" || state.kind === "unavailable") && "text-subtle",
          )}
        >
          {state.kind === "installing" ? <span className="shimmer">{label}</span> : label}
        </button>
      </div>
      {error && (
        <p role="alert" className="mt-1.5 pl-[38px] text-[12px] leading-snug text-danger">
          {error}
        </p>
      )}
    </article>
  );
}

/** T3's registry icon: the CDN's SVG recoloured to the text's colour (an external image can't take `currentColor`), the registry's mark until it loads or when it can't. */
export function RegistryIcon({ icon, className }: { icon: string | undefined; className?: string }) {
  const src = registryIcon(icon);
  const filter = `registry-icon-${useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  const [status, setStatus] = useState<"loading" | "loaded" | "failed">("loading");
  return (
    <span aria-hidden className={cn("relative grid size-7 shrink-0 place-items-center rounded-md border border-line bg-panel-2 text-muted inset-shadow-highlight", className)}>
      <svg className="pointer-events-none absolute size-0" focusable="false">
        <defs>
          <filter id={filter} colorInterpolationFilters="sRGB">
            <feFlood floodColor="currentColor" />
            <feComposite in2="SourceAlpha" operator="in" />
          </filter>
        </defs>
      </svg>
      {status !== "loaded" && <AcpRegistryLogo className="w-4" />}
      {src && status !== "failed" && (
        <img
          src={src}
          alt=""
          referrerPolicy="no-referrer"
          onLoad={() => setStatus("loaded")}
          onError={() => setStatus("failed")}
          className={cn("size-4", status !== "loaded" && "hidden")}
          style={{ filter: `url(#${filter})` }}
        />
      )}
    </span>
  );
}

/** A provider card's row for an agent the daemon installed from the registry: what it is, and Remove. */
export function RegistryRemoveRow({ machine, base }: { machine: Machine; base: ProviderBase }) {
  const { client } = useAgents();
  const installed = installedFor(machine, base.id);
  const [asking, setAsking] = useState(false);
  useEscapeLayer(asking, () => setAsking(false));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!installed) return null;
  const s: Setting = {
    id: `p.${base.id}.registry`,
    title: "From the ACP registry",
    section: base.name,
    description: `Version ${installed.version}, installed by roll on this machine. Removing it deletes its files there.`,
  };
  const remove = async () => {
    setBusy(true);
    setError(null);
    try {
      await client.run(machine.id, { type: "registry.remove", agent: installed.id }, "ok");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      setAsking(false);
    }
  };
  const disabled = !reachable(machine);
  return (
    <SettingRow
      s={s}
      control={
        asking ? (
          <span className="flex items-center gap-1">
            <button type="button" disabled={busy} onClick={remove} className="h-7 rounded-md bg-danger/10 px-2.5 text-[12px] font-medium text-danger hover:bg-danger/20">
              {busy ? <span className="shimmer">Removing…</span> : `Remove ${base.name}`}
            </button>
            <button type="button" onClick={() => setAsking(false)} className="grid size-7 place-items-center rounded-md text-subtle hover:text-fg" aria-label="Keep it">
              <X className="size-3.5" />
            </button>
          </span>
        ) : (
          <button type="button" disabled={disabled} onClick={() => setAsking(true)} className="flex h-7 items-center gap-1.5 rounded-md border border-line-2 px-2.5 text-[12px] text-danger hover:bg-danger/10 disabled:opacity-50">
            <Trash2 className="size-3.5" /> Remove
          </button>
        )
      }
    >
      {error && (
        <p role="alert" className="mt-1.5 text-[12px] text-danger">
          {error}
        </p>
      )}
    </SettingRow>
  );
}
