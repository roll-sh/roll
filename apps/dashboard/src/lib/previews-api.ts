/**
 * Client for roll previews (`crates/roll-previews`, docs/previews.md "HTTP contract"): a full copy
 * of a workspace project per change, with a branch of every data resource. Change both together.
 */

import { apiHeaders, call, onApiStatus, ROLL_API } from "./roll-api";

export const PREVIEW_STATES = ["queued", "awaiting_approval", "creating", "ready", "sleeping", "updating", "failed", "deleting"] as const;
export type PreviewState = (typeof PREVIEW_STATES)[number];

export type PreviewKind = "postgres" | "redis" | "bucket" | "nosql" | "stream" | "queue" | "tasks" | "search";

export interface PreviewService {
  name: string;
  repo: string;
  url: string;
  /** `url` with the preview's access token (link access): the link to open and hand out. */
  share_url: string | null;
  /** The changeset it should run. */
  revision: string | null;
  /** The changeset it was last deployed at. */
  deployed: string | null;
  state: "pending" | "deploying" | "running" | "sleeping" | "failed";
  error: string | null;
}

export interface PreviewResource {
  kind: PreviewKind;
  /** The resource in the project it branches. */
  parent: string;
  /** The branch, once it exists. */
  ref: string | null;
  mode: "branch" | "schema";
  state: "pending" | "ready" | "resetting" | "failed" | "deleting";
  error: string | null;
  /** Bytes the branch owns (not its parent's). */
  bytes: number;
  /** What of the parent's data the preview's code gets: `real` is production data as it is. */
  data: "none" | "masked" | "real";
}

/**
 * What to tell someone about a resource whose previews would get its data as it is (the
 * server's `data: "real"`, from the settings), with the way out; `null` when they wouldn't.
 */
export function unmaskedNote(kind: string, mode: "branch" | "schema" | "skip", masks: number): string | null {
  // Queues and tasks never copy messages; a repo or a service holds no data to branch.
  if (mode !== "branch" || ["queue", "tasks", "repo", "service"].includes(kind)) return null;
  if (kind === "postgres" || kind === "nosql" || kind === "search")
    return masks > 0 ? null : "Previews get its data as it is: mask what holds people's data, or use Schema only.";
  if (kind === "stream") return "Previews get its messages as they are (they can't be masked): use Schema only, or skip it.";
  return "Previews get its data as it is (it can't be masked): a seed can replace it with made-up data, or skip it.";
}

export interface PreviewCost {
  sleeping_usd_month: number;
  active_usd_hour: number;
  month_to_date_usd: number;
  bytes: number;
}

/** Times are unix milliseconds. */
export interface Preview {
  id: string;
  project: string;
  slug: string;
  /** `owner/name`. */
  repo: string;
  /** `chg_…`, for a change's preview. */
  change: string | null;
  number: number | null;
  github_pr: number | null;
  branch: string | null;
  title: string;
  head: string;
  /** The earlier head the change's services still run (later pushes touched only ignored paths). */
  runs?: string | null;
  trusted: boolean;
  desired: "active" | "deleted";
  state: PreviewState;
  error: string | null;
  /** The first service's URL. */
  url: string | null;
  /** `url` with the access token, when the project's previews need a link. */
  share_url: string | null;
  services: PreviewService[];
  resources: PreviewResource[];
  cost: PreviewCost;
  /** What the change did to the Postgres schema (`+ …`, `- …`), once read at `head`. */
  schema_changes?: string[] | null;
  /** The project's release (migrations) at its last head: how long it took on this preview's data, and the run before's. */
  release?: { head: string; ms: number; before_ms: number | null } | null;
  /** Kept past its TTL until then (epoch ms). */
  kept_until?: number | null;
  /** When it goes (its TTL, or a keep, running out), once that's close (a day ahead). */
  goes_at?: number | null;
  /** How much the page changed since the screenshot at an earlier head (`since`). */
  visual_change?: { since: string; percent: number; phone_percent?: number | null } | null;
  /** How much its page differs from the project's preview of the default branch. */
  base_change?: { branch: string; percent: number; phone_percent: number | null; head?: string | null; load_ms?: number | null; lcp_ms?: number | null; a11y_added?: number | null; errors_added?: number | null; text?: TextDiff | null; links_added?: number | null; seo_added?: number | null } | null;
  /** The project's other pages (`github.pages`) at the latest screenshot. */
  pages?: PageCheck[];
  /** Its visual review (`github.visual_review`): whether how its pages look against the baseline was accepted at this head. */
  visual_review?: {
    state: "unchanged" | "changed" | "accepted";
    branch: string;
    percent: number;
    by: string | null;
    at: number | null;
    required: boolean;
    head: string;
    /** Each page that differs (`path` `""`: the first page) and who accepted it at this head: the change is accepted once every one is. */
    pages: { path: string; percent: number; by: string | null; at: number | null }[];
  } | null;
  /** It also has a phone-width screenshot. */
  phone_screenshot?: boolean;
  /** How fast its first page loaded at this head (timed with the screenshot), and at the one before. */
  page_speed?: { now: PageSpeed; before: PageSpeed | null } | null;
  /** What went wrong while that page loaded (the page's own words: shown as text only). */
  page_health?: { errors: number; first_error: string | null; failed: number; first_failed: string | null } | null;
  /** What a screen reader can't name on that page (`first`: its markup, shown as text only). */
  page_a11y?: { issues: A11yIssue[]; lang: boolean; title: boolean; seo?: PageSeo | null } | null;
  /** Its first page's own links that are broken (status 0: no answer), of those checked. */
  page_links?: { checked: number; broken: { path: string; status: number }[] } | null;
  /** Earlier pushes of the change's services, still reachable on the same data (newest first). */
  earlier?: { service: string; revision: string; url: string; share_url: string | null; at: number }[];
  /** When its screenshot was taken (`screenshot` below), if it has one. */
  screenshot_at?: number | null;
  last_reset_at: number | null;
  last_active_at: number;
  expires_at: number | null;
  created_by: string;
  /** roll keeps it to compare the project's changes with (`github.baseline`). */
  baseline?: boolean;
  created_at: number;
  updated_at: number;
}

