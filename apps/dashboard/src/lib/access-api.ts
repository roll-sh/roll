/**
 * Client for rolld's access API (`crates/roll-access`, docs/permissions.md "HTTP API"): scoped
 * tokens and OAuth connections, approvals, the audit log, the catalogue the builder offers, and
 * the OAuth consent screen's answer. Change both together.
 */

import { apiHeaders, call, ROLL_API } from "./roll-api";

export const ACTIONS = ["read", "write", "delete", "reveal", "deploy", "admin"] as const;
export type Action = (typeof ACTIONS)[number];
export type Effect = "allow" | "ask";

/** `index`: only some named parts of it (search's indices, `logs-*,a`; secrets' encryption keys). */
export type Selector = { project: string } | { id: string; index?: string };

/** What a product's resources narrow to by name (`{id, index}`), as roll-auth's `sub_resources`. */
export const SUB_RESOURCES: Record<string, string> = { search: "indices", secrets: "encryption keys" };

/**
 * A selector as the builder writes it: an id, or `id/names` where the product has named parts
 * (`cl_1/logs-*`, `prj_1/payments`; neither an id nor such a name has a `/`).
 */
export const selectorText = (s: { id: string; index?: string }): string => (s.index ? `${s.id}/${s.index}` : s.id);
export function selectorOf(product: string, text: string): { id: string; index?: string } {
  const at = SUB_RESOURCES[product] ? text.indexOf("/") : -1;
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

/**
 * The rule a refusal proposed (`?add=product:action&id=…&index=…`: the `widen` link roll answers a
 * refused request with), as `effect` (allow, or ask each time): one action, on that resource (or
 * the product's every resource when none was named). `null` for anything that isn't one.
 */
export function proposedRule(search: string, effect: Effect): Rule | null {
  const q = new URLSearchParams(search);
  const [product, action] = (q.get("add") ?? "").split(":");
  if (!product || !/^[a-z][a-z0-9_-]{0,31}$/.test(product) || !(ACTIONS as readonly string[]).includes(action)) return null;
  const id = q.get("id");
  const index = q.get("index");
  const resources: Rule["resources"] = id ? [index ? { id, index } : { id }] : "*";
  return { product, resources, actions: { [action as Action]: effect } };
}

export interface OAuthClient {
  id: string;
  name: string;
  uri?: string;
  redirect_uris: string[];
  created_at: number;
  last_used_at: number | null;
}

/** A token's MCP offer: its tools, its prompts and what the list costs an agent's context. */
export interface AgentSees {
  tools: { name: string; title: string; product: string; read_only: boolean; destructive: boolean }[];
  prompts: { name: string; title: string; description: string }[];
  context_tokens?: number;
  live: boolean;
}

export interface Grant {
  id: string;
  name: string;
  /** `workload`: a short-lived token a workload got by proving who it is (roll secrets' identity logins). */
  kind: "token" | "oauth" | "workload";
  rules: Rule[];
  expires_at: number | null;
  /** It works only from then on (Cloudflare's TTL start); before, it's refused. */
  not_before?: number | null;
  /** A person's token waiting for an admin's approval: it doesn't work until then. */
  pending?: boolean;
  /** When an admin approved it, and who decided (an admin's id, or "operator"). */
  approved_at?: number | null;
  decided_by?: string | null;
  /** What its secret looks like (`rlt_…a1b2`), to match one found in a config to this row. */
  hint?: string | null;
  /** The person it's theirs (`usr_…`), when it isn't the operator's. */
  user?: string | null;
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
  not_before?: number | null;
  ip_allow?: string[];
  rate_per_minute?: number | null;
  ask_otherwise?: boolean;
}

/** A token that needs attention (`GET /v1/access/checkup`). */
export interface Checkup {
  grant: { id: string; name: string; kind: Grant["kind"]; last_used_at: number | null; expires_at: number | null; created_at: number };
  /** Most serious first. */
  findings: {
    kind: "pending" | "unapproved" | "everything" | "never_expires" | "secret_client" | "unused" | "narrower" | "roams";
    why: string;
    fix: "approve" | "tighten" | "expire" | "revoke" | "limit_addresses" | "rotate_client";
  }[];
}

/** Checkup findings, most serious first. */
const FINDING_ORDER: Checkup["findings"][number]["kind"][] = ["pending", "unapproved", "everything", "never_expires", "secret_client", "unused", "roams", "narrower"];

/** The checkup's tokens, the most serious first (by each one's worst finding, then how many). */
export function byConcern(list: Checkup[]): Checkup[] {
  const worst = (c: Checkup) => Math.min(...c.findings.map((f) => FINDING_ORDER.indexOf(f.kind)));
  return [...list].sort((a, b) => worst(a) - worst(b) || b.findings.length - a.findings.length);
}

/** What a checkup fix is called on its button. */
export const FIX_LABEL: Record<Checkup["findings"][number]["fix"], string> = {
  approve: "Approve or deny",
  tighten: "Tighten",
  expire: "Give it an expiry",
  revoke: "Revoke",
  limit_addresses: "Limit addresses",
  rotate_client: "Rotate to a key",
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

/** Whether approving it can be remembered: it's about one resource, and roll's policy doesn't ask several people for it. */
export const rememberable = (a: Pick<Approval, "resource" | "gates"> & { method?: string; args?: unknown }) => Boolean(a.resource && !a.resource.startsWith("new in ")) && !a.gates?.length && !accessRequest(a);

/** An access request (a token asking for one more rule for a while): how long and why, or null for a request waiting to run. */
export function accessRequest(a: { method?: string; args?: unknown }): { for: string; reason: string } | null {
  if (a.method !== "GRANT") return null;
  const args = (a.args ?? {}) as { for?: unknown; reason?: unknown };
  return { for: String(args.for ?? ""), reason: String(args.reason ?? "") };
}

const FOR_LABEL: Record<string, string> = { "15m": "15 minutes", "1h": "an hour", "8h": "8 hours", "1d": "a day" };
export const forLabel = (f: string) => FOR_LABEL[f] ?? f;

/** The name an approval records for whoever is looking (`operator`, or a person's `usr_…`). */
export function deciderName(me: Me | null): string | null {
  if (!me) return null;
  if (me.kind === "operator") return me.user?.id ?? "operator";
  return me.kind === "user" ? me.user.id : null;
}

/** One rule of roll's policy a pending approval must pass: so many approvals from whom, so many so far. */
export interface Gate {
  approvals: number;
  approvers?: string[];
  approved: number;
}

/** What a pending approval still needs under roll's policy ("approved by usr_bob · 1 more needed"), or null. */
export function gateProgress(a: Pick<Approval, "gates" | "approved_by">): string | null {
  const gates = a.gates ?? [];
  if (!gates.length) return null;
  const more = gates.reduce((n, g) => n + Math.max(0, g.approvals - g.approved), 0);
  const from = [...new Set(gates.flatMap((g) => g.approvers ?? []))];
  const by = a.approved_by?.length ? `approved by ${a.approved_by.join(", ")} · ` : "";
  const needs = more === 1 ? "1 more approval needed" : `${more} more approvals needed`;
  return `${by}${needs}${from.length ? ` (from ${from.join(", ")})` : ""}`;
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
  /** Who approved it so far, in order (several when roll's policy asks for more than one). */
  approved_by?: string[];
  /** While pending: what roll's policy asks of it. */
  gates?: Gate[];
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
  /** A request's grant (`null`: the operator's); for a change (`source: "access"`), the token it's about. */
  grant_id: string | null;
  grant_name: string | null;
  /** A request (`api`, `mcp`), a change to tokens and access (`access`: who in `args.by`), or roll's own act (`leak`, `workload`). */
  source: "api" | "mcp" | "access" | "leak" | "workload" | "accounts";
  tool: string | null;
  method: string;
  /** The route pattern, never the path. */
  route: string;
  product: string | null;
  action: Action | null;
  resource: string | null;
  /** A request's `allow`, `deny` or `ask`; a change's name (`create`, `change`, `rotate`, `revoke`, `connect`, `approve`, `policy`, …). */
  decision: string;
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

export type Me =
  /** The operator's token, or a signed-in admin (`user`), who acts as the operator. */
  | { kind: "operator"; user?: { id: string; handle: string }; locked: boolean }
  | { kind: "token" | "oauth"; grant: Grant; locked: boolean }
  /** A signed-in user who isn't an admin: their tokens act as them and reach only `reach`. */
  | { kind: "user"; user: { id: string; handle: string }; reach: string[]; approve_people?: Action[]; locked: boolean };

/** Whether a token `me` makes with these rules waits for an admin (the server's `Policy::needs_approval`). */
export function waitsForAdmin(me: Me | null, rules: Rule[], askOtherwise: boolean): boolean {
  const approve = me?.kind === "user" ? (me.approve_people ?? []) : [];
  return approve.length > 0 && (askOtherwise || rules.some((r) => (Object.keys(r.actions) as Action[]).some((a) => approve.includes(a))));
}

/** The catalogue as `me` may use it: a user's tokens reach only what their account does. */
export function catalogFor(c: Catalog, me: Me | null): Catalog {
  if (me?.kind !== "user") return c;
  return { ...c, products: c.products.filter((p) => me.reach.includes(p)) };
}

/** How long a rotated token's old secret may keep working, in seconds (0: it stops at once). */
export const KEEP_OLD: { label: string; s: number }[] = [
  { label: "stops now", s: 0 },
  { label: "works 1 hour", s: 3600 },
  { label: "works 1 day", s: 86_400 },
  { label: "works 7 days", s: 604_800 },
];

export interface Policy {
  max_days?: number | null;
  require_expiry?: boolean;
  /** Tokens unused this many days stop working. */
  max_idle_days?: number | null;
  /** A person's token reaching any of these waits for an admin's approval (GitHub's organizations' rule). */
  approve_people?: Action[];
  /** What needs more than one approver, and whose (each rule covering a request is a gate). */
  approval_rules?: ApprovalRule[];
}

/** A rule of the policy: requests it covers need `approvals` approvals from `approvers` (anyone who may decide when left out), never from the token's own person. */
export interface ApprovalRule {
  /** A product, or "*". */
  product: string;
  /** Every action when left out. */
  actions?: Action[];
  /** Every environment when left out (pv-* covers pv-42). */
  environments?: string[];
  approvals: number;
  approvers?: string[];
}

/** "Postgres · delete · production: 2 approvals (from usr_a, usr_b)". */
export function describeApprovalRule(r: ApprovalRule): string {
  const what = [productName(r.product), r.actions?.length ? r.actions.join(", ") : "anything", r.environments?.join(", ")].filter(Boolean).join(" · ");
  const n = r.approvals === 1 ? "1 approval" : `${r.approvals} approvals`;
  return `${what}: ${n}${r.approvers?.length ? ` from ${r.approvers.join(", ")}` : ""}`;
}

/** The actions an admin may require approval for in people's tokens, strongest first. */
export const APPROVABLE: Action[] = ["admin", "reveal", "deploy", "delete"];

/** What a person allows a client: on the consent screen, or for a device signing in. */
export interface Allowed {
  name?: string;
  rules: Rule[];
  expires_at?: number | null;
  ip_allow?: string[];
  rate_per_minute?: number | null;
  ask_otherwise?: boolean;
}

export interface Consent extends Allowed {
  client_id: string;
  redirect_uri: string;
  code_challenge: string;
  code_challenge_method?: string;
  state?: string;
  resource?: string;
}

/** A device signing in (RFC 8628), as the person who entered its code sees it. */
export interface DeviceRequest {
  user_code: string;
  client: { id: string; name: string; uri?: string | null };
  scope?: string | null;
  /** The network it asked from (`203.0.113.0/24`), to tell it isn't yours. */
  network?: string | null;
  /** Whether it asked from this browser's network: when not, someone may have sent you the code. */
  same_network?: boolean;
  created_at: number;
  expires_at: number;
}

const token = (id: string) => `/v1/access/tokens/${encodeURIComponent(id)}`;
const approval = (id: string) => `/v1/access/approvals/${encodeURIComponent(id)}`;
const device = (code: string) => `/v1/access/oauth/devices/${encodeURIComponent(code.trim())}`;

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
  /** An approver's roll account: they decide as that person (they count toward the policy's approval rules). */
  people?: Record<string, string>;
}

/** Approvers as typed (`U0123=usr_ana, U0456`): the chat ids, and the accounts some are linked to. */
export function parseApprovers(text: string): { approvers: string[]; people: Record<string, string> } {
  const approvers: string[] = [];
  const people: Record<string, string> = {};
  for (const raw of text.split(/[\s,]+/).map((s) => s.trim().replace(/^@/, "")).filter(Boolean)) {
    const [id, user] = raw.split("=");
    if (!approvers.includes(id)) approvers.push(id);
    if (user) people[id] = user;
  }
  return { approvers, people };
}

/** The approvers as the form shows them (`U0123=usr_ana`). */
export const approversText = (c: Pick<ChatTarget, "approvers" | "people">) => c.approvers.map((a) => (c.people?.[a] ? `${a}=${c.people[a]}` : a)).join(", ");

/**
 * A machine client (docs/permissions.md, "Machine clients"): automation with no person that trades
 * a JWT its key signed, or a secret, for an hour's token held to its grant's rules.
 */
export interface MachineClient {
  client_id: string;
  signs_with_key: boolean;
  /** The identity it trusts (CI's own OIDC tokens), when that's its credential. */
  trusts?: TrustedIdentity | null;
  /** When it last got a token (Unix ms). */
  last_sign_in?: number | null;
  grant: Grant;
}

/** OIDC tokens a machine client trusts: from `issuer`, for `audience`, `sub` matching `subject`. */
export interface TrustedIdentity {
  issuer: string;
  audience?: string;
  subject: string;
  claims?: Record<string, string>;
  /** How many keys it pins (an issuer roll can't reach), in a listing. */
  pinned_keys?: number | null;
}

export const GITHUB_ISSUER = "https://token.actions.githubusercontent.com";

/**
 * GitHub Actions as a trusted identity: a repository's jobs, on one ref or in one environment
 * (GitHub's `sub`: `repo:<owner>/<repo>:ref:<ref>`, `…:environment:<name>`), or any of its jobs.
 */
export function githubIdentity(repo: string, ref = "", environment = ""): { identity: TrustedIdentity } | { problem: string } {
  const m = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/.exec(repo.trim());
  if (!m) return { problem: "The repository is owner/name" };
  const base = `repo:${m[1]}/${m[2]}`;
  const subject = environment.trim() ? `${base}:environment:${environment.trim()}` : ref.trim() ? `${base}:ref:${ref.trim()}` : `${base}:*`;
  return { identity: { issuer: GITHUB_ISSUER, subject } };
}

/** What a trusted identity is, in words: GitHub's repository and ref or environment, or its issuer. */
export function trustedLabel(t: TrustedIdentity): string {
  const gh = t.issuer === GITHUB_ISSUER && /^repo:([^:]+)(?::(ref|environment):(.+)|:\*)?$/.exec(t.subject);
  if (gh) return `GitHub Actions in ${gh[1]}${gh[2] === "ref" ? ` on ${gh[3]}` : gh[2] === "environment" ? ` (${gh[3]})` : ""}`;
  return `${t.issuer.replace(/^https:\/\//, "")}: ${t.subject}`;
}

/** The workflow step that trades a job's OIDC token for a roll token, masked, as `ROLL_TOKEN`. */
export function githubStep(clientId: string, tokenEndpoint: string): string {
  const audience = tokenEndpoint.replace(/\/v1\/access\/oauth\/token$/, "");
  return [
    "# the job needs: permissions: { id-token: write }",
    "- name: Sign in to roll",
    "  run: |",
    `    oidc=$(curl -sSf -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=${audience}" | jq -r .value)`,
    `    token=$(curl -sSf ${tokenEndpoint} -d grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer -d client_id=${clientId} --data-urlencode "assertion=$oidc" | jq -r .access_token)`,
    '    echo "::add-mask::$token"',
    '    echo "ROLL_TOKEN=$token" >> "$GITHUB_ENV"',
  ].join("\n");
}

export interface NewMachineClient extends Omit<NewGrant, "not_before" | "ask_otherwise"> {
  /** Its public key: none (and no identity), and roll makes a secret (shown once). */
  public_jwk?: Record<string, string> | null;
  /** The identity it trusts instead (CI's own OIDC tokens). */
  oidc?: TrustedIdentity | null;
}

/** A new machine client: its secret is in the answer this once, when it has no key. */
export interface MadeMachineClient {
  client_id: string;
  grant: Grant;
  token_endpoint: string;
  client_secret?: string;
  /** Set here, not by the API: it trusts GitHub Actions, so the panel shows the workflow step. */
  github?: boolean;
}

/**
 * A pasted public key as a JWK: a JWK as it is, or a PEM `PUBLIC KEY` (what `openssl pkey -pubout`
 * writes) for Ed25519 or P-256, whose encodings are a fixed prefix and then the key's bytes. The
 * API checks the key for real; this only turns a PEM into what it takes. RSA keys go as JWKs.
 */
export function publicJwk(text: string): { jwk: Record<string, string> } | { problem: string } {
  const t = text.trim();
  if (t.startsWith("{")) {
    try {
      const jwk = JSON.parse(t);
      return jwk && typeof jwk === "object" && !Array.isArray(jwk) ? { jwk } : { problem: "A JWK is a JSON object" };
    } catch {
      return { problem: "That JWK isn't valid JSON" };
    }
  }
  if (t.includes("PRIVATE KEY")) return { problem: "That's a private key: paste only its public part (openssl pkey -in key.pem -pubout)" };
  let der: Uint8Array;
  try {
    der = Uint8Array.from(atob(t.split("\n").filter((l) => !l.startsWith("-----")).join("").trim()), (c) => c.charCodeAt(0));
  } catch {
    return { problem: "Paste a PEM public key or a JWK" };
  }
  const url = (b: Uint8Array) => btoa(String.fromCharCode(...b)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  const after = (prefix: number[], len: number) =>
    der.length === prefix.length + len && prefix.every((b, i) => der[i] === b) ? der.slice(prefix.length) : null;
  const ed = after([0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00], 32);
  if (ed) return { jwk: { kty: "OKP", crv: "Ed25519", x: url(ed) } };
  const ec = after([0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04], 64);
  if (ec) return { jwk: { kty: "EC", crv: "P-256", x: url(ec.slice(0, 32)), y: url(ec.slice(32)) } };
  return { problem: "An Ed25519 or P-256 PEM public key (for RSA, paste its JWK)" };
}

/**
 * An identity provider roll trusts to vouch for its people (docs/permissions.md, "Enterprise
 * identity providers"): an app trades the provider's ID-JAG for a token acting as the person.
 */
export interface EnterpriseIdp {
  issuer: string;
  /** Its keys pinned (a JWKS document), for a provider roll can't reach. */
  jwks?: string;
  /** `email` (verified, in one of `domains`) or `aud_sub` (the assertion names the `usr_…`). */
  matching?: "email" | "aud_sub";
  domains?: string[];
  /** Whether it may sign in roll's admins. */
  admins?: boolean;
  name?: string;
  /** A multi-tenant issuer's one tenant roll trusts (its assertions' `tenant`). */
  tenant?: string;
  /** The SSO connection that is this same provider (`sso_…`): its verified domains, as they are at each sign-in. */
  sso?: string;
}

/** An app a trusted provider may vouch for: never its secret. */
export interface EnterpriseApp {
  client_id: string;
  name: string;
  rules: Rule[];
  signs_with_key: boolean;
  /** Until when the secret before a rotation still works (Unix ms). */
  old_until?: number | null;
  /** Where its secret was found leaked: roll dropped it, and it signs in again once rotated. */
  leaked?: string | null;
  created_at: number;
}

export interface Enterprise {
  idps: EnterpriseIdp[];
  apps: EnterpriseApp[];
  /** What a provider puts in an assertion's `aud`: roll's API URL. */
  audience: string;
  token_endpoint: string;
}

/** The trust-a-provider form as a provider, or what's wrong with it (the API checks it again). */
export function idpFromForm(f: { issuer: string; name: string; matching: "email" | "aud_sub"; domains: string; jwks: string; admins: boolean; tenant?: string; sso?: string }): { idp: EnterpriseIdp } | { problem: string } {
  const issuer = f.issuer.trim();
  if (!/^https:\/\/[^\s/]+/.test(issuer)) return { problem: "The issuer is an https URL" };
  const sso = f.sso?.trim() || undefined;
  // A provider tied to an SSO connection vouches for the domains a DNS record proved for it.
  const domains = sso ? [] : [...new Set(f.domains.split(/[\s,]+/).map((d) => d.trim().toLowerCase().replace(/^@/, "")).filter(Boolean))];
  const bad = domains.find((d) => !/^[a-z0-9-]+(\.[a-z0-9-]+)+$/.test(d));
  if (bad) return { problem: `${bad} isn't a domain` };
  if (f.matching === "email" && domains.length === 0 && !sso) return { problem: "Name the email domains it vouches for" };
  const jwks = f.jwks.trim();
  if (jwks) {
    try {
      const doc = JSON.parse(jwks);
      if (!Array.isArray(doc?.keys)) return { problem: "Pinned keys are a JWKS document ({\"keys\": [...]})" };
    } catch {
      return { problem: "The pinned keys aren't valid JSON" };
    }
  }
  return {
    idp: {
      issuer,
      matching: f.matching,
      ...(domains.length ? { domains } : {}),
      ...(jwks ? { jwks } : {}),
      ...(f.admins ? { admins: true } : {}),
      ...(f.name.trim() ? { name: f.name.trim() } : {}),
      ...(f.tenant?.trim() ? { tenant: f.tenant.trim() } : {}),
      ...(sso ? { sso } : {}),
    },
  };
}

export const accessApi = {
  me: () => call<Me>("GET", "/v1/access"),
  /** The operator's machine clients, newest first (a 403 for anyone else). */
  machineClients: () => call<MachineClient[]>("GET", "/v1/access/clients"),
  createMachineClient: (c: NewMachineClient) => call<MadeMachineClient>("POST", "/v1/access/clients", c),
  /** A new key (or, without one, a new secret, in the answer this once), keeping the id. */
  rotateMachineClient: (id: string, r: { public_jwk?: Record<string, string> | null; oidc?: TrustedIdentity | null; keep_old_for_s?: number }) =>
    call<{ grant: Grant; client_secret?: string }>("POST", `/v1/access/clients/${encodeURIComponent(id)}/rotate`, r),
  /** The operator's enterprise identity providers and apps (a 403 for anyone else). */
  enterprise: () => call<Enterprise>("GET", "/v1/access/enterprise"),
  /** Every trusted provider at once: the list replaces the old one. */
  setIdps: (idps: EnterpriseIdp[]) => call<{ idps: EnterpriseIdp[] }>("PUT", "/v1/access/enterprise/idps", { idps }),
  /** A new app: with no key, its secret is in the answer this once. */
  addEnterpriseApp: (a: { name: string; rules: Rule[]; public_jwk?: Record<string, string> | null }) =>
    call<EnterpriseApp & { client_secret?: string }>("POST", "/v1/access/enterprise/apps", a),
  /** A new name or rules: its people's connections take the rules at once. */
  updateEnterpriseApp: (id: string, c: { name?: string; rules?: Rule[] }) => call<EnterpriseApp>("PATCH", `/v1/access/enterprise/apps/${encodeURIComponent(id)}`, c),
  /** A new key, or a new secret in the answer this once; the old secret kept a while or stopped now. */
  rotateEnterpriseApp: (id: string, r: { public_jwk?: Record<string, string> | null; keep_old_for_s?: number }) =>
    call<EnterpriseApp & { client_secret?: string }>("POST", `/v1/access/enterprise/apps/${encodeURIComponent(id)}/rotate`, r),
  /** Removes an app and revokes every person's connection to it. */
  removeEnterpriseApp: (id: string) => call<{ revoked: number }>("DELETE", `/v1/access/enterprise/apps/${encodeURIComponent(id)}`),
  catalog: () => call<Catalog>("GET", "/v1/access/catalog"),
  tokens: () => call<Grant[]>("GET", "/v1/access/tokens"),
  /** The latest workload tokens (they're not in `tokens`). */
  workloads: () => call<Grant[]>("GET", "/v1/access/tokens?kind=workload"),
  token: (id: string) => call<Grant>("GET", token(id)),
  /** The token is in the answer this once. */
  create: (g: NewGrant) => call<{ grant: Grant; token: string }>("POST", "/v1/access/tokens", g),
  update: (id: string, patch: Partial<NewGrant>) => call<Grant>("PATCH", token(id), patch),
  revoke: (id: string) => call<Grant>("DELETE", token(id)),
  /** The live tokens that need attention, each with its findings and their fixes. */
  checkup: () => call<Checkup[]>("GET", "/v1/access/checkup"),
  /** The networks it's been used from (/24s and /48s), newest first. */
  networks: (id: string) => call<{ network: string; first_at: number; last_at: number }[]>("GET", `${token(id)}/networks`),
  /** What an agent connected with this token is offered over MCP (`tools/list`, `prompts/list`). */
  agentSees: (id: string) => call<AgentSees>("GET", `${token(id)}/tools`),
  /** Take back what an approval allowed it for a while (and the approval, if unused). */
  forget: (id: string, approval: string) => call<Grant>("DELETE", `${token(id)}/temporary/${encodeURIComponent(approval)}`),
  /** Its rules narrowed to what its allowed requests used in the last `days` (never wider): a proposal. */
  tighten: (id: string, days?: number) => call<Tightened>("GET", `${token(id)}/tighten${query({ days })}`),
  /** Apply it, made again now, if its rules are still `from` (else 409: look again). */
  applyTighten: (id: string, from: Rule[], days?: number) => call<Grant>("POST", `${token(id)}/tighten`, { from, days }),
  /** A new secret, the same rules; the old one stops at once. Shown this once. */
  /** A new secret; `keepOldS`: the old one keeps working that long (at most a week), as Stripe's roll key. */
  rotate: (id: string, keepOldS?: number) => call<{ grant: Grant; token: string }>("POST", `${token(id)}/rotate`, keepOldS ? { keep_old_for_s: keepOldS } : {}),
  /** An admin decides a person's pending token: approved it works, denied it's revoked. */
  approveToken: (id: string) => call<Grant>("POST", `${token(id)}/approve`),
  denyToken: (id: string) => call<Grant>("POST", `${token(id)}/deny`),
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
  /** The log as CSV (a spreadsheet) or JSON lines (a log tool), for a download. */
  auditExport: async (f: AuditFilter = {}, format: "csv" | "jsonl" = "csv") => {
    const res = await fetch(`${ROLL_API}/v1/access/audit${query({ ...f, format })}`, { headers: apiHeaders(), cache: "no-store" });
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
  /** A device's request by the code its person typed (any case, with or without the dash). */
  device: (code: string) => call<DeviceRequest>("GET", device(code)),
  allowDevice: (code: string, a: Allowed) => call<{ grant_id: string; pending: boolean }>("POST", device(code), a),
  denyDevice: (code: string) => call<null>("DELETE", device(code)),
  /** A search cluster's indices by name (search's own route; it wakes the cluster), for the builder. */
  /** Each product's tool count and context cost, for the connect panel (counts only). */
  toolsetCosts: () => call<ToolsetCosts>("GET", "/v1/mcp/toolsets"),
  searchIndices: (cluster: string) => call<{ index: string }[]>("GET", `/v1/search/clusters/${encodeURIComponent(cluster)}/indices`),
  /** A secrets project's encryption keys by name (secrets' own route), for the builder. */
  encryptionKeys: (project: string) => call<{ name: string }[]>("GET", `/v1/secrets/projects/${encodeURIComponent(project)}/transit`),
};

// ── pure helpers (tested in access-api.test.ts) ──

/**
 * Whether a token matches what someone typed in the list's filter: every word in its name, id,
 * hint, client, person or the products it reaches. A whole secret pasted in matches the token it
 * is by its hint (prefix and last four), so a token found in a config finds its row; it's only
 * compared here, never sent.
 */
export function grantMatches(g: Pick<Grant, "name" | "id" | "hint" | "client" | "user" | "created_by" | "rules">, typed: string): boolean {
  const q = typed.trim().toLowerCase();
  if (!q) return true;
  if (/^rl[a-z]_[a-z2-7]{30,}$/.test(q)) return Boolean(g.hint) && g.hint === `${q.slice(0, q.indexOf("_") + 1)}…${q.slice(-4)}`;
  const hay = [g.name, g.id, g.hint ?? "", g.client?.name ?? "", g.user ?? "", g.created_by, ...g.rules.map((r) => r.product)].join(" ").toLowerCase();
  return q.split(/\s+/).every((w) => hay.includes(w));
}

/** Who an audit row is by: a change's person (`args.by`), else a request's grant, else the operator. */
export function auditWho(e: AuditEvent): string {
  const args = e.args && typeof e.args === "object" ? (e.args as { by?: unknown; actor?: unknown }) : {};
  // People's accounts (docs/accounts.md, "Audit"): who acted (a person's id, operator, scim:<connection>).
  if (e.source === "accounts" && typeof args.actor === "string") return args.actor;
  const by = e.source === "access" ? args.by : undefined;
  if (typeof by === "string") return by;
  return e.source === "access" ? "operator" : (e.grant_name ?? "operator");
}

/** What an audit row did: a tool, a request's route, or a change and the token it's about. */
export function auditWhat(e: AuditEvent): string {
  if (e.tool) return e.tool;
  // An account event: its action (`sign_in`, `scim.update`) or its request.
  if (e.source === "accounts") return e.method === "-" ? e.route : `${e.method} ${e.route.replace(/^[A-Z]+ /, "")}`;
  return e.source === "access" && e.grant_name ? `${e.method} ${e.route} · ${e.grant_name}` : `${e.method} ${e.route}`;
}

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
  accounts: "Accounts",
};

export const productName = (p: string) => PRODUCT_NAMES[p] ?? p;

/** Every product a rule can name ("*" first). */
export const PRODUCTS = Object.keys(PRODUCT_NAMES);

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
          .map((s) => ("project" in s ? `project ${projectName(s.project)}` : s.index ? `${resourceName(r.product, s.id)} ${SUB_RESOURCES[r.product] ?? "parts"} ${s.index}` : resourceName(r.product, s.id)))
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

/** When a live token starts working, if that's still ahead. */
export const startsLater = (g: Pick<Grant, "not_before">, now = Date.now()): number | null => (g.not_before && g.not_before > now ? g.not_before : null);

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

/**
 * A one-click install for clients that take one (Cursor's and VS Code's deep links), as Supabase's,
 * Linear's and Stripe's MCP pages offer: the server's URL only, so the client signs in with OAuth
 * and the person chooses what it may do. Never with a token in it: a link lands in the browser's
 * history and the OS's handlers.
 */
export function installLink(client: McpClient, url: string): { href: string; label: string } | null {
  if (client === "cursor") {
    const config = btoa(JSON.stringify({ url }));
    return { href: `cursor://anysphere.cursor-deeplink/mcp/install?name=roll&config=${encodeURIComponent(config)}`, label: "Add to Cursor" };
  }
  if (client === "vscode") {
    return { href: `vscode:mcp/install?${encodeURIComponent(JSON.stringify({ name: "roll", type: "http", url }))}`, label: "Install in VS Code" };
  }
  return null;
}
export type McpClient = (typeof CLIENTS)[number];

export const CLIENT_NAMES: Record<McpClient, string> = {
  "claude-code": "Claude Code",
  cursor: "Cursor",
  vscode: "VS Code",
  "claude-ai": "Claude.ai & Desktop",
  codex: "Codex",
  other: "Any MCP client",
};

/** One product's tools as `GET /v1/mcp/toolsets` counts them (the whole list, no narrowing). */
export type ToolsetCost = { product: string; tools: number; read_only: number; context_tokens: number };
/** Every product's costs, and search mode's own tools (`?tools=search`: find, run read-only, run). */
export type ToolsetCosts = { products: ToolsetCost[]; search?: Omit<ToolsetCost, "product"> };

/**
 * Tools a client can take in one request. VS Code: 128, its own built-in tools included (its
 * docs, "Use tools with agents"). Cursor: reported to drop tools past about 40.
 */
export const CLIENT_TOOL_LIMIT: Partial<Record<McpClient, { tools: number; note: string }>> = {
  vscode: { tools: 128, note: "VS Code refuses a chat request with more than 128 tools, its own built-in ones included" },
  cursor: { tools: 40, note: "Cursor is reported to drop tools past about 40" },
};

/**
 * What a choice of toolsets lists (none picked: every product; the access tools always). In search
 * mode (`search` given) the products' tools are behind search mode's own instead.
 */
export function toolsetCost(costs: ToolsetCost[], picked: string[], readonly: boolean, search?: Omit<ToolsetCost, "product">): { tools: number; tokens: number } {
  const listed = [
    ...costs.filter((c) => c.product === "access" || (!search && (picked.length === 0 || picked.includes(c.product)))),
    ...(search ? [{ product: "search mode", ...search }] : []),
  ];
  const tools = listed.reduce((n, c) => n + (readonly ? c.read_only : c.tools), 0);
  // Read-only lists fewer tools: their share of the product's cost, near enough.
  const tokens = listed.reduce((n, c) => n + (readonly && c.tools ? Math.round((c.context_tokens * c.read_only) / c.tools) : c.context_tokens), 0);
  return { tools, tokens };
}

/** The MCP server's address for an API base. */
export const mcpUrl = (api: string = ROLL_API) => `${api.replace(/\/+$/, "")}/v1/mcp`;

/** How to connect a client: OAuth by default (no secret in the config), a bearer token with one. */
/** How a connection narrows what the server offers it (docs/mcp.md). */
export type McpMode = {
  /** Only tools that read are listed and run (the token's rules still decide). */
  readonly?: boolean;
  /** A search tool and two run tools instead of the whole list (`?tools=search`). */
  search?: boolean;
  /** Text written by people who can't push to a repo is withheld from the agent (`?lockdown=1`). */
  lockdown?: boolean;
};

/** The MCP URL listing only these products' tools (`?toolsets=`; none: every product's), in a mode. */
export function withToolsets(url: string, products: string[], mode: McpMode = {}): string {
  if (products.length === 0 && !mode.readonly && !mode.search && !mode.lockdown) return url;
  const u = new URL(url);
  if (products.length > 0) u.searchParams.set("toolsets", [...products].sort().join(","));
  if (mode.readonly) u.searchParams.set("readonly", "1");
  if (mode.search) u.searchParams.set("tools", "search");
  if (mode.lockdown) u.searchParams.set("lockdown", "1");
  return u.toString().replace(/%2C/g, ",");
}

/** A word as a shell reads it: a URL with a query (`?`, `&`) quoted, so `&` doesn't end the command. */
export const shellWord = (s: string) => (/^[A-Za-z0-9_./:@%+=,-]+$/.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`);

export function mcpConfig(client: McpClient, url: string, token?: string): { lang: "sh" | "json" | "toml" | "text"; text: string } {
  const header = token ? `Bearer ${token}` : undefined;
  const server = (wrap: "mcpServers" | "servers", typed: boolean) => {
    const s: Record<string, unknown> = typed ? { type: "http", url } : { url };
    if (header) s.headers = { Authorization: header };
    return JSON.stringify({ [wrap]: { roll: s } }, null, 2);
  };
  switch (client) {
    case "claude-code":
      return { lang: "sh", text: `claude mcp add --transport http roll ${shellWord(url)}${header ? ` --header "Authorization: ${header}"` : ""}` };
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
