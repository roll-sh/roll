/**
 * Client for rolld's access API (`crates/roll-access`, docs/permissions.md "HTTP API"): scoped
 * tokens and OAuth connections, approvals, the audit log, the catalogue the builder offers, and
 * the OAuth consent screen's answer. Change both together.
 */

import { call, ROLL_API } from "./roll-api";

export const ACTIONS = ["read", "write", "delete", "reveal", "deploy", "admin"] as const;
export type Action = (typeof ACTIONS)[number];
export type Effect = "allow" | "ask";

export type Selector = { project: string } | { id: string };

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
}

export interface NewGrant {
  name: string;
  rules: Rule[];
  expires_at?: number | null;
  ip_allow?: string[];
}

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
}

const token = (id: string) => `/v1/access/tokens/${encodeURIComponent(id)}`;
const approval = (id: string) => `/v1/access/approvals/${encodeURIComponent(id)}`;

export interface AuditFilter {
  grant?: string;
  product?: string;
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

export const accessApi = {
  me: () => call<Me>("GET", "/v1/access"),
  catalog: () => call<Catalog>("GET", "/v1/access/catalog"),
  tokens: () => call<Grant[]>("GET", "/v1/access/tokens"),
  token: (id: string) => call<Grant>("GET", token(id)),
  /** The token is in the answer this once. */
  create: (g: NewGrant) => call<{ grant: Grant; token: string }>("POST", "/v1/access/tokens", g),
  update: (id: string, patch: Partial<NewGrant>) => call<Grant>("PATCH", token(id), patch),
  revoke: (id: string) => call<Grant>("DELETE", token(id)),
  /** A new secret, the same rules; the old one stops at once. Shown this once. */
  rotate: (id: string) => call<{ grant: Grant; token: string }>("POST", `${token(id)}/rotate`),
  /** What the evaluator would decide for a request, without running it (`token`: the operator asks for one). */
  check: (method: string, path: string, opts: { body?: unknown; token?: string } = {}) =>
    call<{ decision: "allow" | "ask" | "deny"; product?: string; action?: Action; resource?: string | null; route?: string; why?: string }>(
      "POST",
      "/v1/access/check",
      { method, path, ...opts },
    ),
  approvals: (state?: ApprovalState) => call<Approval[]>("GET", `/v1/access/approvals${query({ state })}`),
  approve: (id: string) => call<Approval>("POST", `${approval(id)}/approve`),
  deny: (id: string, reason?: string) => call<Approval>("POST", `${approval(id)}/deny`, { reason }),
  audit: (f: AuditFilter = {}) => call<AuditEvent[]>("GET", `/v1/access/audit${query(f)}`),
  client: (id: string) => call<OAuthClient>("GET", `/v1/access/oauth/clients/${encodeURIComponent(id)}`),
  /** The consent screen's answer: where to send the browser (the client's redirect, with a code). */
  authorize: (c: Consent) => call<{ redirect: string }>("POST", "/v1/access/oauth/authorize", c),
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
export function describeRule(r: Rule, projectName: (id: string) => string = (id) => id): string {
  const actions = ACTIONS.filter((a) => r.actions[a]).map((a) => (r.actions[a] === "ask" ? `${a} (ask)` : a));
  const where =
    r.resources === "*"
      ? "all resources"
      : r.resources.map((s) => ("project" in s ? `project ${projectName(s.project)}` : s.id)).join(", ");
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

/** Expiry choices in the builder, in ms from now (null: never). */
export const EXPIRIES: { label: string; ms: number | null }[] = [
  { label: "7 days", ms: 7 * 86_400_000 },
  { label: "30 days", ms: 30 * 86_400_000 },
  { label: "90 days", ms: 90 * 86_400_000 },
  { label: "1 year", ms: 365 * 86_400_000 },
  { label: "Never", ms: null },
];

/** Checked CIDR-ish entries (`10.0.0.0/8`, `203.0.113.4`, `2001:db8::/32`); the API checks for real. */
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
}

export const emptyDraft = (): Draft => ({ matrix: {}, project: null, environments: [] });

/**
 * The matrix for some rules, when they fit it (every rule on all resources or the same one
 * project, with the same environments and no paths); else null, and the builder shows them as
 * they are.
 */
export function draftOf(rules: Rule[]): Draft | null {
  const d = emptyDraft();
  const key = (r: Rule) => JSON.stringify([r.resources, r.environments ?? [], r.paths ?? []]);
  if (rules.length === 0) return d;
  if (new Set(rules.map(key)).size !== 1 || rules[0].paths?.length) return null;
  const res = rules[0].resources;
  if (res !== "*") {
    if (res.length !== 1 || !("project" in res[0])) return null;
    d.project = res[0].project;
  }
  d.environments = rules[0].environments ?? [];
  for (const r of rules) d.matrix[r.product] = { ...d.matrix[r.product], ...r.actions };
  return d;
}

/** The rules a draft stands for: one per product that has any action. */
export function rulesOf(d: Draft): Rule[] {
  return Object.entries(d.matrix)
    .filter(([, a]) => Object.values(a).some(Boolean))
    .map(([product, actions]) => {
      const r: Rule = { product, resources: d.project ? [{ project: d.project }] : "*", actions: Object.fromEntries(Object.entries(actions).filter(([, e]) => e)) };
      if (d.environments.length) r.environments = d.environments;
      return r;
    });
}

/** A matrix cell's next state: off → allow → ask → off. */
export const nextEffect = (e: Effect | undefined): Effect | undefined => (e === undefined ? "allow" : e === "allow" ? "ask" : undefined);