export interface PreviewActivity {
  at: number;
  message: string;
}

export type ResourceRule = "branch" | "schema" | "skip";

/** A command on the change's tree: the seed, or the release. */
export interface PreviewJob {
  image: string;
  run: string;
  timeout_s?: number;
  memory_mb?: number;
}

/** A link that opens a preview until it ends, for someone outside the team. */
export interface PreviewShare {
  id: string;
  expires_at: number;
  created_by: string;
  created_at: number;
  revoked: boolean;
}

/** Another page of the first service: errors while it loaded, its change since the screenshot at
 * `since`, and how much it differs from the default branch's preview. */
export interface PageCheck {
  path: string;
  errors: number;
  since: string | null;
  percent: number | null;
  base_percent: number | null;
}

/** How slow and heavy the first page may get (0: no limit): over it, `roll/speed` fails. */
export interface SpeedBudget {
  load_ms: number;
  bytes: number;
  /** Largest contentful paint (Core Web Vitals' "good": up to 2500). */
  lcp_ms?: number;
  /** Cumulative layout shift ("good": up to 0.1). */
  cls?: number;
}

/** A page's load, as the browser that took its screenshot timed it. */
export interface PageSpeed {
  ttfb_ms: number;
  fcp_ms: number | null;
  load_ms: number;
  bytes: number;
  /** Core Web Vitals: largest contentful paint, and cumulative layout shift (its largest session window). */
  lcp_ms?: number | null;
  cls?: number | null;
}

/** A preview page's accessibility problems in words ("3 images without alt text", "no title"). */
/** One kind of accessibility problem: one of the session's own four, or an axe-core rule (`color-contrast`, with its impact and words). */
export interface A11yIssue {
  kind: string;
  count: number;
  first: string | null;
  impact?: "minor" | "moderate" | "serious" | "critical" | null;
  help?: string | null;
}

export function a11yWords(a: { issues: A11yIssue[]; lang: boolean; title: boolean }): string[] {
  const what: Record<string, string> = { images: "without alt text", fields: "without a label", buttons: "without a name", links: "without a name" };
  const noun: Record<string, [string, string]> = { images: ["image", "images"], fields: ["form field", "form fields"], buttons: ["button", "buttons"], links: ["link", "links"] };
  const words = a.issues
    .filter((i) => i.count > 0)
    .map((i) =>
      noun[i.kind]
        ? `${i.count} ${noun[i.kind][i.count === 1 ? 0 : 1]} ${what[i.kind]}`
        : `${i.count} ${i.count === 1 ? "element" : "elements"} failing ${i.help ? `“${i.help}”` : i.kind}${i.impact ? ` (${i.impact})` : ""}`,
    );
  if (!a.lang) words.push("no page language");
  if (!a.title) words.push("no title");
  return words;
}

/** The first page's SEO basics (Lighthouse's SEO audit), from its screenshot's session. */
export interface PageSeo {
  description: boolean;
  viewport: boolean;
  h1: number;
  noindex: boolean;
}

