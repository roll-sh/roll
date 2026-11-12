/**
 * Client for roll secrets (`crates/roll-secrets`, docs/secrets.md "HTTP API"): a project's
 * environments, folders, secrets, history, links, change requests, tokens and audit. The shapes are
 * the contract's; change both together.
 *
 * Values only come back from the reveal routes (`value`, `versionValue`, `values`). Callers keep a
 * revealed value for as long as it's shown and no longer.
 */

import { apiHeaders, onApiStatus, ROLL_API } from "./roll-api";

export type Action = "read" | "reveal" | "write" | "delete" | "admin";
export const ACTIONS: readonly Action[] = ["read", "reveal", "write", "delete", "admin"];
export type SecretType = "shared" | "personal";
export type LinkMode = "live" | "managed" | "dynamic";

export type Environment = {
  id: string;
  slug: string;
  name: string;
  position: number;
  kind: "standard" | "ephemeral";
  base: string | null;
  labels: Record<string, string>;
  expires_at: number | null;
  protected: boolean;
  approvals: number;
  approvers: string[];
  created_at: number;
  secrets: number;
};
export type NewEnvironment = {
  slug: string;
  name?: string;
  kind?: "standard" | "ephemeral";
  base?: string;
  labels?: Record<string, string>;
  expires_at?: number;
  ttl_minutes?: number;
  secrets?: Op[];
};
export type Overview = { project: string; environments: Environment[]; previews: number };
export type Folder = { path: string; name: string; secrets: number; folders: number };
export type Import = { environment: string; path: string };
export type Link = { kind: string; ref: string; field: string; mode: LinkMode; options?: Record<string, unknown> };
export type Source = { kind: "own" } | { kind: "import"; environment: string; path: string } | { kind: "base"; environment: string };
export type Secret = {
  id: string;
  key: string;
  path: string;
  type: SecretType;
  version: number;
  comment: string;
  tags: string[];
  link: Link | null;
  fp: string;
  references: string[];
  updated_at: number;
  updated_by: string;
  source: Source;
  /** A personal value of the caller replaces it. */
  overridden: boolean;
};
export type SetSecret = { value?: string; comment?: string; tags?: string[]; type?: SecretType; link?: Link | null; message?: string };
export type Op =
  | { op: "set"; key: string; value?: string; comment?: string; tags?: string[]; type?: SecretType; link?: Link | null }
  | { op: "delete"; key: string; type?: SecretType };
export type Batch = { path?: string; message?: string; ops: Op[]; if_seq?: number };
export type ChangeKind = "write" | "rollback" | "promote" | "rotate" | "request" | "rename";
export type Change = { seq: number; environment: string; path: string; message: string; by: string; at: number; keys: number; kind: ChangeKind };
/** `what`: added | changed | deleted | comment | tags | link | rotated */
export type Item = { key: string; path: string; type: SecretType; what: string[]; from_fp: string | null; to_fp: string | null };
export type Written = { change: Change | null; secret: Secret | null; warnings: string[] };
/** `change` is null when nothing changed. */
/** `request`: the change request opened instead (a promotion into a protected environment). */
export type Applied = { change: Change | null; items: Item[]; warnings: string[]; request?: ChangeRequest };
export type StateRow = { key: string; path: string; version: number; fp: string; comment: string; tags: string[]; link: Link | null; at: number };
export type ChangeDetail = Change & { items: Item[]; state?: StateRow[] };
export type Version = { version: number; seq: number; op: "set" | "delete"; what: string[]; fp: string | null; comment: string; tags: string[]; link: Link | null; at: number; by: string };
export type Problem = { key: string; problem: string };
/** `leases`: ids of dynamic credentials issued. */
export type Values = { values: Record<string, string>; problems: Problem[]; leases: string[] };
export type CellSource = "own" | "import" | "base";
export type Cell = { fp: string; version: number; updated_at: number; link: Link | null; reference: boolean; source: CellSource; comment: boolean; personal: boolean };
export type RowStatus = "same" | "different" | "missing";
export type GridRow = { key: string; cells: Record<string, Cell | null>; status: RowStatus };
export type Grid = { path: string; environments: string[]; rows: GridRow[]; folders: string[]; previews: number };
export type CompareStatus = "same" | "different" | "only_from" | "only_to";
export type CompareRow = { key: string; status: CompareStatus; from: number | null; to: number | null };
export type Compare = { from: string; to: string; path: string; rows: CompareRow[] };
export type RequestState = "open" | "approved" | "rejected" | "applied" | "closed" | "stale";
export type Review = { by: string; verdict: "approve" | "reject"; comment: string; at: number };
export type ChangeRequest = {
  id: string;
  environment: string;
  path: string;
  message: string;
  by: string;
  at: number;
  state: RequestState;
  items: Item[];
  reviews: Review[];
  approvals: number;
  approvals_needed: number;
  applied_seq: number | null;
};
export type LinkField = { field: string; label: string; modes: LinkMode[]; secret: boolean; suggest: string; credential?: string };
export type LinkTarget = { kind: string; ref: string; name: string; fields: LinkField[] };
export type Lease = {
  id: string;
  environment: string;
  key: string;
  kind: "dynamic" | "retired";
  link: Link | null;
  consumer: string;
  issued_at: number;
  expires_at: number;
  revoked_at: number | null;
};
export type Token = {
  id: string;
  name: string;
  environments: string[] | null;
  paths: string[] | null;
  actions: Action[];
  expires_at: number | null;
  created_at: number;
  created_by: string;
  last_used_at: number | null;
  prefix: string;
};
export type NewToken = { name: string; environments?: string[]; paths?: string[]; actions: Action[]; expires_at?: number; ttl_days?: number };
export type AuditEntry = { id: number; at: number; actor: string; action: string; environment: string | null; path: string | null; keys: string[]; summary: string; ip: string | null };
export type AuditFilter = { env?: string; key?: string; actor?: string; action?: string; before?: number; limit?: number };
export type Imported = Applied & { skipped: string[] };
export type ImportedAsRequest = { request: ChangeRequest; skipped: string[] };
export type ExportFormat = "dotenv" | "json" | "shell" | "yaml";
/** `resealed`: "all", or how many projects still need the previous key (null without a key); `links`: the kinds with a link provider. */
export type Health = { ok: boolean; key: "env" | "file" | "missing"; resealed: "all" | number | null; links: string[] };
export type WhoAmI = { principal: string; project?: string | null; scopes?: unknown };

