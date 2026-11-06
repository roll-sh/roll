"use client";

/**
 * Setting up the gateway (docs/code/gateway.md): five steps in one dialog.
 *
 * 1. **Computer**: which of the owner's computers runs it (online, a roll that takes sealed keys).
 * 2. **The notice**: every AI request from the machines that use it goes out from that computer;
 *    the owner ticks that they understand (`GatewayPlan.acknowledged_at`).
 * 3. **Upstreams**: kind presets; each key is sealed to the chosen computer in this browser the
 *    moment it's added (`roll-gateway-upstream:<id>`), so only the box is kept until it's sent.
 * 4. **Machines**: which use it and by which URL (loopback on the computer itself, else an address
 *    the owner adds, labelled Tailscale, LAN or Docker sandboxes), new sandboxes, fallback.
 * 5. **Set up and test**: `gateway.configure`, each `gateway.key`, the plan, `gateway.test` (each
 *    upstream's check, which never generates), then each machine joins through the relay.
 *
 * The shared dialog (`ui/dialog.tsx`): focus is taken and kept inside, and given back after. It
 * closes on Escape, the close button or a press on the scrim, never on a blur (Safari), and not
 * while it sets up.
 */

import { Check, ChevronLeft, Plus, Server, Trash2, X } from "lucide-react";
import { useMemo, useState } from "react";
import { GATEWAY_PORT, reachable, type GatewayPlan, type GatewaySettings, type GatewayUrl, type KeyChange, type Machine, type UpstreamCheck, type UpstreamSpec } from "@/lib/agents-api";
import { checkSettings, checkUrl, gatewayOn, isIPv4, isIPv6, listenError, reachProblem, upstreamKeyChange, upstreamUrl, URL_LABELS, urlChoices, urlFor } from "@/lib/agents-gateway";
import { machineKind } from "@/lib/agents-identity";
import { MachineIcon } from "../identity";
import { field, Note } from "../settings-ui";
import { cn, Select, Switch } from "../ui";
import { Dialog } from "../ui/dialog";
import { useAgents } from "../use-agents";
import { GatewayNotice } from "./notice";
import { iconBtn, KindTile, Labeled, message, primaryBtn, Problem, smallBtn } from "./shared";
import { UpstreamEditor } from "./upstreams";

const STEPS = ["Computer", "Notice", "Upstreams", "Machines", "Set up"] as const;

/** Why a machine can't run the gateway, or `null`. */
export function hostProblem(m: Machine): string | null {
  if (m.sandbox) return "A sandbox can't run it: pick one of your computers.";
  if (!m.info) return "Not connected yet.";
  if (!m.info.seal_key || !m.info.settings) return "Update roll on it first.";
  if (!reachable(m)) return "Offline.";
  return null;
}

interface Address extends GatewayUrl {
  /** The IP the gateway listens on for it (the address itself when it's one). */
  listen?: string;
}

type Phase = { what: string; state: "busy" | "ok" | "failed"; detail?: string };