/** Its SEO problems in words, in the order roll compares them with main's. */
export function seoWords(s: PageSeo): string[] {
  const out: string[] = [];
  if (!s.description) out.push("no meta description");
  if (!s.viewport) out.push("no viewport");
  if (s.h1 === 0) out.push("no h1");
  else if (s.h1 > 1) out.push("more than one h1");
  if (s.noindex) out.push("noindex in its markup");
  return out;
}

/** A page's visible text against another's, by line: the page's own words, shown as text only. */
export interface TextDiff {
  added: string[];
  removed: string[];
  added_n: number;
  removed_n: number;
}

/** A message a preview's services sent, caught instead of delivered. */
export interface CaughtMail {
  id: string;
  at: number;
  from: string;
  to: string[];
  subject: string | null;
  size: number;
}

/** A page as people read it: the first page at another width (`@768`) is "768 px". */
export const pageLabel = (path: string) => (/^@\d+$/.test(path) ? `${path.slice(1)} px` : path || "/");

/** "1.2 s" or "380 ms". */
export const duration = (ms: number) => (ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(1)} s`);

export interface PreviewSettings {
  enabled: boolean;
  /** Globs on the change's branch. */
  branches: string[];
  drafts: boolean;
  /** Only changes whose title asks (`[preview]`) get one. */
  manual: boolean;
  /** Gitignore-style patterns: a change must touch a path one matches (`[]`: any path)… */
  paths: string[];
  /** …that none of these match. */
  ignore_paths: string[];
  /** `"<kind>:<ref>"` → mode; resources not listed get `data`. A patch replaces the whole map. */
  resources: Record<string, ResourceRule>;
  data: "branch" | "schema";
  /** 0: never. */
  reset_every_hours: number;
  sleep_after_s: number;
  ttl_hours: number;
  max_previews: number;
  max_bytes: number;
  /** The month's budget for the project's previews, USD (0: no cap). */
  max_usd_month: number;
  secrets: { base: string; allow_production: boolean };
  /** A patch replaces the whole map. */
  env: Record<string, string>;
  github: { comment: boolean; status: boolean; deployment: boolean; screenshot?: boolean; baseline?: boolean; pages?: string[]; hide?: string[]; budget?: SpeedBudget; visual_threshold?: number; widths?: number[] };
  /** "link": a preview's URLs need its link (its access token once, then a cookie). */
  access: "link" | "public";
  /** Chromatic's UI Review: `check` posts `roll/visual`; `required` also keeps a change waiting for one from landing in roll. */
  visual_review?: "off" | "check" | "required";
  /** `catch` (default): services send mail to roll's catcher, listed on the preview, never delivered; `deliver`: the project's own mail settings. */
  mail?: "catch" | "deliver";
  /** Branch the data as of a point in time where the product keeps history: `-24h`, `-30m`, `-7d`, or an RFC 3339 time (30 days back at most). */
  data_at?: string | null;
  /** A feedback button on its pages for people who came in through roll (link access). */
  feedback: boolean;
  /** Variables for a family of branches: a branch glob → `{name: value}`, over `env`. */
  env_for: Record<string, Record<string, string>>;
  /** Wake the change's services as soon as a new head is deployed (a warm first visit). */
  warm: boolean;
  /** Run once on fresh data (after branching or a reset) before the services deploy. */
  seed: PreviewJob | null;
  /** Run at each new head, after the seed and before services deploy (migrations). */
  release?: PreviewJob | null;
  /** NoSQL column masks: `"nosql:<ref>"` → `{"keyspace.table.column": "mask_default()"}`. */
  masking: Record<string, Record<string, string>>;
}

/** A partial update: objects merge (except `resources` and `env`, which are replaced whole). */
export type SettingsPatch = Partial<Omit<PreviewSettings, "secrets" | "github">> & {
  secrets?: Partial<PreviewSettings["secrets"]>;
  github?: Partial<PreviewSettings["github"]>;
};

export interface PreviewUsage {
  previews: number;
  sleeping_usd_month: number;
  active_usd_hour: number;
  month_to_date_usd: number;
  bytes: number;
  /** This month, deleted previews included (what the budget counts). */
  spent_usd_month?: number;
  /** The project's monthly budget for previews, USD (0: no cap). */
  max_usd_month?: number;
}

/** A project's secret for automation: CI sends it in `header` to open private previews. */
export interface PreviewBypass {
  enabled: boolean;
  created_at: number | null;
  header: string;
  /** Only in the answer that made it. */
  secret?: string;
}

export type PreviewSource = { branch: string; change?: never } | { change: string; branch?: never };

const one = (id: string) => `/v1/previews/${encodeURIComponent(id)}`;
const project = (id: string) => `/v1/previews/projects/${encodeURIComponent(id)}`;

function query(params: Record<string, string | number | undefined>) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== "") q.set(k, String(v));
  const s = q.toString();
  return s ? `?${s}` : "";
}

/** The preview's screenshot as an object URL (the operator's headers, which an <img> can't send);
 * revoke it when done. */
/** The preview's screenshot as an object URL; `previous`: the one at the head before; `phone`: at a phone's width; `base`: the default branch's preview's; `page`: another of the project's pages. */
/** A service's link with the preview's access as a QR code (an object URL; revoke it when done): a phone opens it by scanning. */
export async function qrUrl(id: string, service?: string): Promise<string> {
  const res = await fetch(`${ROLL_API}${one(id)}/qr.png${service ? `?service=${encodeURIComponent(service)}` : ""}`, { headers: apiHeaders(), cache: "no-store" });
  onApiStatus(res.status);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`.trim());
  return URL.createObjectURL(await res.blob());
}

