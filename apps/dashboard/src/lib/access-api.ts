/**
 * Client for rolld's access API (`crates/roll-access`, docs/permissions.md "HTTP API"): scoped
 * tokens and OAuth connections, approvals, the audit log, the catalogue the builder offers, and
 * the OAuth consent screen's answer. Change both together.
 */

import { apiHeaders, call, ROLL_API } from "./roll-api";

export const ACTIONS = ["read", "write", "delete", "reveal", "deploy", "admin"] as const;
export type Action = (typeof ACTIONS)[number];
export type Effect = "allow" | "ask";

/** `index`: only some indices of it (search: `logs-*`, `a,b`). */
export type Selector = { project: string } | { id: string; index?: string };

/**
 * A selector as the builder writes it: an id, or for search `cluster/indices` (`cl_1/logs-*`;
 * neither a cluster id nor an index name has a `/`).
 */
export const selectorText = (s: { id: string; index?: string }): string => (s.index ? `${s.id}/${s.index}` : s.id);
export function selectorOf(product: string, text: string): { id: string; index?: string } {
  const at = product === "search" ? text.indexOf("/") : -1;
  return at > 0 && at < text.length - 1 ? { id: text.slice(0, at), index: text.slice(at + 1) } : { id: text };
}

export interface Rule {
  /** A product name, or "*" for every product. */
  product: string;
  /** "*": every resource of the product, including ones made later. */
  resources: "*" | Selector[];
  environments?: string[];
  paths?: string[];
  actions: Partial<Record<Action, Effect>>;
}

export interface OAuthClient {
  id: string;
  name: string;
  uri?: string;
  redirect_uris: string[];
  created_at: number;
  last_used_at: number | null;
}

export interface Grant {
  id: string;
  name: string;
  kind: "token" | "oauth";
  rules: Rule[];
  expires_at: number | null;
  ip_allow: string[];
  /** "operator", or the grant that made it. */
  created_by: string;
  created_at: number;
  last_used_at: number | null;
  last_used_ip: string | null;
  revoked_at: number | null;
  client?: OAuthClient;
  /** Requests a minute it may make, across the fleet (each node enforces its share); absent: no limit. */
  rate_per_minute?: number;
  /** Why roll revoked it (a leaked secret: "found in acme/site .env at …"). */
  revoked_why?: string;
  /** What its rules don't allow asks a person instead of being refused. */
  ask_otherwise?: boolean;
  /** Rules a person allowed for a while when approving its requests, each until a time (Unix ms). */
  temporary?: { rule: Rule; until: number; approval?: string }[];
  /** Its requests over the last day (in the list). */
  activity?: { allowed: number; denied: number; asked: number };
}

export interface NewGrant {
  name: string;
  rules: Rule[];
  expires_at?: number | null;
  ip_allow?: string[];
  rate_per_minute?: number | null;
  ask_otherwise?: boolean;
}

/** A token that needs attention (`GET /v1/access/checkup`). */
export interface Checkup {
  grant: { id: string; name: string; kind: Grant["kind"]; last_used_at: number | null; expires_at: number | null; created_at: number };
  /** Most serious first. */
  findings: { kind: "everything" | "never_expires" | "unused" | "narrower" | "roams"; why: string; fix: "tighten" | "expire" | "revoke" | "limit_addresses" }[];
}

/** Checkup findings, most serious first. */
const FINDING_ORDER: Checkup["findings"][number]["kind"][] = ["everything", "never_expires", "unused", "roams", "narrower"];

/** The checkup's tokens, the most serious first (by each one's worst finding, then how many). */
export function byConcern(list: Checkup[]): Checkup[] {
  const worst = (c: Checkup) => Math.min(...c.findings.map((f) => FINDING_ORDER.indexOf(f.kind)));
  return [...list].sort((a, b) => worst(a) - worst(b) || b.findings.length - a.findings.length);
}

/** What a checkup fix is called on its button. */
export const FIX_LABEL: Record<Checkup["findings"][number]["fix"], string> = {
  tighten: "Tighten",
  expire: "Give it an expiry",
  revoke: "Revoke",
  limit_addresses: "Limit addresses",
};

