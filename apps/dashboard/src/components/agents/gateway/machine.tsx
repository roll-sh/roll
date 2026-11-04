/**
 * The gateway where a machine's own settings show it (docs/code/gateway.md):
 *
 * - `GatewayMachineCard`: on the gateway's computer, a card with its run state, upstreams and the
 *   machines using it, and the way to the Gateway section; on another computer of the owner with
 *   no gateway yet, "Set up the gateway on this computer…".
 * - `GatewayEndpointItem`: a member's `roll-gateway` endpoint in its Endpoints, read-only (the
 *   relay manages it; the generic editor never edits it), with THE NOTICE.
 * - `ProviderGatewayNote`: under a provider that goes through it, "via roll gateway on <host>"
 *   and the notice.
 */

import { Check, ChevronRight, KeyRound, Network, RefreshCw } from "lucide-react";
import { useState } from "react";
import { useKept } from "../ui/dialog";
import { reachable, type EndpointCheck, type EndpointInfo, type Machine, type ProviderId, type Reply } from "@/lib/agents-api";
import { endpointOf } from "@/lib/agents-endpoints";
import { setAgentsView } from "@/lib/agents-surface";
import { Note, SettingRow, SettingsSection, type Setting } from "../settings-ui";
import { ago } from "../shared";
import { cn } from "../ui";
import { useAgents } from "../use-agents";
import { GatewayNotice } from "./notice";
import { hostOfEndpoint, iconBtn, message, RunState, smallBtn, textBtn, useGatewayPlan } from "./shared";
import { GatewaySetup, hostProblem } from "./wizard";

export const GATEWAY_MACHINE_SETTING: Setting = {
  id: "machine.gateway",
  title: "Model gateway",
  section: "Gateway",
  description: "roll's model gateway: your machines' model requests go out from this computer to your own API keys.",
  terms: ["gateway", "proxy", "upstream", "api key", "usage", "cost", "failover"],
};

/** Open the owner's settings on the Gateway section. */
export function openGatewaySettings() {
  setAgentsView({ kind: "settings", target: "owner" });
  setTimeout(() => document.querySelector('section[aria-label="Gateway"]')?.scrollIntoView?.({ block: "start", behavior: "smooth" }), 250);
}

function ManageLink() {
  return (
    <button type="button" onClick={openGatewaySettings} className="flex h-7 shrink-0 items-center gap-1 rounded-md px-2 text-[12px] text-muted hover:bg-fg/[0.06] hover:text-fg">
      Manage in Gateway settings <ChevronRight className="size-3.5" />
    </button>
  );
}

export function GatewayMachineCard({ machine }: { machine: Machine }) {
  const g = useGatewayPlan();
  const [setup, setSetup] = useState(false);
  const wizard = useKept(setup);
  const info = machine.info?.settings?.gateway;
  const hosts = g.plan?.host === machine.id || !!info;
  if (g.unavailable || g.plan === undefined) return null;
  // Another computer runs it: this machine's part is its endpoint (Endpoints, below).
  if (!hosts && g.plan) return null;
  if (!hosts && (machine.sandbox || hostProblem(machine))) return null;
  const members = (g.plan?.members ?? []).filter((m) => m.machine !== machine.id);
  const ups = info?.settings.upstreams ?? [];
  return (
    <SettingsSection title="Gateway" ids={[GATEWAY_MACHINE_SETTING.id]}>
      {hosts ? (
        <SettingRow s={{ ...GATEWAY_MACHINE_SETTING, description: undefined }} note={<Network className="size-3 text-subtle" aria-hidden />} control={<ManageLink />}>
          <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12.5px]">
            <RunState run={info?.run} />
            <span className="text-muted">{ups.length === 1 ? "1 upstream" : `${ups.length} upstreams`}</span>
            <span className="text-muted">{members.length === 1 ? "1 other machine uses it" : `${members.length} other machines use it`}</span>
          </div>
          {info?.run.state === "running" && <p className="mt-1 font-mono text-[11px] text-subtle">{info.run.addresses.join(" · ")}</p>}
          {info?.run.state === "failed" && <p className="mt-1 text-[12px] text-danger">{info.run.error}</p>}
          <GatewayNotice className="mt-2.5" host={machine.name} who={{ kind: "machines", names: members.map((m) => g.machines.find((x) => x.id === m.machine)?.name ?? m.machine) }} fallback={!!g.plan?.fallback} self={!members.length} />
        </SettingRow>
      ) : (
        <SettingRow
          s={{ ...GATEWAY_MACHINE_SETTING, title: "Set up the gateway on this computer", description: "Your other machines send their model requests through it, to your own API keys. Their requests then go out from here." }}
          control={
            <button type="button" disabled={!reachable(machine)} onClick={() => setSetup(true)} className={smallBtn}>
              Set up…
            </button>
          }
        />
      )}
      {wizard.value && <GatewaySetup key={wizard.key} open={wizard.open} onExited={wizard.onExited} machines={g.machines} initialHost={machine.id} onClose={() => (setSetup(false), void g.reload())} onDone={(p) => g.setPlan(p)} />}
    </SettingsSection>
  );
}