export async function screenshotUrl(id: string, previous = false, phone = false, base = false, page?: string, diff?: "base" | "previous"): Promise<string> {
  const q = [previous ? "previous=1" : "", phone ? "view=phone" : "", base ? "base=1" : "", page ? `page=${encodeURIComponent(page)}` : "", diff ? `diff=${diff}` : ""].filter(Boolean).join("&");
  const res = await fetch(`${ROLL_API}${one(id)}/screenshot${q ? `?${q}` : ""}`, { headers: apiHeaders(), cache: "no-store" });
  onApiStatus(res.status);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`.trim());
  return URL.createObjectURL(await res.blob());
}

export const previewsApi = {
  /** Newest first. */
  list: (filter: { project?: string; state?: PreviewState } = {}) => call<Preview[]>("GET", `/v1/previews${query(filter)}`),
  get: (id: string) => call<Preview>("GET", one(id)),
  /** A preview of a branch (no change) or of a change; the existing one if there is one. */
  create: (projectId: string, repo: string, source: PreviewSource) =>
    call<Preview>("POST", "/v1/previews", { project: projectId, repo, ...source }),
  /** Marks it for deletion (`desired: "deleted"`); the reconciler removes it. */
  remove: (id: string) => call<Preview>("DELETE", one(id)),
  refresh: (id: string) => call<Preview>("POST", `${one(id)}/refresh`),
  /** Keep it past its TTL for `hours` (at most 720); 0 lets its rules decide again. */
  keep: (id: string, hours: number) => call<Preview>("POST", `${one(id)}/keep`, { hours }),
  /** Accept how a change's pages look at its latest screenshot (the `roll/visual` status). */
  /** `head`: the screenshot's that was looked at (`visual_review.head`): refused (409) once a newer one came in. `path`: that page only (`""` the first), else every page. */
  acceptVisual: (id: string, head?: string, path?: string) =>
    call<Preview>("POST", `${one(id)}/visual/accept`, { ...(head ? { head } : {}), ...(path !== undefined ? { path } : {}) }),
  /** Its share links (never their tokens). */
  shares: (id: string) => call<PreviewShare[]>("GET", `${one(id)}/shares`),
  /** A link for someone outside the team, open for `hours`; `url` is in this answer only. */
  share: (id: string, hours: number) => call<PreviewShare & { url: string }>("POST", `${one(id)}/shares`, { hours }),
  unshare: (id: string, share: string) => call<void>("DELETE", `${one(id)}/shares/${encodeURIComponent(share)}`),
  /** Its own variables, over the project's `env`. */
  vars: (id: string) => call<{ vars: Record<string, string> }>("GET", `${one(id)}/env`),
  /** Resolve a note left on its page (the note's review id): the page's button stops listing it. */
  resolveNote: (id: string, note: string) => call<{ resolved: string }>("POST", `${one(id)}/notes/${encodeURIComponent(note)}/resolve`, {}),
  /** The mail its services sent, caught instead of delivered (newest first; the last 50). */
  mail: (id: string) => call<{ mail: CaughtMail[] }>("GET", `${one(id)}/mail`),
  /** One caught message as it was sent, as text. */
  mailText: async (id: string, mail: string) => {
    const res = await fetch(`${ROLL_API}${one(id)}/mail/${encodeURIComponent(mail)}`, { headers: apiHeaders(), cache: "no-store" });
    onApiStatus(res.status);
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`.trim());
    return res.text();
  },
  /** Each data branch's connection, as its services get it (its own credentials, never the parent's). */
  connections: (id: string) => call<{ branches: { kind: PreviewKind; parent: string; vars: Record<string, string> }[] }>("GET", `${one(id)}/connect`),
  /** Set or (`null`) remove some of its own variables; its services redeploy with them. */
  setVars: (id: string, vars: Record<string, string | null>) => call<{ vars: Record<string, string> }>("PUT", `${one(id)}/env`, { vars }),
  sleep: (id: string) => call<Preview>("POST", `${one(id)}/sleep`),
  wake: (id: string) => call<Preview>("POST", `${one(id)}/wake`),
  /** Re-branches every resource, or the ones named (`"<kind>:<parent>"`). */
  reset: (id: string, resources?: string[]) => call<Preview>("POST", `${one(id)}/reset`, resources?.length ? { resources } : {}),
  logs: (id: string, opts: { service?: string; lines?: number } = {}) =>
    call<{ service: string; lines: string[] }>("GET", `${one(id)}/logs${query(opts)}`),
  /** Newest first. */
  activity: (id: string) => call<PreviewActivity[]>("GET", `${one(id)}/activity`),
  settings: (projectId: string) => call<PreviewSettings>("GET", `${project(projectId)}/settings`),
  putSettings: (projectId: string, patch: SettingsPatch) => call<PreviewSettings>("PUT", `${project(projectId)}/settings`, patch),
  usage: (projectId: string) => call<PreviewUsage>("GET", `${project(projectId)}/usage`),
  bypass: (projectId: string) => call<PreviewBypass>("GET", `${project(projectId)}/bypass`),
  /** A new secret (the old one stops working), shown only in this answer. */
  rotateBypass: (projectId: string) => call<PreviewBypass>("POST", `${project(projectId)}/bypass`),
  deleteBypass: (projectId: string) => call<void>("DELETE", `${project(projectId)}/bypass`),
};