/** A token's rules narrowed to what it used (`GET /v1/access/tokens/{id}/tighten`). */
export interface Tightened {
  since: number;
  days: number;
  used: { product: string; action: Action; resource: string | null; count: number; last_at: number }[];
  rules: Rule[];
  /** The token's rules it was made from: applying checks they haven't changed since. */
  from: Rule[];
  /** Whether `rules` is narrower than the token's. */
  narrower: boolean;
}

/** How long an approval also allows the same action on the same resource. */
export type Remember = "once" | "1h" | "8h" | "1d" | "always";

export const REMEMBER: { value: Remember; label: string }[] = [
  { value: "once", label: "Once" },
  { value: "1h", label: "For an hour" },
  { value: "8h", label: "For 8 hours" },
  { value: "1d", label: "For a day" },
  { value: "always", label: "Always" },
];

/** Where in its resource an approval is (what remembering it keeps to): "production · folder /db". */
export function approvalWhere(a: Pick<Approval, "environment" | "scope_path">): string {
  return [a.environment, a.scope_path && a.scope_path !== "/" ? `folder ${a.scope_path}` : null].filter(Boolean).join(" · ");
}

/** Whether an approval is about one resource, so approving it can be remembered. */
export const rememberable = (a: Pick<Approval, "resource">) => Boolean(a.resource && !a.resource.startsWith("new in "));

export type ApprovalState = "pending" | "approved" | "denied" | "expired" | "used";

export interface Approval {
  id: string;
  grant_id: string;
  grant_name: string;
  product: string;
  action: Action;
  resource: string | null;
  method: string;
  route: string;
  path: string;
  summary: string;
  /** The request's body, secrets redacted. */
  args: unknown;
  tool: string | null;
  state: ApprovalState;
  reason: string | null;
  created_at: number;
  decided_at: number | null;
  decided_by: string | null;
  expires_at: number;
  /** The environment and path the request is about (what remembering it keeps to). */
  environment?: string | null;
  scope_path?: string | null;
  /** The indices it was about (search). */
  scope_index?: string | null;
}

/** Actions that destroy, ship, expose a credential or change who may do what: approve them with care. */
export const RISKY: Action[] = ["delete", "deploy", "reveal", "admin"];

/**
 * A token's waiting requests a person may approve together, among those they can see: two or
 * more, none risky, none in production and none with a body to review (those are read one by
 * one, on their page).
 */
export function approvableTogether(shown: Approval[]): { grant_id: string; grant_name: string; ids: string[] }[] {
  const by = new Map<string, { grant_id: string; grant_name: string; ids: string[] }>();
  for (const a of shown) {
    if (RISKY.includes(a.action) || a.environment === "production" || (a.args !== null && a.args !== undefined)) continue;
    const g = by.get(a.grant_id) ?? { grant_id: a.grant_id, grant_name: a.grant_name, ids: [] };
    g.ids.push(a.id);
    by.set(a.grant_id, g);
  }
  return [...by.values()].filter((g) => g.ids.length > 1);
}

export interface AuditEvent {
  id: string;
  at: number;
  grant_id: string | null;
  grant_name: string | null;
  source: "api" | "mcp";
  tool: string | null;
  method: string;
  /** The route pattern, never the path. */
  route: string;
  product: string | null;
  action: Action | null;
  resource: string | null;
  decision: "allow" | "deny" | "ask";
  approval: string | null;
  status: number;
  args: unknown;
  ip: string | null;
  duration_ms: number;
  /** What made it: an MCP client's name and version, else its User-Agent (what it says it is). */
  client?: string;
}

export interface Preset {
  id: string;
  name: string;
  description: string;
  /** `resources: "project"` stands for the project the user picks. */
  rules: (Omit<Rule, "resources"> & { resources: Rule["resources"] | "project" })[];
}