/** A member's managed endpoint: read-only, with the notice and the way to manage it. */
export function GatewayEndpointItem({ machine, endpoint: e, providerName }: { machine: Machine; endpoint: EndpointInfo; providerName: (p: ProviderId) => string }) {
  const { client } = useAgents();
  const [check, setCheck] = useState<EndpointCheck | "busy" | null>(null);
  const host = hostOfEndpoint(e);
  const self = host === machine.name;
  const runCheck = async () => {
    setCheck("busy");
    try {
      setCheck(await client.run<Extract<Reply, { type: "endpoint_checked" }>>(machine.id, { type: "endpoint.check", endpoint: e.id }, "endpoint_checked"));
    } catch (err) {
      setCheck({ ok: false, models: 0, ms: 0, error: message(err) });
    }
  };
  const users = (e.providers ?? []).map(providerName);
  return (
    <div className="px-3.5 py-3 text-[13px]" role="group" aria-label={`Endpoint ${e.name}`}>
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 flex-wrap items-center gap-1.5">
            <h4 className="truncate font-medium tracking-[-0.01em]">{e.name}</h4>
            <Note title="Set by roll when this machine joined the gateway: change it in Gateway settings">managed</Note>
            {e.fallback && <Note title="When the gateway doesn't answer, its agents start on their own logins or keys">fallback</Note>}
          </div>
          <p className="truncate font-mono text-[11.5px] text-muted" title={e.base_url}>
            {e.base_url}
          </p>
          <p className="mt-0.5 text-[12px] text-subtle">{[users.length ? users.join(", ") : "Every provider that can", `${(e.models ?? []).length} models`, e.checked_at && `checked ${ago(e.checked_at)}`].filter(Boolean).join(" · ")}</p>
        </div>
        <button type="button" disabled={!reachable(machine) || check === "busy"} onClick={() => void runCheck()} className={iconBtn} aria-label={`Check ${e.name}`} title="Check: ask it for its models now">
          <RefreshCw className={cn("size-3.5", check === "busy" && "animate-spin motion-reduce:animate-none")} />
        </button>
      </div>
      <p className="mt-2 flex items-center gap-2 text-[12px] text-muted">
        <KeyRound className="size-3.5 shrink-0 text-subtle" aria-hidden />
        A key of its own (never shown): stopping it revokes the key.
      </p>
      {check && check !== "busy" && (
        <p className={cn("mt-1.5 flex items-center gap-1.5 text-[12px]", check.ok ? "text-muted" : "text-danger")} role="status">
          {check.ok && <Check className="size-3.5 text-ok" aria-hidden />}
          {check.ok ? `${check.models} models · ${check.ms} ms` : (check.error ?? "The check failed")}
        </p>
      )}
      {!check && e.error && <p className="mt-1.5 text-[12px] text-danger">{e.error}</p>}
      <GatewayNotice className="mt-2.5" host={host} who={{ kind: "this" }} fallback={!!e.fallback} self={self} />
      <div className="mt-1.5 flex justify-end">
        <ManageLink />
      </div>
    </div>
  );
}

/**
 * The new-sandbox dialog: new sandboxes join the gateway as they first connect (`new_sandboxes`).
 * Sandboxes roll hosts elsewhere (`backend` isn't `docker`) can't reach it, and never join.
 */
export function NewSandboxGatewayNotice({ backend }: { backend: string }) {
  const g = useGatewayPlan();
  if (!g.plan?.new_sandboxes) return null;
  if (backend !== "docker")
    return <p className="text-[11.5px] text-subtle">Sandboxes roll hosts can&apos;t reach your gateway on {g.host?.name ?? "your computer"}: this one uses its own keys or login.</p>;
  return (
    <GatewayNotice host={g.host?.name ?? "your gateway's computer"} who={{ kind: "sandboxes" }} fallback={!!g.plan.fallback}>
      <p className="text-[11.5px] text-subtle">It joins as it first connects, with a key of its own. Change this in Gateway settings.</p>
    </GatewayNotice>
  );
}

/**
 * Under a provider whose requests go through the gateway: one line. The notice itself is said once
 * on the page, on the gateway's endpoint (`GatewayEndpointItem`), not again under every provider.
 */
export function ProviderGatewayNote({ machine, provider }: { machine: Machine; provider: ProviderId }) {
  const e = endpointOf(machine.info, provider);
  if (!e || e.kind !== "gateway") return null;
  const host = hostOfEndpoint(e);
  return (
    <p className="flex flex-wrap items-center gap-x-2 text-[11.5px] text-subtle">
      <span>{host === machine.name ? "Through the gateway on this computer" : `Through the gateway: its requests go out from ${host}`}</span>
      <button type="button" onClick={openGatewaySettings} className={cn(textBtn, "text-[11.5px]")}>
        Manage in Gateway settings
      </button>
    </p>
  );
}
