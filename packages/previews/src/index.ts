/**
 * For a team's own tests against a roll preview (docs/previews.md, "Automation" and "Snapshots
 * from the team's own tests"): the project's bypass secret sent to the preview's own origin only,
 * and pictures for the preview's visual review. No dependency: Playwright's page is taken by the
 * few methods used.
 */

/** The header the project's bypass secret goes in. */
export const BYPASS_HEADER = "x-roll-preview-bypass";

/** What of a Playwright `Route` the bypass uses. */
export interface RouteLike {
  request(): { headers(): Record<string, string> };
  continue(options: { headers: Record<string, string> }): Promise<void>;
}

/** What of a Playwright `Page` these use. */
export interface PageLike {
  url(): string;
  route(url: string, handler: (route: RouteLike) => Promise<void> | void): Promise<void>;
  screenshot(options: { fullPage?: boolean; animations?: "disabled" | "allow" }): Promise<Uint8Array>;
}

export interface SnapshotOptions {
  /** The project's bypass secret (default: `ROLL_PREVIEW_BYPASS`). */
  secret?: string;
  /** The head the test ran against, a changeset or git commit (default: `GITHUB_SHA`): roll refuses a picture of another head. */
  head?: string;
  /** Where the preview is (default: the page's own origin). */
  baseURL?: string;
}

/** What roll answered: the page of the visual review and the head it's kept at. */
export interface Snapshot {
  page: string;
  head: string;
}

/** A variable of the process the tests run in (Node), if there is one. */
const env = (k: string): string | undefined => (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env[k] || undefined;

/**
 * The project's bypass secret on every request to the preview's own origin (`baseURL`'s), and on
 * none elsewhere: a page can send a header set for the whole browser anywhere.
 */
export async function bypass(page: PageLike, baseURL: string, secret: string | undefined = env("ROLL_PREVIEW_BYPASS")): Promise<void> {
  if (!secret) throw new Error("bypass: no secret (set ROLL_PREVIEW_BYPASS)");
  const origin = new URL(baseURL).origin;
  await page.route(`${origin}/**`, (route) => route.continue({ headers: { ...route.request().headers(), [BYPASS_HEADER]: secret } }));
}

/** A picture (PNG bytes) sent as page `snap:<name>` of the preview's visual review. */
export async function sendSnapshot(baseURL: string, name: string, png: Uint8Array, options: SnapshotOptions = {}): Promise<Snapshot> {
  const secret = options.secret ?? env("ROLL_PREVIEW_BYPASS");
  if (!secret) throw new Error("snapshot: no secret (set ROLL_PREVIEW_BYPASS)");
  const url = new URL("/__roll/snapshot", new URL(baseURL).origin);
  url.searchParams.set("name", name);
  const head = options.head ?? env("GITHUB_SHA");
  if (head) url.searchParams.set("head", head);
  const r = await fetch(url, { method: "POST", headers: { [BYPASS_HEADER]: secret, "content-type": "image/png" }, body: new Blob([new Uint8Array(png)], { type: "image/png" }), redirect: "error" });
  const text = await r.text();
  if (r.status !== 201) throw new Error(`snapshot ${name}: ${r.status} ${text.slice(0, 300)}`);
  return JSON.parse(text) as Snapshot;
}

/** The page as it is now (whole, animations stopped), sent as `snap:<name>`. */
export async function snapshot(page: PageLike, name: string, options: SnapshotOptions = {}): Promise<Snapshot> {
  const png = await page.screenshot({ fullPage: true, animations: "disabled" });
  return sendSnapshot(options.baseURL ?? page.url(), name, png, options);
}