export interface Catalog {
  products: string[];
  actions: Action[];
  presets: Preset[];
  routes: { method: string; route: string; product: string; action: string; target: string }[];
  operator_only: { route: string; why: string }[];
  api_url: string;
}

export type Me = { kind: "operator"; locked: boolean } | { kind: "token" | "oauth"; grant: Grant; locked: boolean };

export interface Policy {
  max_days?: number | null;
  require_expiry?: boolean;
  /** Tokens unused this many days stop working. */
  max_idle_days?: number | null;
}

export interface Consent {
  client_id: string;
  redirect_uri: string;
  code_challenge: string;
  code_challenge_method?: string;
  state?: string;
  resource?: string;
  name?: string;
  rules: Rule[];
  expires_at?: number | null;
  ip_allow?: string[];
  rate_per_minute?: number | null;
  ask_otherwise?: boolean;
}

const token = (id: string) => `/v1/access/tokens/${encodeURIComponent(id)}`;
const approval = (id: string) => `/v1/access/approvals/${encodeURIComponent(id)}`;

export interface AuditFilter {
  grant?: string;
  product?: string;
  /** What it was about: this resource or anything under it (who touched it). */
  resource?: string;
  action?: string;
  decision?: string;
  source?: string;
  before?: number;
  limit?: number;
}

function query(q: object): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

/** Where the audit log streams to (a SIEM): signed batches of new rows, in order. */
export interface AuditSink {
  id: string;
  url: string;
  status: "new" | "ok" | "error";
  error: string | null;
  /** When the last row it was sent happened. */
  cursor_at: number;
  delivered_at: number | null;
  created_at: number;
  created_by: string;
}

/** Where approvals are posted, and the chat user ids (Slack `U…`) the operator lets decide there. */
export interface ChatTarget {
  install: string;
  channel: string;
  approvers: string[];
}