/** An answer that isn't 2xx: the API's `error`, its `code` (`approval_required`, `stale`, `conflict`…) and the status. */
export class SecretsError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string | null,
  ) {
    super(message);
    this.name = "SecretsError";
  }
}

/** A write to a protected environment: it needs a change request with the same operations. */
export const needsApproval = (e: unknown): e is SecretsError => e instanceof SecretsError && e.code === "approval_required";

/** Like `call` in roll-api.ts, but keeps the error's `code`, which the approval flow needs. */
async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${ROLL_API}${path}`, {
    method,
    headers: apiHeaders(body),
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  onApiStatus(res.status);
  const text = await res.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    if (res.ok) throw new Error(`${method} ${path}: the response isn't JSON`);
  }
  if (!res.ok) {
    const err = (data ?? {}) as { error?: string; code?: string };
    throw new SecretsError(err.error ?? (`${res.status} ${res.statusText}`.trim() || "request failed"), res.status, err.code ?? null);
  }
  return data as T;
}

const enc = encodeURIComponent;

/** A query string from the set entries (`?a=1&b=2`, or nothing). */
export function qs(q: Record<string, string | number | boolean | undefined | null>): string {
  const parts = Object.entries(q)
    .filter(([, v]) => v !== undefined && v !== null && v !== "")
    .map(([k, v]) => `${enc(k)}=${enc(String(v))}`);
  return parts.length ? `?${parts.join("&")}` : "";
}

