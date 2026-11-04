/**
 * The machines that use the gateway (docs/code/gateway.md, "Machines that use it"): the notice
 * over them, then each one's URL, providers, when it last sent a request and why joining failed,
 * with Leave (its endpoint goes, its key is revoked). Joining sends `POST …/gateway/members`: the
 * relay asks the gateway's computer for a key sealed to the machine and passes the box on.
 */

import { LogOut, Plus, X } from "lucide-react";
import { useState } from "react";
import { reachable, type ClientSeen, type GatewayPlan, type Machine } from "@/lib/agents-api";
import { hostedSandbox, reachProblem, urlChoices } from "@/lib/agents-gateway";
import { MachineIcon } from "../identity";
import { Note } from "../settings-ui";
import { ago } from "../shared";
import { cn, Select } from "../ui";
import { useAgents } from "../use-agents";
import { GatewayNotice } from "./notice";
import { dangerBtn, iconBtn, message, Problem, smallBtn } from "./shared";
import { useEscapeLayer } from "../escape-layers";

export function MembersList({
  plan,
  host,
  port,
  machines,
  seen,
  onChange,
}: {
  plan: GatewayPlan;
  host: Machine | undefined;
  port: number;
  machines: Machine[];
  seen: readonly ClientSeen[];
  /** After a join or a leave: load the plan again. */
  onChange: () => Promise<void>;
}) {
  const { services, owner } = useAgents();
  const api = services?.gateway;
  const members = plan.members ?? [];
  const byId = new Map(machines.map((m) => [m.id, m]));
  const others = members.filter((m) => m.machine !== plan.host).map((m) => byId.get(m.machine)?.name ?? m.machine);
  const candidates = machines.filter((m) => !members.some((x) => x.machine === m.id));
  const [pick, setPick] = useState<string | null>(null);
  const [url, setUrl] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [asking, setAsking] = useState<string | null>(null);
  useEscapeLayer(asking !== null, () => setAsking(null));
  const hostName = host?.name ?? "the gateway's computer";
  const picked = pick ? byId.get(pick) : undefined;
  const choices = picked ? urlChoices(plan, port, picked.id) : [];
  const act = async (what: string, fn: () => Promise<void>) => {
    if (!api) return;
    setBusy(what);
    setProblem(null);
    try {
      await fn();
    } catch (e) {
      setProblem(message(e));
    } finally {
      await onChange().catch(() => {});
      setBusy(null);
    }
  };
  const join = (machine: Machine, at: string | undefined) => act(`join:${machine.id}`, () => api!.join(owner, { machine: machine.id, ...(at ? { url: at } : {}) }));

  return (
    <div className="space-y-2.5">
      {others.length > 0 && <GatewayNotice host={hostName} who={{ kind: "machines", names: others }} fallback={!!plan.fallback} />}
      <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line-2" aria-label="Machines using the gateway">
        {members.length === 0 && <li className="px-3 py-2.5 text-[12.5px] text-subtle">No machine uses it yet.</li>}
        {members.map((m) => {
          const machine = byId.get(m.machine);
          const last = m.client ? seen.find((s) => s.client === m.client)?.at : undefined;
          const label = urlChoices(plan, port, m.machine).find((u) => u.url === m.url)?.label;
          return (
            <li key={m.machine} className="px-3 py-2.5 text-[12.5px]" aria-label={`Member ${machine?.name ?? m.machine}`}>
              <div className="flex items-center gap-2.5">
                {machine ? <MachineIcon machine={machine} tile className="size-5" /> : <span className="size-5" />}
                <div className="min-w-0 flex-1">
                  <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                    <span className="truncate font-medium">{machine?.name ?? m.machine}</span>
                    {m.machine === plan.host && <Note>runs it</Note>}
                    {machine?.sandbox && <Note>sandbox</Note>}
                  </div>
                  <p className="text-[11.5px] text-subtle [overflow-wrap:anywhere] sm:truncate">
                    {label ? `${label} · ` : ""}
                    <span className="font-mono">{m.url}</span>
                    {m.providers?.length ? ` · ${m.providers.join(", ")}` : ""}
                    {" · "}
                    {last ? `last request ${ago(last)}` : "no requests yet"}
                  </p>
                </div>
                {asking === m.machine ? null : (
                  <button type="button" disabled={busy !== null} onClick={() => setAsking(m.machine)} className={cn(iconBtn, "hover:text-danger")} aria-label={`${machine?.name ?? m.machine} stops using the gateway`} title="Stop using it">
                    <LogOut className="size-3.5" />
                  </button>
                )}
              </div>
              {m.error && (
                <p className="mt-1 flex flex-wrap items-center gap-2 pl-7.5 text-[12px] text-danger">
                  {m.error}
                  {machine && (
                    <button type="button" disabled={busy !== null || !reachable(machine)} onClick={() => void join(machine, m.url)} className="rounded px-1 text-[12px] text-muted hover:bg-fg/[0.06] hover:text-fg">
                      Try again
                    </button>
                  )}
                </p>
              )}
              {asking === m.machine && (
                <div className="mt-2 flex flex-wrap items-center gap-2 rounded-lg border border-line-2 bg-panel-2 px-2.5 py-2 text-[12px]">
                  <span className="min-w-0 flex-1 text-muted">
                    {machine?.name ?? m.machine} stops using the gateway: its endpoint goes, its key is revoked, and its agents go back to their own logins.
                  </span>
                  <button type="button" disabled={busy !== null} onClick={() => void act(`leave:${m.machine}`, () => api!.leave(owner, m.machine)).then(() => setAsking(null))} className={dangerBtn}>
                    {busy === `leave:${m.machine}` ? <span className="shimmer">Leaving…</span> : "Stop using it"}
                  </button>
                  <button type="button" onClick={() => setAsking(null)} className={iconBtn} aria-label="Keep it">
                    <X className="size-3.5" />
                  </button>
                </div>
              )}
            </li>
          );
        })}
      </ul>
      {candidates.length > 0 && (
        <div className="flex flex-wrap items-center gap-2">
          <Select
            label="A machine to add"
            placeholder="Add a machine…"
            className="w-48"
            value={pick}
            onChange={(v) => {
              setPick(v);
              setUrl(null);
            }}
            options={candidates.map((m) => ({ value: m.id, label: m.name, disabled: !reachable(m) || hostedSandbox(m), hint: hostedSandbox(m) ? "Can't reach it" : undefined }))}
          />
          {picked && choices.length > 0 && (
            <Select label="Reached at" className="w-48" value={url ?? choices[0].url} onChange={setUrl} options={choices.map((c) => ({ value: c.url, label: c.label, hint: c.url }))} />
          )}
          <button type="button" disabled={!picked || busy !== null || !choices.length || !!reachProblem(picked, plan)} onClick={() => picked && void join(picked, url ?? choices[0]?.url).then(() => setPick(null))} className={smallBtn}>
            <Plus className="size-3.5" /> {busy?.startsWith("join:") ? <span className="shimmer">Joining…</span> : "Use the gateway"}
          </button>
        </div>
      )}
      {picked && reachProblem(picked, plan) && <p className="text-[12px] text-subtle">{reachProblem(picked, plan)}</p>}
      {problem && <Problem>{problem}</Problem>}
    </div>
  );
}