export const accessApi = {
  me: () => call<Me>("GET", "/v1/access"),
  catalog: () => call<Catalog>("GET", "/v1/access/catalog"),
  tokens: () => call<Grant[]>("GET", "/v1/access/tokens"),
  token: (id: string) => call<Grant>("GET", token(id)),
  /** The token is in the answer this once. */
  create: (g: NewGrant) => call<{ grant: Grant; token: string }>("POST", "/v1/access/tokens", g),
  update: (id: string, patch: Partial<NewGrant>) => call<Grant>("PATCH", token(id), patch),
  revoke: (id: string) => call<Grant>("DELETE", token(id)),
  /** The live tokens that need attention, each with its findings and their fixes. */
  checkup: () => call<Checkup[]>("GET", "/v1/access/checkup"),
  /** The networks it's been used from (/24s and /48s), newest first. */
  networks: (id: string) => call<{ network: string; first_at: number; last_at: number }[]>("GET", `${token(id)}/networks`),
  /** Take back what an approval allowed it for a while (and the approval, if unused). */
  forget: (id: string, approval: string) => call<Grant>("DELETE", `${token(id)}/temporary/${encodeURIComponent(approval)}`),
  /** Its rules narrowed to what its allowed requests used in the last `days` (never wider): a proposal. */
  tighten: (id: string, days?: number) => call<Tightened>("GET", `${token(id)}/tighten${query({ days })}`),
  /** Apply it, made again now, if its rules are still `from` (else 409: look again). */
  applyTighten: (id: string, from: Rule[], days?: number) => call<Grant>("POST", `${token(id)}/tighten`, { from, days }),
  /** A new secret, the same rules; the old one stops at once. Shown this once. */
  rotate: (id: string) => call<{ grant: Grant; token: string }>("POST", `${token(id)}/rotate`),
  /** What the evaluator would decide for a request, without running it (`token`: the operator asks for one). */
  check: (method: string, path: string, opts: { body?: unknown; token?: string } = {}) =>
    call<{ decision: "allow" | "ask" | "deny"; product?: string; action?: Action; resource?: string | null; route?: string; why?: string }>(
      "POST",
      "/v1/access/check",
      { method, path, ...opts },
    ),
  /** The newest approvals, in a state and of one token (`grant`) when given. */
  approvals: (state?: ApprovalState, grant?: string) => call<Approval[]>("GET", `/v1/access/approvals${query({ state, grant })}`),
  /** `remember`: also allow the same action on the same resource for a while, or always. */
  approve: (id: string, remember?: Remember) => call<Approval>("POST", `${approval(id)}/approve`, remember && remember !== "once" ? { remember } : {}),
  deny: (id: string, reason?: string) => call<Approval>("POST", `${approval(id)}/deny`, { reason }),
  audit: (f: AuditFilter = {}) => call<AuditEvent[]>("GET", `/v1/access/audit${query(f)}`),
  /** The log as JSON lines (one event per line), for a download. */
  auditExport: async (f: AuditFilter = {}) => {
    const res = await fetch(`${ROLL_API}/v1/access/audit${query({ ...f, format: "jsonl" })}`, { headers: apiHeaders(), cache: "no-store" });
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    return res.blob();
  },
  /** The operator's rules for every token: at most `max_days`, and whether every token expires. */
  sinks: () => call<AuditSink[]>("GET", "/v1/access/audit/sinks"),
  addSink: (url: string, from_start = false) => call<{ sink: AuditSink; secret: string }>("POST", "/v1/access/audit/sinks", { url, from_start }),
  retrySink: (id: string) => call<null>("POST", `/v1/access/audit/sinks/${encodeURIComponent(id)}/retry`),
  removeSink: (id: string) => call<null>("DELETE", `/v1/access/audit/sinks/${encodeURIComponent(id)}`),
  policy: () => call<Policy>("GET", "/v1/access/policy"),
  setPolicy: (p: Policy) => call<Policy>("PUT", "/v1/access/policy", p),
  /** Where approvals are posted (Slack through roll code's chat ops), or `{chat: null}`. */
  notify: () => call<{ chat: ChatTarget | null }>("GET", "/v1/access/notify"),
  setNotify: (chat: ChatTarget | null) => call<{ chat: unknown }>("PUT", "/v1/access/notify", { chat }),
  client: (id: string) => call<OAuthClient>("GET", `/v1/access/oauth/clients/${encodeURIComponent(id)}`),
  /** The consent screen's answer: where to send the browser (the client's redirect, with a code). */
  authorize: (c: Consent) => call<{ redirect: string }>("POST", "/v1/access/oauth/authorize", c),
  /** A search cluster's indices by name (search's own route; it wakes the cluster), for the builder. */
  searchIndices: (cluster: string) => call<{ index: string }[]>("GET", `/v1/search/clusters/${encodeURIComponent(cluster)}/indices`),
};

// ── pure helpers (tested in access-api.test.ts) ──

/** A preset's rules for a project (`project` → that project; none picked → every resource). */
export function presetRules(p: Preset, project?: string): Rule[] {
  return p.rules.map((r) => ({
    ...r,
    resources: r.resources === "project" ? (project ? [{ project }] : "*") : r.resources,
  }));
}

/** Whether a preset needs a project to mean what it says. */
export const needsProject = (p: Preset) => p.rules.some((r) => r.resources === "project");

const PRODUCT_NAMES: Record<string, string> = {
  "*": "Everything",
  workspace: "Projects",
  postgres: "Postgres",
  redis: "Redis",
  storage: "Storage",
  streams: "Streams",
  nosql: "NoSQL",
  queues: "Queues",
  tasks: "Tasks",
  code: "Code",
  agents: "Agents",
  secrets: "Secrets",
  apps: "Apps",
  previews: "Previews",
  search: "Search",
  observability: "Logs & metrics",
  access: "Tokens & access",
};

export const productName = (p: string) => PRODUCT_NAMES[p] ?? p;

export const ACTION_HELP: Record<Action, string> = {
  read: "See metadata and data",
  write: "Create and change",
  delete: "Delete, drop, restore over",
  reveal: "See passwords, keys and secret values",
  deploy: "Land on main, deploy to production",
  admin: "Manage keys, users, settings and tokens",
};

