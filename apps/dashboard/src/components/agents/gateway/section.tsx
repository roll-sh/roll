"use client";

/**
 * The Gateway section of the owner's settings (docs/code/gateway.md): roll's model gateway on one
 * of the owner's computers. Without a plan, a way into setup (`GatewaySetup`). With one: the
 * computer and whether it serves; upstreams with health, priority, on/off and keys; routes and
 * aliases; the machines that use it, under THE NOTICE; usage and cost; the live request log; and
 * its settings (port, addresses, bodies, affinity, retention, new sandboxes, fallback, remove).
 * Every change to the gateway is `gateway.configure` with the settings whole, checked here as the
 * daemon checks them; the host's `hello` (`MachineConfig.gateway`) is the truth shown.
 */

import { ChevronRight, Network, Plus, RefreshCw, X } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { useKept } from "../ui/dialog";
import { GATEWAY_PORT, GATEWAY_RETENTION, reachable, type GatewaySettings, type GatewayStatus, type UpstreamCheck, type UpstreamSpec } from "@/lib/agents-api";
import { checkSettings, gatewayOn, isIPv4, isIPv6, listNames, type GatewayCommands } from "@/lib/agents-gateway";
import { setAgentsView } from "@/lib/agents-surface";
import { MachineIcon } from "../identity";
import { SettingRow, SettingsSection, TextField, type Setting } from "../settings-ui";
import { cn, Select, Switch } from "../ui";
import { useAgents } from "../use-agents";
import { RequestLog } from "./log";
import { MembersList } from "./members";
import { RoutesList } from "./routes";
import { dangerBtn, iconBtn, message, Problem, RunState, smallBtn, useGatewayPlan, usePoll, useVisible } from "./shared";
import { UpstreamEditor, UpstreamItem } from "./upstreams";
import { UsageCharts } from "./usage";
import { Addresses, GatewaySetup, type Address } from "./wizard";
import { useEscapeLayer } from "../escape-layers";

const TERMS = ["gateway", "model gateway", "proxy", "cliproxy", "litellm", "upstream", "api key", "failover", "route", "alias", "usage", "cost", "log", "anthropic", "openai", "bedrock", "vertex", "azure", "openrouter"];

export const GATEWAY_SETTINGS = {
  gateway: {
    id: "gateway",
    title: "Model gateway",
    section: "Gateway",
    description: "One of your computers sends your machines' model requests on to your own API keys: failover between upstreams, one key per machine, usage and cost in one place.",
    terms: TERMS,
  },
  upstreams: { id: "gateway.upstreams", title: "Upstreams", section: "Gateway", description: "Where requests go, in priority order. Keys are sealed to the gateway's computer and never shown.", terms: [...TERMS, "provider", "key", "health", "circuit"] },
  routes: { id: "gateway.routes", title: "Routes and aliases", section: "Gateway", description: "Which upstreams serve a model, in order, and names of your own for models.", terms: ["route", "alias", "model", "failover", "priority"] },
  members: { id: "gateway.members", title: "Machines using it", section: "Gateway", description: "Each has a key of its own; stopping one revokes it.", terms: ["machine", "member", "client", "key", "revoke", "sandbox"] },
  usage: { id: "gateway.usage", title: "Usage and cost", section: "Gateway", description: "By day, from list prices or yours: an estimate, not a bill.", terms: ["usage", "cost", "tokens", "spend", "chart", "billing"] },
  log: { id: "gateway.log", title: "Request log", section: "Gateway", description: "Every request, live, with failovers and errors.", terms: ["log", "requests", "errors", "failover", "tail", "bodies"] },
  options: { id: "gateway.options", title: "Gateway settings", section: "Gateway", description: "Port, addresses, bodies, affinity, retention.", terms: ["port", "listen", "address", "tailscale", "lan", "docker", "bodies", "affinity", "retention", "remove", "fallback", "new sandboxes"] },
} satisfies Record<string, Setting>;

export const GATEWAY_SETTING_IDS = Object.values(GATEWAY_SETTINGS).map((s) => s.id);

const RETENTION = [30, 90, 180, 400, 730, 1825];