/** The client for one project. */
export function secretsApi(project: string) {
  const P = `/v1/secrets/projects/${enc(project)}`;
  const E = (env: string) => `${P}/environments/${enc(env)}`;
  const K = (env: string, key: string) => `${E(env)}/secrets/${enc(key)}`;
  return {
    overview: () => req<Overview>("GET", P),
    grid: (path = "/") => req<Grid>("GET", `${P}/grid${qs({ path })}`),
    compare: (from: string, to: string, path = "/") => req<Compare>("GET", `${P}/compare${qs({ from, to, path })}`),

    environments: () => req<Environment[]>("GET", `${P}/environments`),
    createEnvironment: (e: NewEnvironment) => req<Environment>("POST", `${P}/environments`, e),
    deleteByLabel: (label: string) => req<{ deleted: string[] }>("DELETE", `${P}/environments${qs({ label })}`),
    order: (order: string[]) => req<Environment[]>("PUT", `${P}/environments/order`, { order }),
    environment: (env: string) => req<Environment>("GET", E(env)),
    updateEnvironment: (env: string, patch: { slug?: string; name?: string; expires_at?: number | null }) => req<Environment>("PATCH", E(env), patch),
    deleteEnvironment: (env: string, confirm?: string) => req<null>("DELETE", `${E(env)}${qs({ confirm })}`),
    policy: (env: string, p: { protected: boolean; approvals?: number; approvers?: string[] }) => req<Environment>("PUT", `${E(env)}/policy`, p),

    folders: (env: string, path = "/") => req<{ path: string; folders: Folder[] }>("GET", `${E(env)}/folders${qs({ path })}`),
    createFolder: (env: string, path: string) => req<Folder>("POST", `${E(env)}/folders`, { path }),
    deleteFolder: (env: string, path: string) => req<{ change: Change | null }>("DELETE", `${E(env)}/folders${qs({ path })}`),
    imports: (env: string, path = "/") => req<Import[]>("GET", `${E(env)}/imports${qs({ path })}`),
    setImports: (env: string, path: string, imports: Import[]) => req<Import[]>("PUT", `${E(env)}/imports${qs({ path })}`, imports),

    secrets: (env: string, path = "/") => req<Secret[]>("GET", `${E(env)}/secrets${qs({ path })}`),
    secret: (env: string, key: string, path = "/", type?: SecretType) => req<Secret>("GET", `${K(env, key)}${qs({ path, type })}`),
    set: (env: string, key: string, body: SetSecret, path = "/") => req<Written>("PUT", `${K(env, key)}${qs({ path })}`, body),
    remove: (env: string, key: string, path = "/", type?: SecretType) => req<Written>("DELETE", `${K(env, key)}${qs({ path, type })}`),
    /** Reveals one value (audited). `resolve` (default true) expands references. */
    value: (env: string, key: string, path = "/", resolve = true) =>
      req<{ value: string; problems: Problem[] }>("GET", `${K(env, key)}/value${qs({ path, resolve: resolve ? undefined : "false" })}`),
    versions: (env: string, key: string, path = "/", type?: SecretType) => req<Version[]>("GET", `${K(env, key)}/versions${qs({ path, type })}`),
    versionValue: (env: string, key: string, version: number, path = "/", type?: SecretType) =>
      req<{ value: string }>("GET", `${K(env, key)}/versions/${version}/value${qs({ path, type })}`),
    rotate: (env: string, key: string, path = "/") => req<Applied>("POST", `${K(env, key)}/rotate${qs({ path })}`),

    values: (env: string, path = "/", resolve = true) => req<Values>("GET", `${E(env)}/values${qs({ path, resolve: resolve ? undefined : "false" })}`),
    /** The environment's values as a file (`dotenv`, `json`, `shell`, `yaml`): text, not JSON. */
    exportText: async (env: string, format: ExportFormat, path = "/"): Promise<string> => {
      const url = `${E(env)}/values${qs({ path, format })}`;
      const res = await fetch(`${ROLL_API}${url}`, { headers: apiHeaders(), cache: "no-store" });
      onApiStatus(res.status);
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
      return text;
    },

    changes: (env: string, before?: number, limit?: number) => req<Change[]>("GET", `${E(env)}/changes${qs({ before, limit })}`),
    batch: (env: string, b: Batch) => req<Applied>("POST", `${E(env)}/changes`, b),
    importDotenv: (env: string, body: { dotenv: string; path?: string; overwrite?: boolean; message?: string }) => req<Imported>("POST", `${E(env)}/import`, body),
    /** The same import as a change request (a protected environment). */
    importAsRequest: (env: string, body: { dotenv: string; path?: string; overwrite?: boolean; message?: string }) =>
      req<ImportedAsRequest>("POST", `${E(env)}/import`, { ...body, request: true }),
    change: (env: string, seq: number, state = false) => req<ChangeDetail>("GET", `${E(env)}/changes/${seq}${qs({ state: state ? 1 : undefined })}`),
    rollback: (env: string, body: { to?: number; at?: number; message?: string; bypass?: boolean }) => req<Applied>("POST", `${E(env)}/rollback`, body),
    promote: (to: string, from: string, body: { path?: string; keys?: string[]; message?: string }) => req<Applied>("POST", `${E(to)}/promote${qs({ from })}`, body),

    requests: (env: string, state?: RequestState) => req<ChangeRequest[]>("GET", `${E(env)}/requests${qs({ state })}`),
    openRequest: (env: string, b: Batch) => req<ChangeRequest>("POST", `${E(env)}/requests`, b),
    request: (env: string, id: string) => req<ChangeRequest>("GET", `${E(env)}/requests/${enc(id)}`),
    review: (env: string, id: string, verdict: "approve" | "reject", comment?: string) =>
      req<ChangeRequest>("POST", `${E(env)}/requests/${enc(id)}/review`, { verdict, comment }),
    apply: (env: string, id: string, bypass = false) => req<ChangeRequest>("POST", `${E(env)}/requests/${enc(id)}/apply`, bypass ? { bypass } : {}),
    close: (env: string, id: string) => req<ChangeRequest>("POST", `${E(env)}/requests/${enc(id)}/close`),

    links: () => req<LinkTarget[]>("GET", `${P}/links`),
    leases: () => req<Lease[]>("GET", `${P}/leases`),
    revokeLease: (id: string) => req<null>("DELETE", `${P}/leases/${enc(id)}`),

    tokens: () => req<Token[]>("GET", `${P}/tokens`),
    createToken: (t: NewToken) => req<Token & { token: string }>("POST", `${P}/tokens`, t),
    revokeToken: (id: string) => req<null>("DELETE", `${P}/tokens/${enc(id)}`),

    audit: (f: AuditFilter = {}) => req<AuditEntry[]>("GET", `${P}/audit${qs(f)}`),
  };
}

export type SecretsApi = ReturnType<typeof secretsApi>;

export const secretsHealth = () => req<Health>("GET", "/v1/secrets/health");
export const secretsWhoami = () => req<WhoAmI>("GET", "/v1/secrets/whoami");

/** The rule for keys (docs/secrets.md, "Secrets"). */
export const SECRET_KEY = /^[A-Za-z_][A-Za-z0-9_]{0,255}$/;
/** The rule for environment slugs. */
export const ENV_SLUG = /^[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?$/;
/** The rule for tags. */
export const TAG = /^[a-z0-9-]{1,32}$/;