/** "Postgres · read, write, delete (ask) · in project prj_x · development". */
export function describeRule(r: Rule, projectName: (id: string) => string = (id) => id, resourceName: (product: string, id: string) => string = (_, id) => id): string {
  const actions = ACTIONS.filter((a) => r.actions[a]).map((a) => (r.actions[a] === "ask" ? `${a} (ask)` : a));
  const where =
    r.resources === "*"
      ? "all resources"
      : r.resources
          .map((s) => ("project" in s ? `project ${projectName(s.project)}` : s.index ? `${resourceName(r.product, s.id)} indices ${s.index}` : resourceName(r.product, s.id)))
          .join(", ");
  const parts = [productName(r.product), actions.join(", ") || "nothing", where];
  if (r.environments?.length) parts.push(r.environments.join(", "));
  if (r.paths?.length) parts.push(r.paths.join(", "));
  return parts.join(" · ");
}

/** The strongest action a grant allows anywhere, for its badge. */
export function reach(g: Pick<Grant, "rules">): "read only" | "can change things" | "can destroy or ship" {
  const all = g.rules.flatMap((r) => Object.keys(r.actions) as Action[]);
  if (all.some((a) => a === "delete" || a === "deploy" || a === "admin" || a === "reveal")) return "can destroy or ship";
  if (all.some((a) => a === "write")) return "can change things";
  return "read only";
}

export type GrantStatus = "active" | "revoked" | "expired";

export function statusOf(g: Pick<Grant, "revoked_at" | "expires_at">, now = Date.now()): GrantStatus {
  if (g.revoked_at) return "revoked";
  if (g.expires_at !== null && g.expires_at <= now) return "expired";
  return "active";
}

/** Live tokens that expire within `days`, soonest first (GitHub mails a week before). */
export function expiringSoon<G extends Pick<Grant, "revoked_at" | "expires_at">>(grants: G[], now = Date.now(), days = 7): G[] {
  const until = now + days * 86_400_000;
  return grants
    .filter((g) => statusOf(g, now) === "active" && g.expires_at !== null && g.expires_at <= until)
    .sort((a, b) => (a.expires_at ?? 0) - (b.expires_at ?? 0));
}

/** Expiry choices in the builder, in ms from now (null: never). */
export const EXPIRIES: { label: string; ms: number | null }[] = [
  { label: "7 days", ms: 7 * 86_400_000 },
  { label: "30 days", ms: 30 * 86_400_000 },
  { label: "90 days", ms: 90 * 86_400_000 },
  { label: "1 year", ms: 365 * 86_400_000 },
  { label: "Never", ms: null },
];

/** Checked CIDR-ish entries (`10.0.0.0/8`, `203.0.113.4`, `2001:db8::/32`); the API checks for real. */
/** The most requests a minute a token may be allowed (roll-access's `MAX_RATE`). */
export const MAX_RATE = 100_000;

/** A typed requests-a-minute limit: empty is none. */
export function parseRate(text: string): { rate: number | null; problem?: string } {
  const t = text.trim();
  if (!t) return { rate: null };
  const n = Number(t);
  if (!Number.isInteger(n) || n < 1 || n > MAX_RATE) return { rate: null, problem: `Requests a minute is a whole number from 1 to ${MAX_RATE.toLocaleString("en")}` };
  return { rate: n };
}

export function ipProblems(list: string[]): string[] {
  const ok = (s: string) => /^[0-9a-fA-F:.]+(\/\d{1,3})?$/.test(s) && (s.includes(".") || s.includes(":"));
  return list.filter((s) => !ok(s)).map((s) => `${s} isn't an address or CIDR`);
}

// ── connecting clients ──

export const CLIENTS = ["claude-code", "cursor", "vscode", "claude-ai", "codex", "other"] as const;
export type McpClient = (typeof CLIENTS)[number];

export const CLIENT_NAMES: Record<McpClient, string> = {
  "claude-code": "Claude Code",
  cursor: "Cursor",
  vscode: "VS Code",
  "claude-ai": "Claude.ai & Desktop",
  codex: "Codex",
  other: "Any MCP client",
};

