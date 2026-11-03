/**
 * Pure helpers for wiring roll code to the outside: repo webhook forms, commit-status snippets for
 * external CI (TeamCity, Jenkins, Buildkite, GitHub Actions-style runners), how statuses show up
 * among a change's checks, and MCP client configs for agents. No React, so they're unit-tested.
 */

import { WEBHOOK_EVENTS, type Check, type OwnerTokenInfo, type Status, type Webhook, type WebhookEvent, type WebhookInput } from "./code-api";

// ── webhooks ──

export interface WebhookForm {
  url: string;
  /** Every event, now and later (`["*"]`). */
  all: boolean;
  events: WebhookEvent[];
  /** Empty: roll generates one and shows it once. */
  secret: string;
  active: boolean;
}

export const WEBHOOK_EVENT_HINT: Record<WebhookEvent, string> = {
  landed: "a changeset lands on the default branch",
  change: "a change is opened, revised, queued, landed or closed",
  review: "a review is submitted or dismissed",
  thread: "a comment thread is opened, replied to or resolved",
  check: "a check or commit status changes",
  queue: "the merge queue starts, bisects or lands a batch",
  work: "a work item is created, claimed or finished",
  preview: "a preview starts, sleeps or fails",
  push: "a branch is pushed over git (accepted or not)",
};

export const emptyWebhookForm = (): WebhookForm => ({ url: "", all: false, events: ["landed"], secret: "", active: true });

/** The form for an existing webhook (the secret is never sent back, so it stays empty). */
export function webhookForm(w: Pick<Webhook, "url" | "events" | "active">): WebhookForm {
  const all = w.events.includes("*");
  return {
    url: w.url,
    all,
    events: all ? [] : (w.events.filter((e) => (WEBHOOK_EVENTS as readonly string[]).includes(e)) as WebhookEvent[]),
    secret: "",
    active: w.active,
  };
}

/** What's wrong with the form, field by field; empty when it can be sent. */
export function webhookErrors(f: WebhookForm): Partial<Record<"url" | "events" | "secret", string>> {
  const out: Partial<Record<"url" | "events" | "secret", string>> = {};
  let url: URL | null = null;
  try {
    url = new URL(f.url.trim());
  } catch {
    url = null;
  }
  if (!url || (url.protocol !== "https:" && url.protocol !== "http:")) out.url = "an http(s) URL";
  if (!f.all && f.events.length === 0) out.events = "pick at least one event";
  const s = f.secret.trim();
  if (s && (s.length < 16 || s.length > 200)) out.secret = "16–200 characters, or leave it empty to have one generated";
  return out;
}

/**
 * The request body: events in the contract's order (or `["*"]`), the secret only when typed. For an
 * update, only what differs from `prev` is sent.
 */
export function webhookBody(f: WebhookForm, prev?: Pick<Webhook, "url" | "events" | "active">): WebhookInput {
  const events = f.all ? ["*"] : WEBHOOK_EVENTS.filter((e) => f.events.includes(e));
  const body: WebhookInput = {};
  const url = f.url.trim();
  if (!prev || url !== prev.url) body.url = url;
  if (!prev || events.join(",") !== [...prev.events].join(",")) body.events = events;
  if (!prev || f.active !== prev.active) body.active = f.active;
  if (f.secret.trim()) body.secret = f.secret.trim();
  return body;
}

/** "all events" or "landed, push": for a webhook's row. */
export function eventsLabel(events: string[]) {
  return events.includes("*") ? "all events" : events.length ? events.join(", ") : "no events";
}

// ── commit statuses from external CI ──

export interface CiSystem {
  name: string;
  /** How the CI names the commit it built. */
  commit: string;
  /** A link back to the build. */
  target: string;
  /** A one-line note on where the step goes. */
  note: string;
}

export const CI_SYSTEMS: CiSystem[] = [
  {
    name: "TeamCity",
    commit: "%build.vcs.number%",
    target: "%teamcity.serverUrl%/viewLog.html?buildId=%teamcity.build.id%",
    note: "a Command Line step at the start and end of the build; ROLL_TOKEN as an env.ROLL_TOKEN password parameter",
  },
  {
    name: "Jenkins",
    commit: "$GIT_COMMIT",
    target: "$BUILD_URL",
    note: "an sh step, with ROLL_TOKEN from a Secret text credential (withCredentials)",
  },
  {
    name: "Buildkite",
    commit: "$BUILDKITE_COMMIT",
    target: "$BUILDKITE_BUILD_URL",
    note: "a command step, with ROLL_TOKEN from your secrets",
  },
  {
    name: "GitHub Actions-style",
    commit: "$GITHUB_SHA",
    target: "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID",
    note: "a run step, with ROLL_TOKEN from secrets (Gitea and Forgejo runners expose the same variables)",
  },
];

