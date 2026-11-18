import { SecretsError } from "./errors.js";
import { openSealed } from "./seal.js";
import type { Deleted, ExportFormat, Lease, Problem, SealedValues, Secret, SecretInfo, Totp, TransitPublicKey, Values, Written } from "./types.js";

export type ClientOptions = {
  /** rolld's API, e.g. `https://api.roll.example` (no trailing `/v1`). */
  api: string;
  /** A roll token (`rlt_…`) with a rule on the project, or the operator token. */
  token: string;
  /** The project's id (`prj_…`). A scoped token knows its own project, so it may be left out. */
  project?: string;
  /** For tests and other runtimes; the global `fetch` by default. */
  fetch?: typeof fetch;
  /**
   * Keep `values()` answers this long (ms) and answer from memory meanwhile; after that, ask
   * again with the digest it was read against, and an unchanged folder costs a 304 that reads,
   * issues and audits nothing. A folder holding dynamic credentials is never kept (they end with
   * their lease). Off by default.
   */
  cacheTtlMs?: number;
  /** How many folders `cacheTtlMs` keeps; past it, the one read longest ago goes. No cap by default. */
  cacheSize?: number;
  /** For tests: the clock (ms) the cache's TTL is measured with; `Date.now` by default. */
  now?: () => number;
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
const bytes = (d: Uint8Array | string) => (typeof d === "string" ? new TextEncoder().encode(d) : d);
const toB64 = (d: Uint8Array) => {
  let bin = "";
  for (const b of d) bin += String.fromCharCode(b);
  return btoa(bin);
};
const fromB64 = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const qs = (q: Record<string, string | undefined>) => {
  const parts = Object.entries(q)
    .filter(([, v]) => v !== undefined && v !== "")
    .map(([k, v]) => `${enc(k)}=${enc(v as string)}`);
  return parts.length ? `?${parts.join("&")}` : "";
};

/** An answer that isn't 2xx as a `SecretsError`: roll's message and code, or the status. */
function failure(res: Response, text: string): SecretsError {
  let err: { error?: string; code?: string } = {};
  try {
    err = JSON.parse(text);
  } catch {
    // not JSON: the status says it
  }
  return new SecretsError(err.error ?? `${res.status} ${res.statusText}`.trim(), res.status, err.code ?? null);
}

/** Where a workload's OIDC token comes from: called again for each login (a file replaced on disk, a fresh CI token). */
export type TokenSource = () => string | Promise<string>;

export type LoginOptions = Omit<ClientOptions, "token"> & {
  /** The workload identity's id (`idn_…`). */
  identity: string;
  /** Called for a fresh OIDC token at each login: this first one, then before the roll token ends and when the server refuses it. */
  tokenSource: TokenSource;
};

/**
 * A logged-in client's token, kept fresh as the Go and Python clients keep theirs: a new login once
 * less than a fifth of its life (at least a minute) is left, or when the server refuses it.
 */
function relogin(base: string, identity: string, source: TokenSource, doFetch: typeof fetch) {
  let token = "";
  let issued = 0;
  let expires = 0;
  let project: string | null = null;
  let pending: Promise<string> | null = null;
  async function login(): Promise<string> {
    const oidc = (await source()).trim();
    const res = await doFetch(`${base}/v1/secrets/login`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ identity, token: oidc }),
      cache: "no-store",
    });
    const text = await res.text();
    if (!res.ok) throw failure(res, text);
    const a = JSON.parse(text) as { token?: string; project?: string | null; expires_at?: number | null };
    if (!a.token) throw new SecretsError("the login answered no token", 502);
    [token, issued, expires, project] = [a.token, Date.now(), a.expires_at ?? 0, a.project ?? null];
    return token;
  }
  // One login at a time, whoever asks.
  const renew = () => (pending ??= login().finally(() => (pending = null)));
  return {
    renew,
    project: () => project,
    /** The token, renewed first when it's ending, or when `stale` (the one the server refused) is still it. */
    async token(stale?: string): Promise<string> {
      const margin = Math.max(60_000, (expires - issued) / 5);
      // An answer without an expiry is renewed only when the server refuses it.
      const ending = expires > 0 && Date.now() > expires - margin;
      const refused = stale !== undefined && stale === token;
      if (refused || ending) {
        try {
          return await renew();
        } catch (e) {
          // Renewing early that fails keeps the token it has until it really ends.
          if (refused || Date.now() >= expires) throw e;
        }
      }
      return token;
    },
  };
}

