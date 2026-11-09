/**
 * roll-hosted sandboxes: the typed client for `/v1/code/agents/{owner}/sandboxes`
 * (docs/code/http-api.md, "Sandboxes"; docs/code/agents.md, "Sandboxes").
 *
 * A sandbox is also a machine: it shows up in the agents state and stream with `Machine.sandbox`
 * set, and its status changes arrive there as `machine` events. This client lists, creates,
 * starts, stops and deletes sandboxes and reads their metered running time. The env given at
 * create (provider keys) is sent once and never comes back.
 */

import { ApiError } from "./code-api";
import { getToken, requestSignIn } from "./code-token";
import { array, decode, int, literal, nullable, num, obj, str, withDefault, type Decoder } from "./decode";
import { machineD, type Machine, type Millis, type SandboxStatus, type StopReason } from "./agents-api";
import { ROLL_API } from "./roll-api";

/** A size on offer and its always-on price. */
export interface SandboxSize {
  name: string;
  cpu: number;
  memory_gb: number;
  disk_gb: number;
  hourly_cents: number;
}

/** What the sandbox should be: what the owner asked for, or asleep (stopped while idle; the next command wakes it). */
export type SandboxDesired = "running" | "stopped" | "asleep" | "deleted";

export interface Sandbox {
  /** `sbx_…`. */
  id: string;
  name: string;
  /** The machine (`mch_…`) it runs as, on the canvas. */
  machine: string;
  backend: string;
  size: SandboxSize;
  /** The same as its machine's `sandbox.status`. */
  status: SandboxStatus;
  desired: SandboxDesired;
  /** Why a `stopped` one is stopped: asleep (`idle`) or its owner's stop. */
  stopped_reason: StopReason | null;
  /** The backend's last complaint (a capacity error being retried, why it failed). */
  error: string | null;
  created_at: Millis;
  updated_at: Millis;
  /** When its machine last did something; it sleeps an idle window after. */
  last_active_at: Millis | null;
}

export interface SandboxList {
  sandboxes: Sandbox[];
  /** The first is the default. */
  sizes: SandboxSize[];
  /** Sandboxes one owner may have. */
  max: number;
  /** `null`: sandboxes are off on this server (creating one is a 503). */
  backend: string | null;
}

export interface NewSandbox {
  name?: string;
  /** A size's name; the default size when left out. */
  size?: string;
  /** Passed to the sandbox only (e.g. `ANTHROPIC_API_KEY`); never returned. */
  env?: Record<string, string>;
}

export interface CreatedSandbox {
  sandbox: Sandbox;
  machine: Machine;
}

export interface SandboxUsageRow {
  sandbox: string;
  /** `null` for a row whose sandbox record is gone. */
  name: string | null;
  size: string;
  hourly_cents: number;
  running_ms: number;
  running_hours: number;
  price_usd: number;
}

export interface SandboxUsage {
  owner: string;
  /** `YYYY-MM` (UTC). */
  month: string;
  sandboxes: SandboxUsageRow[];
  totals: { running_ms: number; running_hours: number; price_usd: number };
}

const sizeD = obj<SandboxSize>({ name: str, cpu: int, memory_gb: int, disk_gb: int, hourly_cents: int });

export const sandboxD = obj<Sandbox>({
  id: str,
  name: str,
  machine: str,
  backend: str,
  size: sizeD,
  status: literal("creating", "starting", "running", "stopped", "failed", "deleting"),
  desired: literal("running", "stopped", "asleep", "deleted"),
  stopped_reason: withDefault(nullable(literal("idle", "owner")), () => null),
  error: withDefault(nullable(str), () => null),
  created_at: int,
  updated_at: int,
  last_active_at: withDefault(nullable(int), () => null),
});

export const sandboxListD = obj<SandboxList>({
  sandboxes: array(sandboxD),
  sizes: array(sizeD),
  max: int,
  backend: withDefault(nullable(str), () => null),
});

const createdD = obj<CreatedSandbox>({ sandbox: sandboxD, machine: machineD });

export const sandboxUsageD = obj<SandboxUsage>({
  owner: str,
  month: str,
  sandboxes: array(
    obj<SandboxUsageRow>({
      sandbox: str,
      name: withDefault(nullable(str), () => null),
      size: str,
      hourly_cents: int,
      running_ms: int,
      running_hours: num,
      price_usd: num,
    }),
  ),
  totals: obj<SandboxUsage["totals"]>({ running_ms: int, running_hours: num, price_usd: num }),
});

/** What a sandbox costs per running hour, as shown: `$0.03/h`, `$1.20/h`. */
export function formatHourly(cents: number | undefined): string {
  // A server from before hourly prices sends none: say nothing rather than "$NaN/h".
  if (cents === undefined || !Number.isFinite(cents)) return "";
  return `$${(cents / 100).toFixed(2)}/h`;
}

/** A sandbox the owner can start (stopped or asleep, and not on its way out). */
export const canStart = (s: Sandbox) => s.desired !== "deleted" && s.status === "stopped";
/** A sandbox the owner can stop (an asleep one already is). */
export const canStop = (s: Sandbox) => s.desired === "running" && (s.status === "running" || s.status === "creating" || s.status === "starting");

const enc = encodeURIComponent;
const base = (owner: string) => `/v1/code/agents/${enc(owner)}/sandboxes`;

export function createSandboxesApi(
  apiBase: string,
  token?: string | (() => string | undefined),
  onUnauthorized?: () => void,
  doFetch: typeof fetch = (...a) => fetch(...a),
) {
  const call = async <T>(d: Decoder<T> | null, method: string, path: string, body?: unknown): Promise<T | null> => {
    const t = typeof token === "function" ? token() : token;
    const headers: Record<string, string> = t ? { authorization: `Bearer ${t}` } : {};
    if (body !== undefined) headers["content-type"] = "application/json";
    const res = await doFetch(`${apiBase}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    });
    const raw = await res.text();
    let data: unknown = null;
    try {
      data = raw ? JSON.parse(raw) : null;
    } catch {
      data = raw;
    }
    if (!res.ok) {
      if (res.status === 401) onUnauthorized?.();
      const message = (data as { error?: string } | null)?.error ?? `${res.status} ${res.statusText}`;
      throw new ApiError(message, res.status, data);
    }
    return d && data !== null ? decode(d, data) : null;
  };

  return {
    list: async (owner: string) => (await call(sandboxListD, "GET", base(owner)))!,
    /** 422 at the owner's limit or without a server secret; 503 when sandboxes are off. */
    create: async (owner: string, req: NewSandbox = {}) => (await call(createdD, "POST", base(owner), req))!,
    start: async (owner: string, id: string) => (await call(sandboxD, "POST", `${base(owner)}/${enc(id)}/start`))!,
    stop: async (owner: string, id: string) => (await call(sandboxD, "POST", `${base(owner)}/${enc(id)}/stop`))!,
    /** `null` once it's gone (204); the sandbox, `deleting`, while the backend is still at it (202). */
    remove: (owner: string, id: string) => call(sandboxD, "DELETE", `${base(owner)}/${enc(id)}`),
    /** Running time and price per sandbox in `month` (`YYYY-MM`; this UTC month by default). */
    usage: async (owner: string, month?: string) =>
      (await call(sandboxUsageD, "GET", `${base(owner)}/usage${month ? `?month=${enc(month)}` : ""}`))!,
  };
}

export type SandboxesApi = ReturnType<typeof createSandboxesApi>;

export const sandboxesApi = createSandboxesApi(ROLL_API, getToken, requestSignIn);