/** Single quotes for the shell; variables inside stay literal, so the JSON is built with "…" around them. */
const json = (fields: Record<string, string>) =>
  "{" +
  Object.entries(fields)
    .map(([k, v]) => `"${k}":"${v.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`)
    .join(",") +
  "}";

/**
 * `curl` that posts a commit status for the commit CI built. `api` is the repo's API base
 * (`…/v1/code/repos/owner/name`). The body is double-quoted so the CI's variables expand; `state`
 * may itself be a variable (`$STATE`).
 */
export function statusCurl(api: string, ci: Pick<CiSystem, "commit" | "target">, state: Status["state"] | "$STATE", context = "ci", description?: string) {
  const fields: Record<string, string> = { state, context, target_url: ci.target };
  if (description) fields.description = description;
  const body = json(fields).replace(/"/g, '\\"');
  return `curl -fsS -X POST "${api}/statuses/${ci.commit}" \\
  -H "Authorization: Bearer $ROLL_TOKEN" \\
  -H "content-type: application/json" \\
  -d "${body}"`;
}

/** The start (pending) and the end of a CI run: set STATE to success or failure before the second call. */
export function statusScript(api: string, ci: CiSystem, context: string) {
  return `# when the build starts
${statusCurl(api, ci, "pending", context, "running")}

# when it finishes: STATE=success or STATE=failure
${statusCurl(api, ci, "$STATE", context)}`;
}

export interface CheckRow {
  name: string;
  status: Check["status"];
  /** "roll", "CI status", "GitHub", "reported". */
  source: string;
  url: string | null;
  summary: string | null;
  /** Who posted it (the token's name), for commit statuses. */
  creator: string | null;
  /** The status said `error` (rather than a failing test): the summary starts with "error:". */
  errored: boolean;
  updated: number | null;
}

const SOURCE_LABEL: Record<Check["source"], string> = { runner: "roll", external: "reported", github: "GitHub" };

/**
 * A change's checks with the commit statuses posted for its head folded in: a status becomes a
 * check named after its context, so the two are matched by name and the status adds who posted it.
 * Statuses with no check yet (a status posted before the change's head moved there) are listed too.
 */
export function checkRows(checks: Check[], statuses: Status[] | null | undefined): CheckRow[] {
  const byContext = new Map((statuses ?? []).map((s) => [s.context, s]));
  const rows: CheckRow[] = checks.map((c) => {
    const st = c.source === "external" ? byContext.get(c.name) : undefined;
    if (st) byContext.delete(c.name);
    return {
      name: c.name,
      status: c.status,
      source: st ? "CI status" : SOURCE_LABEL[c.source],
      url: c.url ?? st?.target_url ?? null,
      summary: st?.description ?? c.summary,
      creator: st?.creator ?? null,
      errored: st ? st.state === "error" : (c.summary ?? "").startsWith("error:"),
      updated: st?.updated_at ?? c.finished_at ?? c.started_at,
    };
  });
  for (const st of byContext.values()) {
    rows.push({
      name: st.context,
      status: statusToCheck(st.state),
      source: "CI status",
      url: st.target_url,
      summary: st.description,
      creator: st.creator,
      errored: st.state === "error",
      updated: st.updated_at,
    });
  }
  return rows;
}

/** How the server maps a status onto a check. */
export function statusToCheck(state: Status["state"]): Check["status"] {
  return state === "pending" ? "running" : state === "success" ? "success" : "failure";
}

// ── MCP ──

/** `claude mcp add` for Claude Code; the token is a placeholder unless given. */
export function mcpCommand(url: string, token?: string | null) {
  const header = token === null ? "" : ` \\\n  --header "Authorization: Bearer ${token || "$ROLL_CODE_TOKEN"}"`;
  return `claude mcp add --transport http roll ${url}${header}`;
}

/** The JSON most MCP clients take. */
export function mcpJson(url: string, token?: string | null, agent?: string) {
  const headers: Record<string, string> = {};
  if (token !== null) headers.Authorization = `Bearer ${token || "rlc_…"}`;
  if (agent) headers["X-Roll-Agent"] = agent;
  const server: Record<string, unknown> = { type: "http", url };
  if (Object.keys(headers).length) server.headers = headers;
  return JSON.stringify({ mcpServers: { roll: server } }, null, 2);
}

// ── access tokens ──

/** How a listed token reads: `rlc_ab12…` (or its id when it predates prefixes) and when it was last used. */
export function tokenLine(t: OwnerTokenInfo, ago: (unix: number) => string) {
  return {
    id: t.prefix ? `${t.prefix}…` : t.id,
    used: t.last_used_at === null ? "never used" : `used ${ago(t.last_used_at)}`,
  };
}