/**
 * A client logged in as a workload identity (docs/secrets.md, "Workload identities") with the
 * workload's own OIDC token from `tokenSource`: nothing long-lived in its settings. It logs in again
 * before the roll token ends and when the server refuses it, asking `tokenSource` afresh each time.
 * The project is the identity's unless `project` says otherwise. Throws `SecretsError` when the
 * first login fails.
 */
export async function loginWithIdentity(options: LoginOptions): Promise<SecretsClient> {
  const { identity, tokenSource, ...rest } = options;
  const login = relogin(options.api.replace(/\/+$/, ""), identity, tokenSource, options.fetch ?? fetch);
  await login.renew();
  return makeClient({ ...rest, token: "", project: options.project ?? login.project() ?? undefined }, login);
}

/** Reads one setting; the process's environment (trimmed) by default. */
export type Getenv = (key: string) => string;

const processEnv: Getenv = (key) => ((globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env?.[key] ?? "").trim();

/**
 * The workload's OIDC token as `fromEnv` finds it, as the Go and Python clients do: `ROLL_OIDC_TOKEN`,
 * the file `ROLL_OIDC_TOKEN_FILE` names (read now: a projected token is replaced on disk), or GitHub
 * Actions' token for the audience `ROLL_OIDC_AUDIENCE` (by default the API's own URL: a token minted
 * for one roll server isn't good at another).
 */
export async function oidcTokenFromEnv(api: string, getenv: Getenv = processEnv, fetchFn: typeof fetch = fetch): Promise<string> {
  const direct = getenv("ROLL_OIDC_TOKEN");
  if (direct) return direct;
  const file = getenv("ROLL_OIDC_TOKEN_FILE");
  if (file) {
    // Node only (a browser has no token file): imported by name, so the package needs no Node types.
    const fsModule: string = "node:fs/promises";
    const { readFile } = (await import(fsModule)) as { readFile: (path: string, encoding: "utf8") => Promise<string> };
    return (await readFile(file, "utf8")).trim();
  }
  const requestUrl = getenv("ACTIONS_ID_TOKEN_REQUEST_URL");
  const bearer = getenv("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
  if (!requestUrl || !bearer) {
    throw new Error("ROLL_IDENTITY is set but there's no OIDC token: set ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE, or run in GitHub Actions with `permissions: id-token: write`");
  }
  const audience = getenv("ROLL_OIDC_AUDIENCE") || api.replace(/\/+$/, "");
  const res = await fetchFn(`${requestUrl}${requestUrl.includes("?") ? "&" : "?"}audience=${encodeURIComponent(audience)}`, { headers: { authorization: `Bearer ${bearer}` } });
  if (!res.ok) throw new Error(`GitHub Actions didn't issue an OIDC token (${res.status})`);
  const body = (await res.json()) as { value?: unknown };
  if (typeof body.value !== "string" || !body.value) throw new Error("GitHub Actions answered no OIDC token");
  return body.value;
}

/**
 * A client from the process's settings, as the Go and Python clients' `FromEnv`/`from_env`:
 * `ROLL_API_URL`, then `ROLL_TOKEN`, or `ROLL_IDENTITY` for a workload identity (its OIDC token from
 * `oidcTokenFromEnv`, asked afresh at each login), and `ROLL_PROJECT` if set.
 */
export async function fromEnv(options: { getenv?: Getenv; fetch?: typeof fetch; cacheTtlMs?: number } = {}): Promise<SecretsClient> {
  const env = options.getenv ?? processEnv;
  const fetchFn = options.fetch ?? fetch;
  const api = env("ROLL_API_URL");
  if (!api) throw new Error("ROLL_API_URL isn't set");
  const project = env("ROLL_PROJECT") || undefined;
  const token = env("ROLL_TOKEN");
  if (token) return createClient({ api, token, project, fetch: options.fetch, cacheTtlMs: options.cacheTtlMs });
  const identity = env("ROLL_IDENTITY");
  if (identity) return loginWithIdentity({ api, identity, tokenSource: () => oidcTokenFromEnv(api, env, fetchFn), project, fetch: options.fetch, cacheTtlMs: options.cacheTtlMs });
  throw new Error("set ROLL_TOKEN, or ROLL_IDENTITY for a workload identity");
}

/**
 * A client for one project's secrets (docs/secrets.md, "Consumption"): its environments' values,
 * the runtime resolve (sealed to a key only this process holds, if asked), the digest that changes
 * when the values would, a watch that follows it, one key's record and stored value, and one key's
 * writes.
 */
export function createClient(options: ClientOptions) {
  return makeClient(options);
}

function makeClient(options: ClientOptions, login?: ReturnType<typeof relogin>) {
  const base = options.api.replace(/\/+$/, "");
  const doFetch = options.fetch ?? fetch;
  let project: Promise<string> | null = options.project ? Promise.resolve(options.project) : null;

  /** `send` with the client's token; a logged-in client's that the server refuses is renewed, then tried once more. */
  async function authed(send: (token: string) => Promise<Response>): Promise<Response> {
    if (!login) return send(options.token);
    const token = await login.token();
    const res = await send(token);
    if (res.status !== 401) return res;
    await res.body?.cancel();
    return send(await login.token(token));
  }

  async function call(method: string, path: string, body?: unknown, raw = false, signal?: AbortSignal): Promise<unknown> {
    const res = await authed((token) =>
      doFetch(`${base}${path}`, {
        method,
        headers: { authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
        body: body === undefined ? undefined : JSON.stringify(body),
        cache: "no-store",
        signal,
      }),
    );
    const text = await res.text();
    if (!res.ok) throw failure(res, text);
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

  /**
   * `fresh`: skip `cacheTtlMs` (a watch that saw the digest move reads anew). `dynamic: false`:
   * no dynamic credential is issued (for a reader that can't renew a lease): those keys are left
   * out and named in `Values.dynamic`, and such an answer can be kept.
   */
  function values(environment: string, o?: { path?: string; format?: undefined; resolve?: boolean; dynamic?: boolean; fresh?: boolean }): Promise<Values>;
  function values(environment: string, o: { path?: string; format: ExportFormat; resolve?: boolean; dynamic?: boolean }): Promise<string>;
  async function values(environment: string, o: { path?: string; format?: ExportFormat; resolve?: boolean; dynamic?: boolean; fresh?: boolean } = {}): Promise<Values | string> {
    const url = `${await env(environment)}/values${qs({ path: o.path, format: o.format, resolve: o.resolve === false ? "false" : undefined, dynamic: o.dynamic === false ? "false" : undefined })}`;
    if (!options.cacheTtlMs || o.format || o.fresh) {
      const v = (await call("GET", url, undefined, !!o.format)) as Values | string;
      // A fresh read is the newest answer: a kept one for the same folder goes.
      if (o.fresh) cache.delete(url);
      return v;
    }
    return structuredClone(await cached(url, options.cacheTtlMs, o.dynamic === false));
  }

  /** Values kept for `ttl` ms, then revalidated with their digest (a 304 keeps them). */
  const cache = new Map<string, { values: Values; etag: string | null; at: number }>();
  const now = options.now ?? Date.now;
  async function cached(url: string, ttl: number, noDynamic: boolean): Promise<Values> {
    const hit = cache.get(url);
    if (hit && now() - hit.at < ttl) return hit.values;
    const sep = url.includes("?") ? "&" : "?";
    const res = await authed((token) =>
      doFetch(`${base}${url}${sep}digest=1`, {
        method: "GET",
        headers: { authorization: `Bearer ${token}`, ...(hit?.etag ? { "if-none-match": `"${hit.etag}"` } : {}) },
        cache: "no-store",
      }),
    );
    if (res.status === 304 && hit) {
      hit.at = now();
      return hit.values;
    }
    const text = await res.text();
    if (!res.ok) throw failure(res, text);
    const v = JSON.parse(text) as Values;
    const etag = res.headers.get("etag")?.replace(/^W\//, "").replace(/"/g, "") ?? null;
    // Never kept: dynamic credentials (they end with their lease; with `dynamic=false` there are
    // none, only their names) or a key that didn't resolve (the digest may not move when the link
    // recovers).
    if ((v.dynamic.length && !noDynamic) || v.problems.length) cache.delete(url);
    else {
      if (!cache.has(url) && options.cacheSize && cache.size >= options.cacheSize) {
        let oldest: string | null = null;
        for (const [k, x] of cache) if (oldest === null || x.at < cache.get(oldest)!.at) oldest = k;
        if (oldest !== null) cache.delete(oldest);
      }
      cache.set(url, { values: v, etag, at: now() });
    }
    return v;
  }

  const secretUrl = async (environment: string, key: string, path?: string, suffix = "", q: Record<string, string | undefined> = {}) =>
    `${await env(environment)}/secrets/${enc(key)}${suffix}${qs({ path, ...q })}`;

  /**
   * `key`'s record as `path`'s folder (the root by default) sees it, without its value: its
   * version, comment, link kind and where it's set (`source`: its own, a base's or an import's).
   * null when the folder has no such key.
   */
  async function secret(environment: string, key: string, o: { path?: string } = {}): Promise<SecretInfo | null> {
    let s: Secret;
    try {
      s = (await call("GET", await secretUrl(environment, key, o.path))) as Secret;
    } catch (e) {
      if (e instanceof SecretsError && e.status === 404) return null;
      throw e;
    }
    const { key: k, path, type, version, comment, tags, source, updated_at, updated_by, expires_at, link } = s;
    return { key: k, path, type, version, comment, tags, source, updated_at, updated_by, expires_at: expires_at ?? null, link_kind: link?.kind ?? null };
  }

  /**
   * `key`'s value as stored, references left as written (`resolve=false`). A reveal: the token
   * needs `reveal`, the read is audited, and `cacheTtlMs` never keeps it. A linked key's value
   * still comes from its link. A key that doesn't read (sensitive, a link that failed) is a
   * `SecretsError` with the code `unresolved`; errors never hold the value.
   */
  async function storedValue(environment: string, key: string, o: { path?: string } = {}): Promise<string> {
    const r = (await call("GET", await secretUrl(environment, key, o.path, "/value", { resolve: "false" }))) as { value?: unknown; problems?: Problem[] } | null;
    const problem = r?.problems?.[0];
    if (problem) throw new SecretsError(`${key} doesn't read: ${problem.problem}`, 409, "unresolved");
    if (typeof r?.value !== "string") throw new SecretsError(`the answer for ${key} has no value`, 502);
    return r.value;
  }

  /**
   * Sets `key` to `value` in `path`'s folder (the root by default), replacing a link the key had;
   * `comment` and the change's `message` are optional. Answers the change and the key's new
   * version; the same value again writes nothing (`change: null`). A protected environment, or a
   * folder that needs approval, is a `SecretsError` with the code `approval_required` (open a
   * change request instead). Errors never hold the value. Drops the cached reads.
   */
  async function set(environment: string, key: string, value: string, o: { path?: string; comment?: string; message?: string } = {}): Promise<Written> {
    try {
      return (await call("PUT", await secretUrl(environment, key, o.path), { value, link: null, comment: o.comment, message: o.message })) as Written;
    } finally {
      cache.clear();
    }
  }

  /**
   * Deletes `key` from `path`'s folder. A key that isn't there (or only shows through a base or an
   * import) is `{deleted: false}`, not an error; `approval_required` as for `set`. Drops the cached
   * reads.
   */
  async function del(environment: string, key: string, o: { path?: string } = {}): Promise<Deleted> {
    try {
      const w = (await call("DELETE", await secretUrl(environment, key, o.path))) as Written;
      return { deleted: true, change: w.change };
    } catch (e) {
      if (e instanceof SecretsError && e.status === 404) return { deleted: false, change: null };
      throw e;
    } finally {
      cache.clear();
    }
  }

  function resolve(environment: string, o: { consumer: string; path?: string; sealTo?: undefined }): Promise<Values>;
  function resolve(environment: string, o: { consumer: string; path?: string; sealTo: string }): Promise<SealedValues>;
  async function resolve(environment: string, o: { consumer: string; path?: string; sealTo?: string }): Promise<Values | SealedValues> {
    return (await call("POST", `${await env(environment)}/resolve`, { consumer: o.consumer, path: o.path, seal_to: o.sealTo })) as Values | SealedValues;
  }

  /** A TOTP key's current code (docs/secrets.md, "TOTP keys"): needs reveal, audited without the code. */
  async function totp(environment: string, key: string, o: { path?: string } = {}): Promise<Totp> {
    return (await call("GET", `${await env(environment)}/secrets/${enc(key)}/totp${qs({ path: o.path })}`)) as Totp;
  }

  /**
   * The folder's digest. With `since` and `waitSeconds` (at most 60), a long poll: it answers as
   * soon as the digest differs from `since`, or with the same one when the wait runs out.
   */
  async function digest(environment: string, path?: string, o: { since?: string; waitSeconds?: number; signal?: AbortSignal } = {}): Promise<string> {
    const wait = o.since && o.waitSeconds ? { since: o.since, wait: String(Math.min(25, Math.max(1, Math.round(o.waitSeconds)))) } : {};
    return ((await call("GET", `${await env(environment)}/digest${qs({ path, ...wait })}`, undefined, false, o.signal)) as { digest: string }).digest;
  }

  async function renew(lease: string, ttlMinutes?: number): Promise<Lease> {
    return (await call("POST", `/v1/secrets/projects/${enc(await projectId())}/leases/${enc(lease)}/renew`, ttlMinutes ? { ttl_minutes: ttlMinutes } : {})) as Lease;
  }

  /**
   * Calls `onChange` with the values now, then follows the digest and calls it again whenever they
   * change, renewing the dynamic leases of the latest values every 10 minutes meanwhile (as
   * `roll run --watch` does). It long-polls (a change arrives within a second or two); a server
   * that answers at once is asked again every `intervalMs`. Returns the function that stops it.
   */
  function watch(environment: string, o: WatchOptions): () => void {
    const every = Math.max(1, o.intervalMs ?? 5000);
    const fail = o.onError ?? (() => {});
    let stopped = false;
    // Stopping abandons a held long poll (nothing is called back after stop).
    const abort = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let leases: string[] = [];
    let last: string | null = null;
    // Always a fresh read: the digest moved, so a kept answer is the old one.
    const load = () => (o.consumer ? resolve(environment, { consumer: o.consumer, path: o.path }) : values(environment, { path: o.path, fresh: true }));
    const take = async (v: Values) => {
      if (v.problems.length) throw new SecretsError(`${environment} doesn't resolve: ${v.problems.map((p) => `${p.key}: ${p.problem}`).join("; ")}`, 409, "unresolved");
      leases = v.leases;
      await o.onChange(v);
    };
    const tick = async () => {
      if (stopped) return;
      // How soon to ask again: at once after a held or changed answer, else `every`.
      let next = every;
      try {
        const asked = Date.now();
        const now = await digest(environment, o.path, last === null ? { signal: abort.signal } : { since: last, waitSeconds: 25, signal: abort.signal });
        if (stopped) return;
        if (last !== null && (now !== last || Date.now() - asked >= 1000)) next = 0;
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
      if (!stopped) timer = setTimeout(tick, next);
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
      abort.abort();
      clearTimeout(timer);
      clearInterval(renewing);
    };
  }

  /**
   * An encryption key (docs/secrets.md, "Encryption keys"): the key stays in roll; data goes there
   * and back. Strings are sent as UTF-8; decrypt answers bytes.
   */
  function transit(name: string) {
    const at = async (op: string) => `/v1/secrets/projects/${enc(await projectId())}/transit/${enc(name)}/${op}`;
    const post = async <T>(op: string, body: unknown) => (await call("POST", await at(op), body)) as T;
    return {
      encrypt: async (data: Uint8Array | string, context?: string) => (await post<{ ciphertext: string }>("encrypt", { plaintext: toB64(bytes(data)), context })).ciphertext,
      decrypt: async (ciphertext: string, context?: string) => fromB64((await post<{ plaintext: string }>("decrypt", { ciphertext, context })).plaintext),
      /** Ed25519 over the bytes as given: anyone with `publicKeys()` checks it. */
      sign: async (data: Uint8Array | string) => (await post<{ signature: string }>("sign", { input: toB64(bytes(data)) })).signature,
      verify: async (data: Uint8Array | string, signature: string) => (await post<{ valid: boolean }>("verify", { input: toB64(bytes(data)), signature })).valid,
      /** HMAC-SHA256, the same for the same input: a lookup column over encrypted fields. */
      hmac: async (data: Uint8Array | string) => (await post<{ hmac: string }>("hmac", { input: toB64(bytes(data)) })).hmac,
      publicKeys: async () => (await call("GET", await at("public"))) as TransitPublicKey[],
    };
  }

  /** A sealed resolve's values, opened with the key pair whose public key was `sealTo`. */
  function open(r: SealedValues, secretKey: Uint8Array): Record<string, string> {
    return JSON.parse(new TextDecoder().decode(openSealed(r.sealed, secretKey, r.context))) as Record<string, string>;
  }

  return { values, secret, storedValue, set, delete: del, resolve, digest, totp, renew, watch, open, transit, project: projectId };
}

export type SecretsClient = ReturnType<typeof createClient>;