/** The MCP server's address for an API base. */
export const mcpUrl = (api: string = ROLL_API) => `${api.replace(/\/+$/, "")}/v1/mcp`;

/** How to connect a client: OAuth by default (no secret in the config), a bearer token with one. */
/** The MCP URL listing only these products' tools (`?toolsets=`); none: every product's. */
export function withToolsets(url: string, products: string[]): string {
  if (products.length === 0) return url;
  const u = new URL(url);
  u.searchParams.set("toolsets", [...products].sort().join(","));
  return u.toString().replace(/%2C/g, ",");
}

export function mcpConfig(client: McpClient, url: string, token?: string): { lang: "sh" | "json" | "toml" | "text"; text: string } {
  const header = token ? `Bearer ${token}` : undefined;
  const server = (wrap: "mcpServers" | "servers", typed: boolean) => {
    const s: Record<string, unknown> = typed ? { type: "http", url } : { url };
    if (header) s.headers = { Authorization: header };
    return JSON.stringify({ [wrap]: { roll: s } }, null, 2);
  };
  switch (client) {
    case "claude-code":
      return { lang: "sh", text: `claude mcp add --transport http roll ${url}${header ? ` --header "Authorization: ${header}"` : ""}` };
    case "cursor":
      return { lang: "json", text: server("mcpServers", false) };
    case "vscode":
      return { lang: "json", text: server("servers", true) };
    case "codex":
      return {
        lang: "toml",
        text: header
          ? `[mcp_servers.roll]\nurl = "${url}"\nbearer_token_env_var = "ROLL_TOKEN"`
          : `[mcp_servers.roll]\nurl = "${url}"\n# then: codex mcp login roll`,
      };
    case "claude-ai":
      return { lang: "text", text: `Settings → Connectors → Add custom connector\nName: roll\nURL: ${url}` };
    case "other":
      return { lang: "json", text: server("mcpServers", true) };
  }
}

/** Where each client's config goes. */
export const CLIENT_WHERE: Record<McpClient, string> = {
  "claude-code": "Run in a terminal. Claude Code opens the browser for you to approve the connection.",
  cursor: "~/.cursor/mcp.json (or .cursor/mcp.json in a project). Cursor asks you to sign in.",
  vscode: ".vscode/mcp.json in a workspace. VS Code asks you to sign in.",
  "claude-ai": "Claude.ai and Claude Desktop connect with OAuth: you approve the connection here.",
  codex: "~/.codex/config.toml.",
  other: "Streamable HTTP. OAuth 2.1 with dynamic registration, or Authorization: Bearer with a token.",
};

// ── OAuth consent (the dashboard's /oauth/authorize) ──

export interface AuthorizeRequest {
  client_id: string;
  redirect_uri: string;
  code_challenge: string;
  code_challenge_method: string;
  state?: string;
  resource?: string;
  scope?: string;
  response_type: string;
}

/**
 * The authorization request from `/oauth/authorize`'s query, or what's wrong with it. Errors
 * that can be sent back to a verified redirect are the API's to judge; these are the ones the
 * page shows (RFC 6749 §4.1.2.1: never redirect to an unchecked URI).
 */
export function parseAuthorize(search: Record<string, unknown>): AuthorizeRequest | { error: string } {
  const get = (k: string) => (typeof search[k] === "string" ? (search[k] as string) : undefined);
  const r = {
    client_id: get("client_id") ?? "",
    redirect_uri: get("redirect_uri") ?? "",
    code_challenge: get("code_challenge") ?? "",
    code_challenge_method: get("code_challenge_method") ?? "S256",
    state: get("state"),
    resource: get("resource"),
    scope: get("scope"),
    response_type: get("response_type") ?? "",
  };
  if (!r.client_id || !r.redirect_uri) return { error: "The request names no client or redirect." };
  if (r.response_type !== "code") return { error: "Only the authorization code flow is supported (response_type=code)." };
  if (!r.code_challenge || r.code_challenge_method !== "S256") return { error: "The client must use PKCE with S256." };
  return r;
}

