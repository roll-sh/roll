import { SecretsError } from "./errors.js";
import { openSealed } from "./seal.js";
import type { ExportFormat, Lease, SealedValues, Values } from "./types.js";

export type ClientOptions = {
  /** rolld's API, e.g. `https://api.roll.example` (no trailing `/v1`). */
  api: string;
  /** A roll token (`rlt_…`) with a rule on the project, or the operator token. */
  token: string;
  /** The project's id (`prj_…`). A scoped token knows its own project, so it may be left out. */
  project?: string;
  /** For tests and other runtimes; the global `fetch` by default. */
  fetch?: typeof fetch;
};

export type WatchOptions = {
  path?: string;
  /** How often the digest is checked (default 5 s). */
  intervalMs?: number;
  /** With a consumer (`app:web`), values come from `resolve` and dynamic leases are the consumer's. */
  consumer?: string;
  /** The values at the start, and again each time they change. */
  onChange: (values: Values) => void | Promise<void>;
  /** A check, a fetch or a lease renewal that failed; the watch goes on. A change that doesn't resolve keeps the previous values. */
  onError?: (error: unknown) => void;
  /** How often `Values.leases` are renewed (default 10 minutes, as `roll run --watch`). */
  renewMs?: number;
};

const enc = encodeURIComponent;
const qs = (q: Record<string, string | undefined>) => {
  const parts = Object.entries(q)
    .filter(([, v]) => v !== undefined && v !== "")
    .map(([k, v]) => `${enc(k)}=${enc(v as string)}`);
  return parts.length ? `?${parts.join("&")}` : "";
};

/**
 * A client for one project's secrets (docs/secrets.md, "Consumption"): its environments' values,
 * the runtime resolve (sealed to a key only this process holds, if asked), the digest that changes
 * when the values would, and a watch that follows it.
 */
export function createClient(options: ClientOptions) {
  const base = options.api.replace(/\/+$/, "");
  const doFetch = options.fetch ?? fetch;
  let project: Promise<string> | null = options.project ? Promise.resolve(options.project) : null;

  async function call(method: string, path: string, body?: unknown, raw = false): Promise<unknown> {
    const res = await doFetch(`${base}${path}`, {
      method,
      headers: { authorization: `Bearer ${options.token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store",
    });
    const text = await res.text();
    if (!res.ok) {
      let err: { error?: string; code?: string } = {};
      try {
        err = JSON.parse(text);
      } catch {
        // not JSON: the status says it
      }
      throw new SecretsError(err.error ?? `${res.status} ${res.statusText}`.trim(), res.status, err.code ?? null);
    }
    return raw ? text : text ? JSON.parse(text) : null;
  }

  /** The project: as given, or the scoped token's own (`GET /v1/secrets/whoami`). */
  function projectId(): Promise<string> {
    project ??= (call("GET", "/v1/secrets/whoami") as Promise<{ project?: string | null }>).then((w) => {
      if (!w.project) throw new SecretsError("this token isn't scoped to a project: pass `project`", 400, "project");
      return w.project;
    });
    return project;
  }
  const env = async (e: string) => `/v1/secrets/projects/${enc(await projectId())}/environments/${enc(e)}`;

  function values(environment: string, o?: { path?: string; format?: undefined; resolve?: boolean }): Promise<Values>;
  function values(environment: string, o: { path?: string; format: ExportFormat; resolve?: boolean }): Promise<string>;
  async function values(environment: string, o: { path?: string; format?: ExportFormat; resolve?: boolean } = {}): Promise<Values | string> {
    const url = `${await env(environment)}/values${qs({ path: o.path, format: o.format, resolve: o.resolve === false ? "false" : undefined })}`;
    return (await call("GET", url, undefined, !!o.format)) as Values | string;
  }

  function resolve(environment: string, o: { consumer: string; path?: string; sealTo?: undefined }): Promise<Values>;
  function resolve(environment: string, o: { consumer: string; path?: string; sealTo: string }): Promise<SealedValues>;
  async function resolve(environment: string, o: { consumer: string; path?: string; sealTo?: string }): Promise<Values | SealedValues> {
    return (await call("POST", `${await env(environment)}/resolve`, { consumer: o.consumer, path: o.path, seal_to: o.sealTo })) as Values | SealedValues;
  }

  async function digest(environment: string, path?: string): Promise<string> {
    return ((await call("GET", `${await env(environment)}/digest${qs({ path })}`)) as { digest: string }).digest;
  }

  async function renew(lease: string, ttlMinutes?: number): Promise<Lease> {
    return (await call("POST", `/v1/secrets/projects/${enc(await projectId())}/leases/${enc(lease)}/renew`, ttlMinutes ? { ttl_minutes: ttlMinutes } : {})) as Lease;
  }

  /**
   * Calls `onChange` with the values now, then polls the digest and calls it again whenever they
   * change, renewing the dynamic leases of the latest values every 10 minutes meanwhile (as
   * `roll run --watch` does). Returns the function that stops it.
   */
  function watch(environment: string, o: WatchOptions): () => void {
    const every = Math.max(1, o.intervalMs ?? 5000);
    const fail = o.onError ?? (() => {});
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let leases: string[] = [];
    let last: string | null = null;
    const load = () => (o.consumer ? resolve(environment, { consumer: o.consumer, path: o.path }) : values(environment, { path: o.path }));
    const take = async (v: Values) => {
      if (v.problems.length) throw new SecretsError(`${environment} doesn't resolve: ${v.problems.map((p) => `${p.key}: ${p.problem}`).join("; ")}`, 409, "unresolved");
      leases = v.leases;
      await o.onChange(v);
    };
    const tick = async () => {
      if (stopped) return;
      try {
        const now = await digest(environment, o.path);
        if (last === null || now !== last) {
          const before = last;
          last = now;
          // A load that failed (the network, the server) is retried on the next tick; a change
          // that doesn't resolve keeps what's running, and the next change tries again.
          const v = await load().catch((e) => {
            last = before;
            throw e;
          });
          await take(v).catch((e) => {
            if (before === null) last = null;
            fail(e);
          });
        }
      } catch (e) {
        fail(e);
      }
      if (!stopped) timer = setTimeout(tick, every);
    };
    const renewing = setInterval(
      () => {
        for (const l of leases) renew(l).catch(fail);
      },
      Math.max(1, o.renewMs ?? 600_000),
    );
    void tick();
    return () => {
      stopped = true;
      clearTimeout(timer);
      clearInterval(renewing);
    };
  }

  /** A sealed resolve's values, opened with the key pair whose public key was `sealTo`. */
  function open(r: SealedValues, secretKey: Uint8Array): Record<string, string> {
    return JSON.parse(new TextDecoder().decode(openSealed(r.sealed, secretKey, r.context))) as Record<string, string>;
  }

  return { values, resolve, digest, renew, watch, open, project: projectId };
}

export type SecretsClient = ReturnType<typeof createClient>;
