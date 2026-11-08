/**
 * Pieces the Gateway UI shares (docs/code/gateway.md): the owner's plan as a hook, the upstream
 * kinds' icons (lucide only: no provider art), the run state, buttons and fields in the agents
 * design system (docs/code/agents-design.md), and "while visible" for the polls.
 */

import { Boxes, Cloud, Hexagon, Route, Server, ServerCog, Sparkles, Waypoints, type LucideIcon } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import { GATEWAY_LIMITS, type EndpointInfo, type GatewayInfo, type GatewayPlan, type GatewayRun, type Machine, type UpstreamKind } from "@/lib/agents-api";
import { memberEndpointName } from "@/lib/agents-gateway";
import { cn } from "../ui";
import { useAgents, useAgentsSnapshot } from "../use-agents";

export const iconBtn = "grid size-7 shrink-0 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg disabled:opacity-40 disabled:hover:bg-transparent";
export const smallBtn = "flex h-7 shrink-0 items-center gap-1.5 rounded-md border border-line-2 bg-raised px-2.5 text-[12px] shadow-card transition-colors hover:bg-fg/[0.06] active:scale-[0.97] disabled:opacity-50 motion-reduce:active:scale-100";
export const textBtn = "rounded px-1 text-[12px] text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg disabled:opacity-40";
export const primaryBtn = "flex h-8 items-center gap-1.5 rounded-lg bg-fg px-3 text-[12.5px] font-medium text-bg shadow-card inset-shadow-highlight transition active:scale-[0.97] hover:opacity-90 disabled:opacity-50 motion-reduce:active:scale-100";
export const dangerBtn = "h-7 rounded-md bg-danger/10 px-2.5 text-[12px] font-medium text-danger hover:bg-danger/20 disabled:opacity-50";

export const KIND_ICON: Record<UpstreamKind, LucideIcon> = {
  anthropic: Sparkles,
  openai: Hexagon,
  azure_openai: Cloud,
  bedrock: Boxes,
  vertex: Waypoints,
  openrouter: Route,
  anthropic_compatible: Server,
  openai_compatible: ServerCog,
};

export function KindTile({ kind, className }: { kind: UpstreamKind; className?: string }) {
  const Icon = KIND_ICON[kind];
  return (
    <span className={cn("grid size-6 shrink-0 place-items-center rounded-md border border-line-2 bg-panel-2 text-muted inset-shadow-highlight", className)} aria-hidden>
      <Icon className="size-3.5" strokeWidth={1.75} />
    </span>
  );
}

export function Problem({ children }: { children: ReactNode }) {
  return (
    <p role="alert" className="text-[12px] text-danger">
      {children}
    </p>
  );
}

export function Labeled({ label, children, hint, className }: { label: string; children: ReactNode; hint?: ReactNode; className?: string }) {
  return (
    <div className={cn("space-y-1.5", className)}>
      <p className="text-[11.5px] font-[510] text-muted">{label}</p>
      {children}
      {hint && <p className="text-[11.5px] leading-snug text-subtle">{hint}</p>}
    </div>
  );
}

/** A dot and a word for the gateway's run state; the accent only while it serves. */
export function RunState({ run, className }: { run: GatewayRun | undefined; className?: string }) {
  const state = run?.state ?? "stopped";
  const label = state === "running" ? "Running" : state === "failed" ? "Can't start" : "Stopped";
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-[12px]", state === "failed" ? "text-danger" : "text-muted", className)} title={run?.state === "failed" ? run.error : undefined}>
      <span className={cn("size-1.5 rounded-full", state === "running" ? "bg-accent" : state === "failed" ? "bg-danger" : "bg-faint")} aria-hidden />
      {label}
    </span>
  );
}

export const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** The gateway computer's name a member's endpoint names ("roll gateway on <host>"). */
export function hostOfEndpoint(e: Pick<EndpointInfo, "name">): string {
  const prefix = memberEndpointName("");
  return e.name.startsWith(prefix) ? e.name.slice(prefix.length) : e.name;
}

/** A machine's managed gateway endpoint, if it uses the gateway. */
export const gatewayEndpoint = (m: Machine | undefined): EndpointInfo | undefined => m?.info?.settings?.endpoints?.find((e) => e.kind === "gateway" || e.id === GATEWAY_LIMITS.endpointId);

export interface GatewayState {
  /** `undefined` while loading; `null`: none set up. */
  plan: GatewayPlan | null | undefined;
  setPlan: (p: GatewayPlan | null) => void;
  reload: () => Promise<void>;
  error: string | null;
  /** The owner's machines, by name. */
  machines: Machine[];
  host: Machine | undefined;
  /** What the host reports (`MachineConfig.gateway`). */
  info: GatewayInfo | undefined;
  /** The services have no gateway (an old relay, a share). */
  unavailable: boolean;
}

/** The owner's plan from the relay, and the machines it names from the stream. */
export function useGatewayPlan(): GatewayState {
  const { client, services, owner } = useAgents();
  const data = useAgentsSnapshot(client)?.data ?? client.data;
  const api = services?.gateway;
  const [plan, setPlan] = useState<GatewayPlan | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(async () => {
    if (!api) return;
    try {
      setPlan(await api.get(owner));
      setError(null);
    } catch (e) {
      setError(message(e));
      setPlan((p) => (p === undefined ? null : p));
    }
  }, [api, owner]);
  useEffect(() => {
    void reload();
  }, [reload]);
  const machines = useMemo(() => [...data.machines.values()].map((r) => r.value).sort((a, b) => a.name.localeCompare(b.name)), [data.machines]);
  const host = plan ? data.machines.get(plan.host)?.value : undefined;
  return { plan, setPlan, reload, error, machines, host, info: host?.info?.settings?.gateway, unavailable: !api };
}

/** Whether `ref` is on screen and the page is in front (polls run only then). */
export function useVisible(ref: RefObject<Element | null>): boolean {
  const [onScreen, setOnScreen] = useState(true);
  const [front, setFront] = useState(true);
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver(([e]) => setOnScreen(e.isIntersecting));
    io.observe(el);
    return () => io.disconnect();
  }, [ref]);
  useEffect(() => {
    if (typeof document === "undefined") return;
    const on = () => setFront(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", on);
    return () => document.removeEventListener("visibilitychange", on);
  }, []);
  return onScreen && front;
}

/** Run `fn` every `ms` while `on`, and once when it turns on. Overlapping runs are skipped. */
export function usePoll(on: boolean, ms: number, fn: () => Promise<unknown>) {
  const latest = useRef(fn);
  useEffect(() => {
    latest.current = fn;
  });
  useEffect(() => {
    if (!on) return;
    let busy = false;
    const tick = () => {
      if (busy) return;
      busy = true;
      void latest.current().finally(() => (busy = false));
    };
    tick();
    const t = setInterval(tick, ms);
    return () => clearInterval(t);
  }, [on, ms]);
}