export function GatewaySetup({
  machines,
  initialHost,
  onClose,
  onDone,
}: {
  machines: Machine[];
  /** "Set up the gateway on this computer" from a machine's settings. */
  initialHost?: string;
  onClose: () => void;
  onDone: (plan: GatewayPlan) => void;
}) {
  const { client, services, owner } = useAgents();
  const [step, setStep] = useState(0);
  const computers = machines.filter((m) => !m.sandbox);
  // An always-on computer first (a server, a desktop), then any that can host it: a laptop sleeps.
  const [hostId, setHostId] = useState<string | null>(() => {
    const able = computers.filter((m) => !hostProblem(m));
    const rank = (m: Machine) => {
      const i = ["server", "cloud", "desktop"].indexOf(machineKind(m));
      return i < 0 ? 3 : i;
    };
    return able.find((m) => m.id === initialHost)?.id ?? [...able].sort((a, b) => rank(a) - rank(b))[0]?.id ?? null;
  });
  const host = machines.find((m) => m.id === hostId);
  const [ack, setAck] = useState(false);
  const [upstreams, setUpstreams] = useState<UpstreamSpec[]>([]);
  /** Keys already sealed to `host`: the boxes, never the keys. */
  const [keys, setKeys] = useState<Map<string, KeyChange>>(new Map());
  const [editing, setEditing] = useState<number | "new" | null>(null);
  const [addresses, setAddresses] = useState<Address[]>([]);
  const [members, setMembers] = useState<Map<string, { on: boolean; url?: string }>>(new Map());
  const [newSandboxes, setNewSandboxes] = useState(false);
  const [fallback, setFallback] = useState(false);
  const [phases, setPhases] = useState<Phase[]>([]);
  const [checks, setChecks] = useState<UpstreamCheck[] | null>(null);
  const [running, setRunning] = useState<"idle" | "busy" | "done">("idle");
  const [problem, setProblem] = useState<string | null>(null);

  const plan: Pick<GatewayPlan, "host" | "urls"> = { host: hostId ?? "", urls: addresses.map(({ label, url }) => ({ label, url })) };
  const isMember = (m: Machine) => members.get(m.id)?.on ?? m.id === hostId;
  const chosen = machines.filter(isMember);
  const others = chosen.filter((m) => m.id !== hostId).map((m) => m.name);
  const hostName = host?.name ?? "the computer you pick";

  const pickHost = (id: string) => {
    if (id === hostId) return;
    setHostId(id);
    // Boxes sealed to another computer don't open on this one: those keys are typed again.
    setKeys(new Map());
    setMembers(new Map());
  };

  const settings = (): GatewaySettings => ({
    enabled: true,
    port: GATEWAY_PORT,
    ...(addresses.some((a) => a.listen) ? { listen: [...new Set(addresses.map((a) => a.listen).filter((x): x is string => !!x))] } : {}),
    ...(upstreams.length ? { upstreams } : {}),
  });

  const canNext = [!!host && !hostProblem(host), ack, upstreams.length > 0 && editing === null, true, false][step];

  const run = async () => {
    if (!host || !services?.gateway) return;
    const checked = checkSettings(settings());
    if ("error" in checked) return setProblem(checked.error);
    setProblem(null);
    setRunning("busy");
    setPhases([]);
    setChecks(null);
    const gw = gatewayOn(client, host);
    const step = async (what: string, fn: () => Promise<string | void>): Promise<boolean> => {
      setPhases((ps) => [...ps, { what, state: "busy" }]);
      const done = (p: Omit<Phase, "what">) => setPhases((ps) => ps.map((x) => (x.what === what ? { what, ...p } : x)));
      try {
        const detail = await fn();
        done({ state: "ok", ...(detail ? { detail } : {}) });
        return true;
      } catch (e) {
        done({ state: "failed", detail: message(e) });
        return false;
      }
    };
    const ok =
      (await step(`Start the gateway on ${host.name}`, () => gw.configure(checked.settings).then(() => {}))) &&
      (await (async () => {
        for (const [upstream, key] of keys) {
          const name = upstreams.find((u) => u.id === upstream)?.name ?? upstream;
          if (!(await step(`Send ${name}'s key, sealed to ${host.name}`, () => client.run(host.id, { type: "gateway.key", upstream, key }, "ok").then(() => {})))) return false;
        }
        return true;
      })()) &&
      (await step("Save the plan", async () => {
        const saved = await services.gateway!.save(owner, { host: host.id, urls: plan.urls, new_sandboxes: newSandboxes || undefined, fallback: fallback || undefined, acknowledged_at: Date.now(), updated_at: 0 });
        onDone(saved);
      }));
    // The keys are sent (or not): their boxes go too.
    setKeys(new Map());
    if (!ok) return setRunning("idle");
    await step("Check each upstream", async () => {
      const r = await gw.test();
      setChecks(r.checks);
      const bad = r.checks.filter((c) => !c.ok).length;
      return bad ? `${bad} of ${r.checks.length} didn't answer: see below` : `${r.checks.length} answered`;
    });
    for (const m of chosen) {
      const url = members.get(m.id)?.url ?? urlChoices(plan, GATEWAY_PORT, m.id)[0]?.url;
      await step(`${m.name} uses it`, async () => {
        await services.gateway!.join(owner, { machine: m.id, ...(url ? { url } : {}) });
        const after = await services.gateway!.get(owner);
        const member = after?.members?.find((x) => x.machine === m.id);
        if (after) onDone(after);
        if (member?.error) throw new Error(member.error);
      });
    }
    setRunning("done");
  };

  // The shared dialog (M5): portaled, focus taken and kept inside, given back after; Escape, a press
  // outside and × wait while it sets up. Escape with an upstream open in its editor closes the
  // editor first.
  const dismiss = () => {
    if (editing !== null) return setEditing(null);
    onClose();
  };
  return (
    <Dialog
      open
      onClose={dismiss}
      busy={running === "busy"}
      className="sm:w-[min(600px,100%)]"
      title="Set up the model gateway"
      lead="One of your computers sends your machines' model requests on to your own API keys, with usage and cost in one place."
      footer={
        <>
          {step > 0 && running === "idle" && (
            <button type="button" onClick={() => setStep((s) => s - 1)} className="flex h-8 items-center gap-1 rounded-lg px-2 text-[12.5px] text-muted hover:text-fg">
              <ChevronLeft className="size-3.5" /> Back
            </button>
          )}
          <span className="ml-auto" />
          {step < 4 ? (
            <button type="button" disabled={!canNext} onClick={() => setStep((s) => s + 1)} className={primaryBtn}>
              {step === 3 ? "Review" : "Continue"}
            </button>
          ) : running === "done" ? (
            <button type="button" onClick={onClose} className={primaryBtn}>
              Done
            </button>
          ) : (
            <button type="button" disabled={running === "busy" || !services?.gateway} onClick={() => void run()} className={primaryBtn}>
              {running === "busy" ? <span className="shimmer">Setting up…</span> : phases.length ? "Try again" : "Set up and test"}
            </button>
          )}
        </>
      }
    >
        <ol className="sticky top-0 z-10 -mx-5 flex gap-1 bg-sheet px-5 pb-3" aria-label="Steps">
          {STEPS.map((s, i) => (
            <li key={s} aria-current={i === step ? "step" : undefined} className="min-w-0 flex-1">
              <div className={cn("h-0.5 rounded-full transition-colors duration-200", i <= step ? "bg-fg/70" : "bg-line-2")} />
              <p className={cn("mt-1.5 truncate text-[11px]", i === step ? "font-[510] text-fg" : "text-subtle")}>{s}</p>
            </li>
          ))}
        </ol>
        <div className="space-y-3 pt-1" key={step}>
          {step === 0 && (
            <Labeled label="Which computer runs it?" hint="An always-on computer is best: when it's offline, the machines that use it can't reach their models.">
              <ul className="divide-y divide-line overflow-hidden rounded-xl border border-line-2 bg-panel inset-shadow-highlight" role="radiogroup" aria-label="Computers">
                {computers.length === 0 && <li className="px-3 py-2.5 text-[12.5px] text-subtle">Connect a computer with roll agent up first.</li>}
                {computers.map((m) => {
                  const why = hostProblem(m);
                  return (
                    <li key={m.id}>
                      <label className={cn("flex min-w-0 items-center gap-2.5 px-3 py-2.5 text-[13px]", why ? "cursor-not-allowed" : "cursor-pointer hover:bg-fg/[0.03]")}>
                        <input type="radio" name="gateway-host" disabled={!!why} checked={hostId === m.id} onChange={() => pickHost(m.id)} className="accent-[var(--accent)]" aria-label={m.name} />
                        <MachineIcon machine={m} tile className="size-5" />
                        <span className={cn("min-w-0 truncate", why && "text-subtle")}>{m.name}</span>
                        <span className="ml-auto min-w-0 truncate text-right text-[11.5px] text-subtle">{why ?? `${m.info?.os ?? ""} · ${m.info?.hostname ?? ""}`}</span>
                      </label>
                    </li>
                  );
                })}
              </ul>
            </Labeled>
          )}
          {step === 1 && host && (
            <>
              <GatewayNotice host={host.name} who={{ kind: "machines", names: [] }} fallback={fallback} acknowledged={ack} onAcknowledge={setAck}>
                <p className="text-[12px] text-subtle">You pick the machines, and whether they fall back to their own logins, in step 4.</p>
              </GatewayNotice>
              <ul className="space-y-1 text-[12px] text-subtle">
                <li>Upstreams take API keys and cloud credentials only. Claude.ai and ChatGPT logins stay on each machine and never go through the gateway.</li>
                <li>Requests go out as roll gateway, never dressed as another app. A provider that asks to slow down is given time; the gateway doesn't try another key of it.</li>
              </ul>
            </>
          )}
          {step === 2 && host && (
            <Labeled label="Upstreams, in priority order" hint="Requests go to the first one that serves the model; the next takes over when one fails. Routes and aliases come after setup.">
              <div className="overflow-hidden rounded-xl border border-line-2 bg-panel inset-shadow-highlight">
                {upstreams.length === 0 && editing !== "new" && <p className="px-3.5 py-3 text-[12.5px] text-subtle">No upstreams yet.</p>}
                <ul className="divide-y divide-line">
                  {upstreams.map((u, i) =>
                    editing === i ? (
                      <li key={u.id}>
                        <UpstreamEditor initial={u} keyState={keys.has(u.id) ? "sealed" : u.key_ref ? "reference" : "none"} taken={upstreams.map((x) => x.id)} onSave={(r) => saveUpstream(i, r.upstream, r.key)} onCancel={() => setEditing(null)} />
                      </li>
                    ) : (
                      <li key={u.id} className="flex items-center gap-3 px-3.5 py-2.5 text-[13px]" aria-label={`Upstream ${u.name}`}>
                        <KindTile kind={u.kind} />
                        <div className="min-w-0 flex-1">
                          <p className="truncate font-medium">{u.name}</p>
                          <p className="truncate font-mono text-[11.5px] text-subtle">{upstreamUrl(u)}</p>
                        </div>
                        <Note>{u.key_ref ? "1Password" : keys.has(u.id) ? "key sealed" : "no key"}</Note>
                        <button type="button" onClick={() => setEditing(i)} className={smallBtn}>
                          Edit
                        </button>
                        <button type="button" onClick={() => removeUpstream(i)} className={cn(iconBtn, "hover:text-danger")} aria-label={`Remove ${u.name}`}>
                          <Trash2 className="size-3.5" />
                        </button>
                      </li>
                    ),
                  )}
                </ul>
                {editing === "new" ? (
                  <UpstreamEditor taken={upstreams.map((x) => x.id)} onSave={(r) => saveUpstream("new", r.upstream, r.key)} onCancel={() => setEditing(null)} />
                ) : (
                  <div className="border-t border-line px-3.5 py-2.5">
                    <button type="button" onClick={() => setEditing("new")} className={smallBtn}>
                      <Plus className="size-3.5" /> Add an upstream
                    </button>
                  </div>
                )}
              </div>
            </Labeled>
          )}
          {step === 3 && host && (
            <>
              <Addresses addresses={addresses} onChange={setAddresses} hostName={host.name} />
              <Labeled label="Machines that use it">
                <ul className="divide-y divide-line overflow-hidden rounded-xl border border-line-2 bg-panel inset-shadow-highlight" aria-label="Machines">
                  {machines.map((m) => {
                    const choices = urlChoices(plan, GATEWAY_PORT, m.id);
                    const why = reachProblem(m, plan);
                    const on = isMember(m);
                    const blocked = !choices.length;
                    return (
                      <li key={m.id} className="px-3 py-2 text-[13px]">
                        <div className="flex min-w-0 flex-wrap items-center gap-2.5">
                          <label className={cn("flex min-w-0 flex-1 items-center gap-2.5", blocked ? "cursor-not-allowed" : "cursor-pointer")}>
                            <input
                              type="checkbox"
                              disabled={blocked}
                              checked={on && !blocked}
                              onChange={(e) => setMembers((all) => new Map(all).set(m.id, { ...all.get(m.id), on: e.target.checked }))}
                              className="accent-[var(--accent)]"
                              aria-label={m.name}
                            />
                            <MachineIcon machine={m} tile className="size-5" />
                            <span className="min-w-0 truncate">{m.name}</span>
                            {m.id === hostId && <Note>runs it</Note>}
                          </label>
                          {choices.length > 0 && (
                            <Select
                              label={`${m.name} reaches it at`}
                              className="w-44"
                              disabled={!on}
                              value={members.get(m.id)?.url ?? choices[0].url}
                              onChange={(url) => setMembers((all) => new Map(all).set(m.id, { on, url }))}
                              options={choices.map((c) => ({ value: c.url, label: c.label, hint: c.url }))}
                            />
                          )}
                        </div>
                        {why && <p className="mt-1 pl-6 text-[11.5px] text-subtle">{why}</p>}
                      </li>
                    );
                  })}
                </ul>
              </Labeled>
              <div className="space-y-2 text-[12.5px]">
                <label className="flex items-center gap-2.5">
                  <Switch size="sm" checked={newSandboxes} onCheckedChange={setNewSandboxes} aria-label="New sandboxes use it" />
                  <span>New sandboxes use it too</span>
                </label>
                <label className="flex items-center gap-2.5">
                  <Switch size="sm" checked={fallback} onCheckedChange={setFallback} aria-label="Fall back to their own logins" />
                  <span>When {host.name} is offline, agents use their own logins or keys</span>
                </label>
              </div>
              {(others.length > 0 || newSandboxes) && <GatewayNotice host={host.name} who={others.length ? { kind: "machines", names: newSandboxes ? [...others, "new sandboxes"] : others } : { kind: "sandboxes" }} fallback={fallback} />}
            </>
          )}
          {step === 4 && host && (
            <>
              <p className="text-[12.5px] text-muted">
                {host.name} starts the gateway with {upstreams.length === 1 ? "1 upstream" : `${upstreams.length} upstreams`}; then {chosen.length === 1 ? "1 machine joins" : `${chosen.length} machines join`}, each with a key of its own that only it can open.
              </p>
              <ol className="space-y-1.5" aria-label="Progress">
                {phases.map((p) => (
                  <li key={p.what} className="flex items-start gap-2 text-[12.5px]">
                    <span className="mt-0.5 grid size-4 shrink-0 place-items-center">
                      {p.state === "busy" ? <span className="size-1.5 rounded-full bg-accent motion-safe:animate-pulse" /> : p.state === "ok" ? <Check className="size-3.5 text-ok" /> : <X className="size-3.5 text-danger" />}
                    </span>
                    <span className="min-w-0">
                      <span className={cn(p.state === "busy" && "shimmer")}>{p.what}</span>
                      {p.detail && <span className={cn("block text-[11.5px]", p.state === "failed" ? "text-danger" : "text-subtle")}>{p.detail}</span>}
                    </span>
                  </li>
                ))}
              </ol>
              {checks && (
                <ul className="divide-y divide-line overflow-hidden rounded-xl border border-line-2 bg-panel" aria-label="Upstream checks">
                  {checks.map((c) => {
                    const u = upstreams.find((x) => x.id === c.upstream);
                    return (
                      <li key={c.upstream} className="flex items-center gap-2.5 px-3 py-2 text-[12.5px]">
                        {u ? <KindTile kind={u.kind} /> : <Server className="size-4" />}
                        <span className="min-w-0 flex-1 truncate">{u?.name ?? c.upstream}</span>
                        <span className={cn("min-w-0 truncate text-right text-[12px]", c.ok ? "text-muted" : "text-danger")} role="status">
                          {c.ok ? `${c.models} models · ${c.ms} ms` : (c.error ?? "didn't answer")}
                        </span>
                      </li>
                    );
                  })}
                </ul>
              )}
            </>
          )}
          {problem && <Problem>{problem}</Problem>}
        </div>
    </Dialog>
  );

  async function saveUpstream(at: number | "new", u: UpstreamSpec, key: string | null | undefined): Promise<string | null> {
    try {
      // Sealed now, to the chosen computer: the plaintext isn't kept anywhere.
      const change = key === undefined ? undefined : await upstreamKeyChange(host?.info?.seal_key, u.id, key);
      setUpstreams((us) => (at === "new" ? [...us, u] : us.map((x, i) => (i === at ? u : x))));
      setKeys((ks) => {
        const next = new Map(ks);
        if (change?.type === "set") next.set(u.id, change);
        else if (change?.type === "clear" || u.key_ref) next.delete(u.id);
        return next;
      });
      setEditing(null);
      return null;
    } catch (e) {
      return message(e);
    }
  }

  function removeUpstream(at: number) {
    const id = upstreams[at]?.id;
    setUpstreams((us) => us.filter((_, i) => i !== at));
    setKeys((ks) => {
      const next = new Map(ks);
      if (id) next.delete(id);
      return next;
    });
  }
}