export function GatewaySection() {
  const g = useGatewayPlan();
  const [setup, setSetup] = useState(false);
  const wizard = useKept(setup);
  const { plan } = g;
  return (
    <SettingsSection title="Gateway" ids={GATEWAY_SETTING_IDS}>
      {g.unavailable ? (
        <SettingRow s={{ ...GATEWAY_SETTINGS.gateway, description: "Sign in to roll to set up a gateway." }} />
      ) : plan === undefined ? (
        <SettingRow s={GATEWAY_SETTINGS.gateway}>
          <p className="mt-2 text-[12.5px] text-subtle">
            <span className="shimmer">Loading…</span>
          </p>
        </SettingRow>
      ) : plan === null ? (
        <SettingRow
          s={GATEWAY_SETTINGS.gateway}
          note={<Network className="size-3 text-subtle" aria-hidden />}
          control={
            <button type="button" onClick={() => setSetup(true)} className={smallBtn}>
              Set up…
            </button>
          }
        >
          {g.error && (
            <div className="mt-2">
              <Problem>{g.error}</Problem>
            </div>
          )}
        </SettingRow>
      ) : (
        <GatewayRows g={g} onSetup={() => setSetup(true)} />
      )}
      {wizard.value && <GatewaySetup key={wizard.key} open={wizard.open} onExited={wizard.onExited} machines={g.machines} onClose={() => (setSetup(false), void g.reload())} onDone={(p) => g.setPlan(p)} />}
    </SettingsSection>
  );
}