/** Server-sent events in `text` (after `rest`, what was left of the last chunk): the complete
 * events, and what's left over. */
export function parseSse(rest: string, text: string): { events: { event: string; data: string }[]; rest: string } {
  let raw = rest + text;
  // A \r at the very end may be the first half of a \r\n split across chunks: wait for the next.
  const held = raw.endsWith("\r") ? "\r" : "";
  if (held) raw = raw.slice(0, -1);
  const all = raw.replace(/\r\n?/g, "\n");
  const blocks = all.split("\n\n");
  const left = (blocks.pop() ?? "") + held;
  const events = blocks.flatMap((b) => {
    let event = "message";
    const data: string[] = [];
    for (const line of b.split("\n")) {
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
    }
    return data.length ? [{ event, data: data.join("\n") }] : [];
  });
  return { events, rest: left };
}

/** A service's last `lines`, then its output as it comes (`…/logs?follow=1`, read with the
 * operator's headers, which EventSource can't send). Resolves when the stream ends or `signal`
 * aborts it. */
export async function followLogs(id: string, service: string, lines: number, onLine: (l: string) => void, signal: AbortSignal): Promise<void> {
  const res = await fetch(`${ROLL_API}${one(id)}/logs${query({ service, lines, follow: 1 })}`, { headers: apiHeaders(), cache: "no-store", signal });
  onApiStatus(res.status);
  if (!res.ok || !res.body) {
    const data = await res.json().catch(() => null);
    throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
  }
  const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
  let rest = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    const parsed = parseSse(rest, value);
    rest = parsed.rest;
    for (const e of parsed.events) if (e.event === "message") onLine(e.data);
  }
}

/** A resource as the reset body and the settings name it. */
export const resourceId = (r: Pick<PreviewResource, "kind" | "parent">) => `${r.kind}:${r.parent}`;

/** Work in progress: the list polls quickly while any preview is in one of these. */
export const BUSY: ReadonlySet<PreviewState> = new Set(["creating", "updating", "deleting"]);

/** Production's environment names (crates/roll-previews `is_production`). */
export const isProduction = (env: string) => ["production", "prod", "live"].includes(env.toLowerCase());

/** The URL to open: with the access token when the preview is private. */
export const openUrl = (x: { url: string | null; share_url?: string | null }) => x.share_url ?? x.url ?? undefined;

/** How a load compares with the default branch's, past the noise (100 ms and a tenth): "300 ms slower than main"; the comment says the same. */
export function thanMain(now: number, main: number | null | undefined, branch: string): string | null {
  if (main == null) return null;
  const d = now - main;
  if (Math.abs(d) < 100 || Math.abs(d) * 10 < main) return null;
  const t = Math.abs(d) < 1000 ? `${Math.abs(d)} ms` : `${(Math.abs(d) / 1000).toFixed(1)} s`;
  return `${t} ${d > 0 ? "slower" : "faster"} than ${branch}`;
}