/**
 * The addresses machines reach the gateway at, labelled. An IP literal is listened on as it is; a
 * name (a tailnet name) needs the IP it stands for, which the gateway listens on.
 */
export function Addresses({ addresses, onChange, hostName, port = GATEWAY_PORT }: { addresses: Address[]; onChange: (a: Address[]) => void; hostName: string; port?: number }) {
  const [label, setLabel] = useState<string>(URL_LABELS[0]);
  const [address, setAddress] = useState("");
  const [ip, setIp] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const literal = isIPv4(address.trim()) || isIPv6(address.trim());
  const add = () => {
    const a = address.trim();
    if (!a) return;
    const url = urlFor(a, port);
    const bad = checkUrl(url, "the address");
    if (bad) return setProblem(bad);
    const listen = literal ? a : ip.trim();
    if (!listen) return setProblem(`Add the IP address ${a} stands for on ${hostName}: the gateway listens on addresses, not names.`);
    const why = listenError(listen);
    if (why) return setProblem(why);
    if (addresses.some((x) => x.url === url)) return setProblem("That address is there already.");
    setProblem(null);
    onChange([...addresses, { label, url, listen }]);
    setAddress("");
    setIp("");
  };
  const LABELS = useMemo(() => URL_LABELS.map((l) => ({ value: l as string, label: l })), []);
  return (
    <Labeled label={`How other machines reach ${hostName}`} hint={`${hostName} itself uses its loopback address. Others need an address of ${hostName} on a network they share: Tailscale, your LAN, or the Docker bridge for sandboxes on it. It's listened on, port ${port}, nowhere else.`}>
      {addresses.length > 0 && (
        <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line-2" aria-label="Addresses">
          {addresses.map((a) => (
            <li key={a.url} className="flex items-center gap-2 px-2.5 py-1.5 text-[12.5px]">
              <Note>{a.label}</Note>
              <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{a.url}</span>
              {a.listen && a.url !== urlFor(a.listen, port) && <span className="font-mono text-[11px] text-subtle">listens on {a.listen}</span>}
              <button type="button" onClick={() => onChange(addresses.filter((x) => x.url !== a.url))} className={iconBtn} aria-label={`Remove ${a.url}`}>
                <X className="size-3.5" />
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <Select label="Network" className="w-40" value={label} onChange={setLabel} options={LABELS} />
        <input aria-label="Address" value={address} onChange={(e) => setAddress(e.target.value)} onKeyDown={(e) => e.key === "Enter" && (e.preventDefault(), add())} placeholder={`${hostName.toLowerCase().replace(/[^a-z0-9-]+/g, "-")}.tailnet.ts.net or an IP`} spellCheck={false} className={cn(field, "min-w-48 flex-1 font-mono text-[12px]")} />
        {!literal && address.trim() && (
          <input aria-label="Its IP address" value={ip} onChange={(e) => setIp(e.target.value)} placeholder="its IP on that network" spellCheck={false} className={cn(field, "w-40 font-mono text-[12px]")} />
        )}
        <button type="button" disabled={!address.trim()} onClick={add} className={smallBtn}>
          <Plus className="size-3.5" /> Add
        </button>
      </div>
      {problem && <Problem>{problem}</Problem>}
    </Labeled>
  );
}

export type { Address };