/** Presets a client's `scope` never preselects: the person picks them on purpose. */
const NEVER_PRESELECTED = new Set(["full"]);

/**
 * The preset a client's `scope` asks for (the first known one), else the agent preset. A client
 * can't preselect full access: anyone can register a client, so what it asks for is a hint.
 */
export function presetFor(scope: string | undefined, presets: Preset[]): Preset | undefined {
  const asked = (scope ?? "").split(/\s+/).filter((s) => s && !NEVER_PRESELECTED.has(s));
  return presets.find((p) => asked.includes(p.id)) ?? presets.find((p) => p.id === "agent") ?? presets[0];
}

/** An error back to the client (the redirect was registered and the request came from it). */
export function denyRedirect(redirect: string, state?: string): string {
  const u = new URL(redirect);
  u.searchParams.set("error", "access_denied");
  u.searchParams.set("error_description", "The user denied the request");
  if (state) u.searchParams.set("state", state);
  return u.toString();
}

// ── the permission builder's model ──

/** What the builder edits: a product × action matrix, applied to every resource or one project. */
export interface Draft {
  /** Product ("*" for every product) → its actions. */
  matrix: Record<string, Partial<Record<Action, Effect>>>;
  project: string | null;
  environments: string[];
  /** Product → only these resources (its ids), instead of the project or every resource. */
  ids: Record<string, string[]>;
}

export const emptyDraft = (): Draft => ({ matrix: {}, project: null, environments: [], ids: {} });

const onlyIds = (r: Rule): string[] | null =>
  r.resources !== "*" && r.resources.length > 0 && r.resources.every((s) => "id" in s) ? r.resources.map((s) => ("id" in s ? selectorText(s) : "")) : null;

/**
 * The matrix for some rules, when they fit it (every rule on all resources or the same one
 * project, with the same environments and no paths); else null, and the builder shows them as
 * they are.
 */
export function draftOf(rules: Rule[]): Draft | null {
  const d = emptyDraft();
  if (rules.length === 0) return d;
  // Every rule shares the environments and has no paths; a product's rule may name its own
  // resources (ids); the others share one place (everything, or one project).
  const envs = (r: Rule) => JSON.stringify(r.environments ?? []);
  if (new Set(rules.map(envs)).size !== 1 || rules.some((r) => r.paths?.length)) return null;
  d.environments = rules[0].environments ?? [];
  const placed = rules.filter((r) => onlyIds(r) === null);
  if (new Set(placed.map((r) => JSON.stringify(r.resources))).size > 1) return null;
  const res = placed[0]?.resources;
  if (res && res !== "*") {
    if (res.length !== 1 || !("project" in res[0])) return null;
    d.project = res[0].project;
  }
  for (const r of rules) {
    const ids = onlyIds(r);
    if (ids) {
      if (r.product === "*" || d.ids[r.product] || rules.some((o) => o !== r && o.product === r.product)) return null;
      d.ids[r.product] = ids;
    }
    d.matrix[r.product] = { ...d.matrix[r.product], ...r.actions };
  }
  return d;
}

/** The rules a draft stands for: one per product that has any action. */
export function rulesOf(d: Draft): Rule[] {
  return Object.entries(d.matrix)
    .filter(([, a]) => Object.values(a).some(Boolean))
    .map(([product, actions]) => {
      const ids = product !== "*" ? d.ids[product] : undefined;
      const resources: Rule["resources"] = ids?.length ? ids.map((id) => selectorOf(product, id)) : d.project ? [{ project: d.project }] : "*";
      const r: Rule = { product, resources, actions: Object.fromEntries(Object.entries(actions).filter(([, e]) => e)) };
      if (d.environments.length) r.environments = d.environments;
      return r;
    });
}

/** A matrix cell's next state: off → allow → ask → off. */
export const nextEffect = (e: Effect | undefined): Effect | undefined => (e === undefined ? "allow" : e === "allow" ? "ask" : undefined);
