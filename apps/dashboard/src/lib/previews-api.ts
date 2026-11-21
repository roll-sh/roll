/**
 * Client for roll previews (`crates/roll-previews`, docs/previews.md "HTTP contract"): a full copy
 * of a workspace project per change, with a branch of every data resource. Change both together.
 */

import { withSession } from "./code-token";
import { followScreen, type ScreenHandlers } from "./sse";
import { apiHeaders, call, onApiStatus, ROLL_API } from "./roll-api";

export const PREVIEW_STATES = ["queued", "awaiting_approval", "creating", "ready", "sleeping", "updating", "failed", "deleting"] as const;
export type PreviewState = (typeof PREVIEW_STATES)[number];

export type PreviewKind = "postgres" | "redis" | "bucket" | "nosql" | "stream" | "queue" | "tasks" | "search";

export interface PreviewService {
  name: string;
  repo: string;
  url: string;
  /** `url` with the preview's access token (link access; plain otherwise): the link to open and hand out. */
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

/** A column (or search field) of a project's Postgres, NoSQL or search resource that looks like
 * people's data and has no mask yet (`GET …/mask-suggestions`), with a rule its type and
 * constraints take. Every such resource is read, whatever its rule; the dialog shows them under
 * those on `branch`. */
export interface MaskSuggestion {
  /** `"postgres:<ref>"`, `"nosql:<ref>"` or `"search:<ref>"`, as `masking` keys it. */
  resource: string;
  path: string;
  rule: string;
  why: string;
}

export interface MaskSuggestions {
  suggestions: MaskSuggestion[];
  /** Resources whose columns weren't all read, and why: asleep (not woken for this), not read
   * within 10 s, couldn't be read, or only the first 2,000 read. */
  unread: { resource: string; why: string }[];
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
  /** What in `schema_changes` would hurt production when it lands (a dropped table or column, a type change, a column made NOT NULL). */
  schema_risks?: string[];
  /** The project's release (migrations) at its last head: how long it took on this preview's data, and the run before's. */
  release?: { head: string; ms: number; before_ms: number | null } | null;
  /** Kept past its TTL until then (epoch ms). */
  kept_until?: number | null;
  /** What its code may reach: a fork's preview is `roll` by default though the project's reach anything (`egress_forks`). */
  reaches?: "open" | "roll";
  /** The pull request a Git URL service's preview is of (roll apps' push hook). A fork's runs only at `approved_head`. */
  pull?: { number: number; url: string; fork: boolean; approved_head: string | null; approved_by: string | null };
  /** When it goes (its TTL, or a keep, running out), once that's close (a day ahead). */
  goes_at?: number | null;
  /** The project that reports a Git URL service's pull request, when another one does (the service is on both canvases; the one that has had it longest speaks). */
  reported_by?: string | null;
  /** How much the page changed since the screenshot at an earlier head (`since`). */
  visual_change?: { since: string; percent: number; phone_percent?: number | null } | null;
  /** How much its page differs from the project's preview of the default branch. */
  base_change?: { branch: string; percent: number; phone_percent: number | null; head?: string | null; load_ms?: number | null; lcp_ms?: number | null; a11y_added?: number | null; errors_added?: number | null; text?: TextDiff | null; links_added?: number | null; seo_added?: number | null; api?: TextDiff | null; api_breaking?: string[]; api_unread?: boolean; endpoints?: EndpointTime[] } | null;
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
    pages: { path: string; percent: number; by: string | null; at: number | null; unstable?: boolean }[];
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
  /** Its routes (from its traces) clearly slower than main's preview's, as its holder last read them. */
  /** The project's rule for regressions against main (`check`, `required`). */
  regressions?: "check" | "required" | null;
  /** Who accepted its regressions at its head. */
  regressions_accepted?: { head: string; by: string; at: number } | null;
  /** The keys it holds as placeholders (its egress proxy swaps each in for its hosts only), and
   * how many requests had one swapped in or were refused. Never a value. */
  intercepted?: {
    keys: { key: string; hosts: string[] }[];
    swapped: number;
    refused: number;
    /** roll apps' proxies for its services there (their own placeholders): requests filtered,
     * placeholders swapped, the latest refusals' reasons. */
    on_apps?: { requests: number; swapped: number; refused?: string[] };
  } | null;
  /** Its error groups main's preview doesn't have, as its holder last read them. */
  new_errors?: { branch: string; total: number; errors: { service: string; title: string }[] } | null;
  slower_routes?: { branch: string; routes: { name: string; count: number; p50_ms: number; main_p50_ms: number; grew?: GrewSpan | null; /** Its budget, when it's over it. */ budget_ms?: number | null }[] } | null;
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

/** Someone signed in to roll who opened a team-access preview: their handle, and when they were
 * last in it (as of their session's 5-minute recheck). */
export interface PreviewVisitor {
  who: string;
  last_at: number;
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
  /** The first page's checks, read on this page too (accessibility with SEO, broken links). */
  a11y?: { issues: A11yIssue[]; lang: boolean; title: boolean; seo?: PageSeo | null } | null;
  links?: { checked: number; broken: { path: string; status: number }[] } | null;
  /** Against main's page at the same path. */
  against?: { a11y_added: number | null; links_added: number | null; seo_added: number | null; text: TextDiff | null } | null;
  /** A page state's step that found nothing (the state taken as it was then). */
  step?: string | null;
}

/** A page state (`github.states`): a page of the first service's, reached by its steps. */
export interface PageState {
  name: string;
  page: string;
  steps: ({ click: string } | { type: [string, string] } | { press: string } | { wait: number })[];
}

/** How slow and heavy the first page may get (0: no limit): over it, `roll/speed` fails. */
export interface SpeedBudget {
  load_ms: number;
  bytes: number;
  /** Largest contentful paint (Core Web Vitals' "good": up to 2500). */
  lcp_ms?: number;
  /** Cumulative layout shift ("good": up to 0.1). */
  cls?: number;
  /** Routes held to a time of their own: a trace's root span name (`GET /orders`) → its p50 in ms. */
  routes?: Record<string, number>;
}

/** `GET /orders 300` lines → `{ "GET /orders": 300 }` (in name order, as the server keeps them):
 * a route, then its p50 budget in ms. `bad`: the lines that aren't one, to say so. */
export function parseRouteBudgets(text: string): { routes: Record<string, number>; bad: string[] } {
  const found: [string, number][] = [];
  const bad: string[] = [];
  for (const line of text.split("\n").map((l) => l.trim()).filter(Boolean)) {
    const m = line.match(/^(.+?)\s+(\d+)\s*(?:ms)?$/);
    if (m) found.push([m[1].trim(), Number(m[2])]);
    else bad.push(line);
  }
  found.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return { routes: Object.fromEntries(found), bad };
}

export const routeBudgetLines = (routes: Record<string, number> | undefined) =>
  Object.entries(routes ?? {})
    .map(([name, ms]) => `${name} ${ms}`)
    .join("\n");

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

/**
 * What the first page's checks found, for the sheet's "Checks" heading: problems (errors and
 * failed requests, broken links, accessibility and SEO problems) and how many of them are new
 * against main, where it was compared (`null`: not compared).
 */
export function checkSummary(p: Pick<Preview, "page_health" | "page_links" | "page_a11y" | "base_change">): { problems: number; added: number | null } {
  const errors = (p.page_health?.errors ?? 0) + (p.page_health?.failed ?? 0);
  const links = p.page_links?.broken.length ?? 0;
  const a11y = p.page_a11y ? a11yWords(p.page_a11y).length : 0;
  const seo = p.page_a11y?.seo ? seoWords(p.page_a11y.seo).length : 0;
  const b = p.base_change;
  const parts = b ? [b.errors_added, b.links_added, b.a11y_added, b.seo_added].filter((n): n is number => n != null) : [];
  return { problems: errors + links + a11y + seo, added: parts.length ? parts.reduce((x, y) => x + y, 0) : null };
}

/** A page as people read it: the first page at another width (`@768`) is "768 px". */
/** An endpoint's time (`github.endpoints`: the median of 10 GETs) against main's. */
export interface EndpointTime {
  path: string;
  ms: number;
  /** The last GET's status (0: no answer). */
  status: number;
  main_ms: number | null;
  /** Slower than main's by half again and by more than 20 ms (roll code's change view says so). */
  slower?: boolean;
}

/** Slower than main's by half again and by more than 20 ms (`EndpointAgainst::slower`). */
export const endpointSlower = (e: EndpointTime) => e.slower ?? (e.main_ms != null && e.ms * 2 > e.main_ms * 3 && e.ms > e.main_ms + 20);

export const pageLabel = (path: string) =>
  /^@\d+$/.test(path) ? `${path.slice(1)} px` : path === "browser:firefox" ? "Firefox" : path === "browser:webkit" ? "WebKit" : path === "browser:webkit-phone" ? "WebKit, phone" : path || "/";

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
  /** Globs on a change's author (a pull request's GitHub login, `*[bot]`): skipped unless asked. */
  skip_authors?: string[];
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
  github: { comment: boolean; status: boolean; deployment: boolean; screenshot?: boolean; baseline?: boolean; pages?: string[]; hide?: string[]; budget?: SpeedBudget; visual_threshold?: number; widths?: number[]; openapi?: string | null; endpoints?: string[]; states?: PageState[]; browsers?: ("firefox" | "webkit" | "webkit-phone")[] };
  /** "link": a preview's URLs need its link (its access token once, then a cookie). "team": only people signed in to roll who may open the project (they sign in on the way; the link's token lets nobody in). */
  access: "link" | "team" | "public";
  /** Chromatic's UI Review: `check` posts `roll/visual`; `required` also keeps a change waiting for one from landing in roll. */
  visual_review?: "off" | "check" | "required";
  /** `check` posts `roll/regressions`, failing while a change has routes slower than main's preview or errors main's doesn't have. */
  regressions?: "off" | "check" | "required";
  /** `catch` (default): services send mail to roll's catcher, listed on the preview, never delivered; `deliver`: the project's own mail settings. */
  mail?: "catch" | "deliver";
  /** Its `roll.toml` services get the OpenTelemetry SDK's variables, their spans kept on the preview a few hours. */
  traces?: boolean;
  /** What its services' code may reach: `open` (default) anything; `roll` only roll's own products (a network with no route out). */
  egress?: "open" | "roll";
  /** With `egress: "roll"` (or `egress_forks: "roll"`, for forks): hosts outside roll its code may still reach (`api.stripe.com`, `*.sentry.io`), through a proxy that refuses anything else. */
  egress_hosts?: string[];
  /** What a fork's preview (an untrusted head a writer approved) may reach when `egress` is `open`: `roll` (default) or `open`. */
  egress_forks?: "open" | "roll";
  /** Branch the data as of a point in time where the product keeps history: `-24h`, `-30m`, `-7d`, or an RFC 3339 time (30 days back at most). */
  data_at?: string | null;
  /** A feedback button on its pages for people who came in through roll (link access, or a team session). */
  feedback: boolean;
  /** Variables for a family of branches: a branch glob → `{name: value}`, over `env`. */
  env_for: Record<string, Record<string, string>>;
  /** Wake the change's services as soon as a new head is deployed (a warm first visit). */
  warm: boolean;
  /** Run once on fresh data (after branching or a reset) before the services deploy. */
  seed: PreviewJob | null;
  /** Run at each new head, after the seed and before services deploy (migrations). */
  release?: PreviewJob | null;
  /** Run once when the preview is deleted, before its services and branches go (cleanup outside roll). */
  teardown?: PreviewJob | null;
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

/** A deploy hook (a headless CMS's publish webhook): calling its URL builds the project's previews of `branches` again at their heads. */
export interface PreviewHook {
  id: string;
  name: string;
  branches: string[];
  created_at: number;
  fired_at: number | null;
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
/** What's at `path` (absolute) in a running `roll.toml` service, as a tar (`GET …/files`, admin): a
 * 404 says nothing is there, in the server's words. */
export async function downloadFiles(id: string, service: string, path: string): Promise<Blob> {
  const res = await fetch(`${ROLL_API}${one(id)}/files${query({ service, path })}`, withSession({ headers: apiHeaders(), cache: "no-store" }));
  onApiStatus(res.status);
  if (!res.ok) {
    const data = await res.json().catch(() => null);
    throw new Error(data?.error ?? `${res.status} ${res.statusText}`.trim());
  }
  return res.blob();
}

/** A service's link with the preview's access as a QR code (an object URL; revoke it when done): a phone opens it by scanning. */
export async function qrUrl(id: string, service?: string): Promise<string> {
  const res = await fetch(`${ROLL_API}${one(id)}/qr.png${service ? `?service=${encodeURIComponent(service)}` : ""}`, withSession({ headers: apiHeaders(), cache: "no-store" }));
  onApiStatus(res.status);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`.trim());
  return URL.createObjectURL(await res.blob());
}

/** A picture of a note's element, as a screenshot run cropped it (an object URL; revoke it when
 * done), or `null` while it has none. */
export async function noteCropUrl(id: string, note: string): Promise<string | null> {
  const res = await fetch(`${ROLL_API}${one(id)}/notes/${encodeURIComponent(note)}/crop`, withSession({ headers: apiHeaders(), cache: "no-store" }));
  onApiStatus(res.status);
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`.trim());
  return URL.createObjectURL(await res.blob());
}

export async function screenshotUrl(id: string, previous = false, phone = false, base = false, page?: string, diff?: "base" | "previous"): Promise<string> {
  const q = [previous ? "previous=1" : "", phone ? "view=phone" : "", base ? "base=1" : "", page ? `page=${encodeURIComponent(page)}` : "", diff ? `diff=${diff}` : ""].filter(Boolean).join("&");
  const res = await fetch(`${ROLL_API}${one(id)}/screenshot${q ? `?${q}` : ""}`, withSession({ headers: apiHeaders(), cache: "no-store" }));
  onApiStatus(res.status);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`.trim());
  return URL.createObjectURL(await res.blob());
}

export const previewsApi = {
  /** Newest first. */
  list: (filter: { project?: string; state?: PreviewState } = {}) => call<Preview[]>("GET", `/v1/previews${query(filter)}`),
  get: (id: string) => call<Preview>("GET", one(id)),
  /** A preview of a branch (no change) or of a change; the existing one if there is one. Made in the
   * project `X-Roll-Project` names, which roll-access decides a member's create by. */
  create: (projectId: string, repo: string, source: PreviewSource) =>
    call<Preview>("POST", "/v1/previews", { project: projectId, repo, ...source }, { "x-roll-project": projectId }),
  /** Marks it for deletion (`desired: "deleted"`); the reconciler removes it. */
  remove: (id: string) => call<Preview>("DELETE", one(id)),
  refresh: (id: string) => call<Preview>("POST", `${one(id)}/refresh`),
  /** Keep it past its TTL for `hours` (at most 720); 0 lets its rules decide again. */
  keep: (id: string, hours: number) => call<Preview>("POST", `${one(id)}/keep`, { hours }),
  /** Accept how a change's pages look at its latest screenshot (the `roll/visual` status). */
  /** `head`: the screenshot's that was looked at (`visual_review.head`): refused (409) once a newer one came in. `path`: that page only (`""` the first), else every page. */
  /** Let one head of a fork's pull request on a Git URL service run (409 once it moved on). */
  approve: (id: string, head: string) => call<Preview>("POST", `${one(id)}/approve`, { head }),
  /** As the person signed in (a session, never a token): a one-use URL into a team-access preview's page `to` (one of its service URLs), good for a minute. */
  visit: (id: string, to: string) => call<{ url: string }>("POST", `${one(id)}/visit`, { to }),
  acceptRegressions: (id: string, head?: string) => call<Preview>("POST", `${one(id)}/regressions/accept`, head ? { head } : {}),
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
    const res = await fetch(`${ROLL_API}${one(id)}/mail/${encodeURIComponent(mail)}`, withSession({ headers: apiHeaders(), cache: "no-store" }));
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
  /** Its latest requests through roll, newest first (roll's runtime; a roll apps instance's are roll apps'). */
  requests: (id: string) => call<{ requests: RequestLine[] }>("GET", `${one(id)}/requests`),
  /** What its running services use right now (cores of CPU, resident memory, the memory each runs with). */
  metrics: (id: string) => call<{ services: ServiceMetrics[] }>("GET", `${one(id)}/metrics`),
  /** Its services' errors in the last `minutes` (5–1440, 60 by default, rounded up to 15-minute steps), grouped, newest first; masked as its logs are. */
  errors: (id: string, minutes?: number) => call<{ minutes: number; groups: ErrorGroup[] }>("GET", `${one(id)}/errors${query({ minutes })}`),
  /** How many errors its services wrote in that span (a list's rows: nothing to mask, so no values are read). */
  errorCount: (id: string, minutes?: number) => call<{ minutes: number; count: number }>("GET", `${one(id)}/errors/count${query({ minutes })}`),
  /** A login shell in a running `roll.toml` service (started if it sleeps, kept awake while open; admin). */
  openShell: (id: string, body: { service?: string; cols: number; rows: number }) => call<{ id: string }>("POST", `${one(id)}/shell/sessions`, body),
  shellKeys: (id: string, sh: string, data: string) => call<null>("POST", `${one(id)}/shell/sessions/${encodeURIComponent(sh)}/keys`, { data }),
  resizeShell: (id: string, sh: string, size: { cols: number; rows: number }) => call<null>("POST", `${one(id)}/shell/sessions/${encodeURIComponent(sh)}/resize`, size),
  closeShell: (id: string, sh: string) => call<null>("DELETE", `${one(id)}/shell/sessions/${encodeURIComponent(sh)}`),
  /** Its recent traces (the project's `traces`), newest first, masked. */
  traces: (id: string, minutes?: number) => call<{ minutes: number; traces: TraceSummary[] }>("GET", `${one(id)}/traces${query({ minutes })}`),
  /** Each route's time from its traces against main's preview's same route, the slower ones first. */
  routes: (id: string, minutes?: number) => call<{ minutes: number; main: string | null; routes: RouteTime[] }>("GET", `${one(id)}/routes${query({ minutes })}`),
  /** One trace's spans, oldest first, masked. */
  trace: (id: string, trace: string) => call<{ trace: string; spans: TraceSpan[]; repeated?: Repeated | null }>("GET", `${one(id)}/traces/${encodeURIComponent(trace)}`),
  logs: (id: string, opts: { service?: string; lines?: number } = {}) =>
    call<{ service: string; lines: string[] }>("GET", `${one(id)}/logs${query(opts)}`),
  /** Newest first. */
  activity: (id: string) => call<PreviewActivity[]>("GET", `${one(id)}/activity`),
  visitors: (id: string) => call<PreviewVisitor[]>("GET", `${one(id)}/visitors`),
  settings: (projectId: string) => call<PreviewSettings>("GET", `${project(projectId)}/settings`),
  putSettings: (projectId: string, patch: SettingsPatch) => call<PreviewSettings>("PUT", `${project(projectId)}/settings`, patch),
  usage: (projectId: string) => call<PreviewUsage>("GET", `${project(projectId)}/usage`),
  /** Columns of the project's branched data that look like people's data and aren't masked. */
  maskSuggestions: (projectId: string) => call<MaskSuggestions>("GET", `${project(projectId)}/mask-suggestions`),
  bypass: (projectId: string) => call<PreviewBypass>("GET", `${project(projectId)}/bypass`),
  /** A new secret (the old one stops working), shown only in this answer. */
  rotateBypass: (projectId: string) => call<PreviewBypass>("POST", `${project(projectId)}/bypass`),
  deleteBypass: (projectId: string) => call<void>("DELETE", `${project(projectId)}/bypass`),
  hooks: (projectId: string) => call<{ hooks: PreviewHook[] }>("GET", `${project(projectId)}/hooks`),
  /** A new hook; `path` (its secret URL) is only in this answer. */
  createHook: (projectId: string, name: string, branches: string[]) => call<{ hook: PreviewHook; path: string }>("POST", `${project(projectId)}/hooks`, { name, branches }),
  deleteHook: (projectId: string, id: string) => call<void>("DELETE", `${project(projectId)}/hooks/${encodeURIComponent(id)}`),
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

/** One error in a preview's service's output, grouped (a stack trace joined from its lines, or an error line): how many, first and last seen (ms), and the newest one whole. `fingerprint` names the group (keyed per process: not stable across restarts). */
export type ErrorGroup = { service: string; fingerprint: string; kind: string; title: string; count: number; first_seen: number; last_seen: number; sample: string; /** The trace its newest sample names, when the preview keeps it. */ trace?: string };

/** A running service's use right now: cores of CPU, resident bytes (an instance's, on roll apps), and the MB it runs with. */
export type ServiceMetrics = { service: string; cpu: number; memory_bytes: number; memory_mb: number };

/** One of a preview's recent traces: its root span's name and service, when (ms), how long, how many spans, whether any failed. */
/** The span inside a slower route whose time grew most against main's preview's: where the time went. */
export type GrewSpan = { span: string; ms: number; main_ms: number };
/** A database statement a trace runs many times (an N+1), its literals taken out. */
export type Repeated = { statement: string; count: number };
export type TraceSummary = { trace: string; name: string; service: string; start_ms: number; ms: number; spans: number; error: boolean; repeated?: Repeated | null };
/** A route's time from a preview's traces (a root span's name): how many, p50 and p95 (ms), main's preview's p50 for it, and whether it's clearly slower. */
export type RouteTime = { name: string; count: number; p50_ms: number; p95_ms: number; main_p50_ms: number | null; main_count: number; slower: boolean; repeated?: Repeated | null; grew?: GrewSpan | null; /** Its budget (`github.budget.routes`), and whether its p50 is over it. */ budget_ms?: number | null; over_budget?: boolean };
/** A span of a trace (times in Unix nanoseconds), its attributes as name/value pairs. */
export type TraceSpan = { trace: string; span: string; parent?: string; name: string; service?: string; start: number; end: number; error?: boolean; attrs?: [string, string][] };

/** A request to a preview's service through roll: when (ms), the method, the path (never the query), the status, ms until the answer's headers. */
export type RequestLine = { at: number; service: string; method: string; path: string; status: number; ms: number };

/** A service's last `lines`, then its output as it comes (`…/logs?follow=1`, read with the
 * operator's headers, which EventSource can't send). Resolves when the stream ends or `signal`
 * aborts it. */
export async function followLogs(id: string, service: string, lines: number, onLine: (l: string, stderr: boolean) => void, signal: AbortSignal): Promise<void> {
  const res = await fetch(`${ROLL_API}${one(id)}/logs${query({ service, lines, follow: 1 })}`, withSession({ headers: apiHeaders(), cache: "no-store", signal }));
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
    // A stderr line is `event: stderr`; a stdout one the default event.
    for (const e of parsed.events) if (e.event === "message" || e.event === "stderr") onLine(e.data, e.event === "stderr");
  }
}

/** A preview's shell's screen: base64 chunks, the scrollback first, then `onClosed`. */
export function followShell(id: string, sh: string, opts: ScreenHandlers): () => void {
  return followScreen(`${ROLL_API}${one(id)}/shell/sessions/${encodeURIComponent(sh)}/screen`, () => apiHeaders() ?? {}, opts);
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