function GatewayRows({ g, onSetup }: { g: ReturnType<typeof useGatewayPlan>; onSetup: () => void }) {
  const { client, services, owner } = useAgents();
  const plan = g.plan!;
  const host = g.host;
  const info = g.info;
  const gw = useMemo<GatewayCommands | null>(() => (host ? gatewayOn(client, host) : null), [client, host]);
  const live = !!host && reachable(host) && !!info;
  const settings = info?.settings;
  const ups = settings?.upstreams ?? [];
  const [status, setStatus] = useState<GatewayStatus | null>(null);
  const [checks, setChecks] = useState<Map<string, UpstreamCheck | "busy">>(new Map());
  const [editing, setEditing] = useState<{ at: number | "new"; key?: boolean } | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [asking, setAsking] = useState(false);
  useEscapeLayer(asking, () => setAsking(false));
  const [removing, setRemoving] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  const visible = useVisible(box);
  usePoll(visible && live, 15_000, async () => {
    if (!gw) return;
    try {
      setStatus(await gw.status());
    } catch {
      // The section says what's wrong elsewhere (offline, not set up); health just stays as it was.
    }
  });

  /** Send the settings whole, changed by `patch`; `null` when done, else why not. */
  const apply = async (patch: (s: GatewaySettings) => GatewaySettings): Promise<string | null> => {
    if (!gw || !settings) return "The gateway's computer isn't connected.";
    const checked = checkSettings(patch(settings));
    if ("error" in checked) return checked.error;
    try {
      await gw.configure(checked.settings);
      setStatus(await gw.status().catch(() => status));
      return null;
    } catch (e) {
      return message(e);
    }
  };
  const run = async (fn: () => Promise<string | null | void>) => {
    setProblem(null);
    const refused = await fn();
    if (refused) setProblem(refused);
  };
  const keyOf = (id: string) => info?.keys.find((k) => k.upstream === id)?.key ?? "none";
  const health = (id: string) => status?.upstreams.find((h) => h.upstream === id);
  const upstreamName = (id: string) => ups.find((u) => u.id === id)?.name ?? id;
  const machineName = (id: string | undefined) => (id ? (g.machines.find((m) => m.id === id)?.name ?? id) : "A client of your own");
  const checkOne = async (id?: string) => {
    if (!gw) return;
    const ids = id ? [id] : ups.map((u) => u.id);
    setChecks((c) => new Map([...c, ...ids.map((x) => [x, "busy"] as const)]));
    try {
      const r = await gw.test(id);
      setChecks((c) => new Map([...c, ...r.checks.map((x) => [x.upstream, x] as const)]));
    } catch (e) {
      setChecks((c) => new Map([...c, ...ids.map((x) => [x, { upstream: x, ok: false, ms: 0, models: 0, error: message(e) }] as const)]));
    }
  };
  const saveUpstream = async (at: number | "new", u: UpstreamSpec, key: string | null | undefined) => {
    const refused = await apply((s) => ({ ...s, upstreams: at === "new" ? [...(s.upstreams ?? []), u] : (s.upstreams ?? []).map((x, i) => (i === at ? u : x)) }));
    if (refused) return refused;
    if (key !== undefined && !u.key_ref) {
      try {
        await gw!.setKey(u.id, key);
      } catch (e) {
        return message(e);
      }
    }
    setEditing(null);
    return null;
  };
  const move = (i: number, by: -1 | 1) =>
    run(() =>
      apply((s) => {
        const list = [...(s.upstreams ?? [])];
        const [x] = list.splice(i, 1);
        list.splice(i + by, 0, x);
        return { ...s, upstreams: list };
      }),
    );
  const savePlan = async (patch: Partial<typeof plan>) => {
    if (!services?.gateway) return;
    setProblem(null);
    try {
      g.setPlan(await services.gateway.save(owner, { ...plan, ...patch }));
    } catch (e) {
      setProblem(message(e));
    }
  };
  const addresses: Address[] = (plan.urls ?? []).map((u) => {
    const at = u.url.replace(/^https?:\/\//, "").replace(/:\d+$/, "").replace(/^\[|\]$/g, "");
    return { ...u, ...(isIPv4(at) || isIPv6(at) ? { listen: at } : {}) };
  });
  const setAddresses = async (next: Address[]) => {
    const refused = await apply((s) => {
      const keep = (s.listen ?? []).filter((a) => !addresses.some((x) => x.listen === a));
      return { ...s, listen: [...new Set([...keep, ...next.map((a) => a.listen).filter((x): x is string => !!x)])] };
    });
    if (refused) return setProblem(refused);
    await savePlan({ urls: next.map(({ label, url }) => ({ label, url })) });
  };
  const removeAll = async () => {
    if (!services?.gateway) return;
    setRemoving(true);
    setProblem(null);
    try {
      for (const m of plan.members ?? []) await services.gateway.leave(owner, m.machine);
      if (gw && live) await gw.remove();
      await services.gateway.remove(owner);
      g.setPlan(null);
    } catch (e) {
      setProblem(message(e));
      await g.reload();
    } finally {
      setRemoving(false);
    }
  };

  const offline = !host ? "The gateway's computer was removed from roll." : !reachable(host) ? `${host.name} is offline: the machines that use it can't reach their models${plan.fallback ? " and use their own logins or keys" : ""} until it's back.` : null;
  const port = settings?.port ?? GATEWAY_PORT;

  return (
    <div ref={box} className="divide-y divide-line">
      <SettingRow
        s={{ ...GATEWAY_SETTINGS.gateway, title: "Gateway computer", description: undefined }}
        note={host ? <MachineIcon machine={host} className="size-3" /> : undefined}
        control={
          <>
            <button type="button" disabled={!live || [...checks.values()].includes("busy")} onClick={() => void checkOne()} className={smallBtn} title="Ask each upstream for its models now (never generates)">
              <RefreshCw className={cn("size-3.5", [...checks.values()].includes("busy") && "animate-spin motion-reduce:animate-none")} /> Check upstreams
            </button>
            <label className="flex items-center gap-2 text-[12px] text-muted">
              {settings?.enabled ? "Serving" : "Off"}
              <Switch checked={!!settings?.enabled} disabled={!live} onCheckedChange={(on) => void run(() => apply((s) => ({ ...s, enabled: on })))} aria-label="The gateway serves" />
            </label>
          </>
        }
      >
        <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-[12.5px]">
          <button type="button" onClick={() => host && setAgentsView({ kind: "settings", target: `m:${host.id}` })} className="inline-flex items-center gap-1 font-medium hover:underline">
            {host?.name ?? plan.host} <ChevronRight className="size-3 text-subtle" />
          </button>
          <RunState run={info?.run} />
          {info?.run.state !== "stopped" &&
            (info?.run.addresses ?? []).map((a) => (
              <span key={a} className="font-mono text-[11px] text-subtle">
                {a}
              </span>
            ))}
        </div>
        {info?.run.state === "failed" && <p className="mt-1 text-[12px] text-danger">{info.run.error}</p>}
        {offline && <p className="mt-2 rounded-lg border border-line bg-panel-2 px-2.5 py-1.5 text-[12px] text-subtle">{offline}</p>}
        {host && reachable(host) && !info && (
          <p className="mt-2 flex flex-wrap items-center gap-2 text-[12px] text-subtle">
            The gateway isn't set up on {host.name} any more.
            <button type="button" onClick={onSetup} className={smallBtn}>
              Set up again…
            </button>
          </p>
        )}
      </SettingRow>
      {problem && (
        <div className="px-3.5 py-2">
          <Problem>{problem}</Problem>
        </div>
      )}
      <SettingRow
        s={GATEWAY_SETTINGS.upstreams}
        control={
          <button type="button" disabled={!live || editing?.at === "new"} onClick={() => setEditing({ at: "new" })} className={smallBtn}>
            <Plus className="size-3.5" /> Add
          </button>
        }
      />
      {editing?.at === "new" && <UpstreamEditor taken={ups.map((u) => u.id)} onSave={(r) => saveUpstream("new", r.upstream, r.key)} onCancel={() => setEditing(null)} />}
      {ups.map((u, i) =>
        editing?.at === i ? (
          <UpstreamEditor key={u.id} initial={u} keyState={keyOf(u.id)} replaceKey={editing.key} taken={ups.map((x) => x.id)} onSave={(r) => saveUpstream(i, r.upstream, r.key)} onCancel={() => setEditing(null)} />
        ) : (
          <UpstreamItem
            key={u.id}
            upstream={u}
            keyState={keyOf(u.id)}
            health={health(u.id)}
            check={checks.get(u.id)}
            first={i === 0}
            last={i === ups.length - 1}
            locked={!live}
            onMove={(by) => void move(i, by)}
            onToggle={(on) => void run(() => apply((s) => ({ ...s, upstreams: (s.upstreams ?? []).map((x) => (x.id === u.id ? { ...x, disabled: on ? undefined : true } : x)) })))}
            onEdit={(key) => setEditing({ at: i, key })}
            onKey={async () => {
              await gw!.setKey(u.id, null);
            }}
            onCheck={() => void checkOne(u.id)}
            onRemove={async () => {
              const refused = await apply((s) => ({
                ...s,
                upstreams: (s.upstreams ?? []).filter((x) => x.id !== u.id),
                routes: (s.routes ?? []).map((r) => ({ ...r, targets: r.targets.filter((t) => t.upstream !== u.id) })).filter((r) => r.targets.length),
              }));
              if (refused) throw new Error(refused);
            }}
          />
        ),
      )}
      <SettingRow s={GATEWAY_SETTINGS.routes} />
      <RoutesList routes={settings?.routes ?? []} upstreams={ups} locked={!live} onSave={(routes) => apply((s) => ({ ...s, routes }))} />
      <SettingRow s={GATEWAY_SETTINGS.members}>
        <div className="mt-2.5">
          <MembersList plan={plan} host={host} port={port} machines={g.machines} seen={status?.clients_seen ?? []} onChange={g.reload} />
        </div>
      </SettingRow>
      <SettingRow s={GATEWAY_SETTINGS.usage}>
        <div className="mt-2.5">{gw && live ? <UsageCharts gw={gw} upstreamName={upstreamName} /> : <p className="text-[12.5px] text-subtle">Usage shows while {host?.name ?? "the gateway's computer"} is online.</p>}</div>
      </SettingRow>
      <SettingRow s={GATEWAY_SETTINGS.log}>
        <div className="mt-2.5">{gw && live ? <RequestLog gw={gw} logBodies={!!settings?.log_bodies} upstreamName={upstreamName} machineName={machineName} /> : <p className="text-[12.5px] text-subtle">The log shows while {host?.name ?? "the gateway's computer"} is online.</p>}</div>
      </SettingRow>
      <SettingRow s={GATEWAY_SETTINGS.options}>
        <div className="mt-3 space-y-4">
          <div className="flex flex-wrap items-center gap-3 text-[12.5px]">
            <span className="text-muted">Port</span>
            <TextField
              label="Gateway port"
              mono
              className="w-24"
              value={String(port)}
              disabled={!live}
              onSave={(v) => void run(() => apply((s) => ({ ...s, port: Number(v) || GATEWAY_PORT })))}
            />
            <span className="text-[11.5px] text-subtle">Machines' URLs keep the port they joined with: change it before they join.</span>
          </div>
          <Addresses addresses={addresses} onChange={(a) => void setAddresses(a)} hostName={host?.name ?? "the gateway's computer"} port={port} />
          <div className="grid gap-2.5 text-[12.5px]">
            <Toggle label="Keep request and response bodies in the log (a day, 64 KiB each)" checked={!!settings?.log_bodies} disabled={!live} onChange={(on) => void run(() => apply((s) => ({ ...s, log_bodies: on })))} />
            <Toggle label="Keep a session on the upstream it started on (prompt cache hits)" checked={settings?.affinity !== false} disabled={!live} onChange={(on) => void run(() => apply((s) => ({ ...s, affinity: on })))} />
            <Toggle label="New sandboxes use it" checked={!!plan.new_sandboxes} onChange={(on) => void savePlan({ new_sandboxes: on || undefined })} />
            <Toggle label={`When ${host?.name ?? "it"} is offline, agents use their own logins or keys`} checked={!!plan.fallback} onChange={(on) => void savePlan({ fallback: on || undefined })} />
            <div className="flex flex-wrap items-center gap-3">
              <span className="text-muted">Keep usage for</span>
              <Select
                label="Keep usage for"
                className="w-36"
                disabled={!live}
                value={String(settings?.retention_days ?? GATEWAY_RETENTION)}
                onChange={(v) => void run(() => apply((s) => ({ ...s, retention_days: Number(v) })))}
                options={[...new Set([...RETENTION, settings?.retention_days ?? GATEWAY_RETENTION])].sort((a, b) => a - b).map((d) => ({ value: String(d), label: d % 365 === 0 ? `${d / 365} year${d === 365 ? "" : "s"}` : `${d} days` }))}
              />
            </div>
          </div>
          {plan.fallback && (plan.members ?? []).length > 0 && (
            <p className="text-[11.5px] text-subtle">Fallback applies when a machine's agent starts; a running session keeps what it started with.</p>
          )}
          <div className="flex flex-wrap items-center gap-2 border-t border-line pt-3">
            {asking ? (
              <>
                <span className="min-w-0 flex-1 text-[12px] text-muted">
                  Remove the gateway? {listNames((plan.members ?? []).map((m) => machineName(m.machine))) || "No machine"} stop{(plan.members ?? []).length === 1 ? "s" : ""} using it, their keys are revoked, and {host?.name ?? "its computer"} forgets its upstreams' keys, usage and log.
                </span>
                <button type="button" disabled={removing} onClick={() => void removeAll()} className={dangerBtn}>
                  {removing ? <span className="shimmer">Removing…</span> : "Remove the gateway"}
                </button>
                <button type="button" onClick={() => setAsking(false)} className={iconBtn} aria-label="Keep it">
                  <X className="size-3.5" />
                </button>
              </>
            ) : (
              <button type="button" onClick={() => setAsking(true)} className="h-7 rounded-md border border-line-2 px-2.5 text-[12px] text-muted hover:text-danger">
                Remove the gateway…
              </button>
            )}
          </div>
        </div>
      </SettingRow>
    </div>
  );
}

function Toggle({ label, checked, disabled, onChange }: { label: string; checked: boolean; disabled?: boolean; onChange: (on: boolean) => void }) {
  return (
    <label className={cn("flex items-center gap-2.5", disabled && "opacity-60")}>
      <Switch size="sm" checked={checked} disabled={disabled} onCheckedChange={onChange} aria-label={label} />
      <span>{label}</span>
    </label>
  );
}

