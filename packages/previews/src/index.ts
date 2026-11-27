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
  /**
   * The head the test ran against, a changeset or git commit: roll refuses a picture of another
   * head. By default `ROLL_PREVIEW_HEAD`, then a pull request event's head commit (on
   * `pull_request` workflows `GITHUB_SHA` is GitHub's merge commit, never the head), then
   * `GITHUB_SHA`.
   */
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

/** The head a CI run tested: see [`SnapshotOptions.head`]. */
export async function ciHead(): Promise<string | undefined> {
  const own = env("ROLL_PREVIEW_HEAD");
  if (own) return own;
  const path = env("GITHUB_EVENT_PATH");
  if (path && /^pull_request/.test(env("GITHUB_EVENT_NAME") ?? "")) {
    try {
      // Named at run time: the package has no Node types, and runs only where Node is.
      const fs = "node:fs/promises";
      const { readFile } = (await import(fs)) as { readFile(path: string, encoding: "utf8"): Promise<string> };
      const sha = JSON.parse(await readFile(path, "utf8"))?.pull_request?.head?.sha;
      if (typeof sha === "string" && /^[0-9a-f]{40}$/.test(sha)) return sha;
    } catch {
      // No event file to read: GITHUB_SHA, as on other events.
    }
  }
  return env("GITHUB_SHA");
}

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
  const head = options.head ?? (await ciHead());
  if (head) url.searchParams.set("head", head);
  const r = await fetch(url, { method: "POST", headers: { [BYPASS_HEADER]: secret, "content-type": "image/png" }, body: new Blob([new Uint8Array(png)], { type: "image/png" }), redirect: "error" });
  const text = await r.text();
  if (r.status !== 201) {
    // A picture of another head than the preview's: say which head the test sent, and from where.
    const said = r.status === 409 && head ? ` (sent head ${head}${env("GITHUB_EVENT_NAME") ? ` on a ${env("GITHUB_EVENT_NAME")} event` : ""}; set ROLL_PREVIEW_HEAD or the head option to the commit the preview runs)` : "";
    throw new Error(`snapshot ${name}: ${r.status} ${text.slice(0, 300)}${said}`);
  }
  return JSON.parse(text) as Snapshot;
}

/** The page as it is now (whole, animations stopped), sent as `snap:<name>`. */
export async function snapshot(page: PageLike, name: string, options: SnapshotOptions = {}): Promise<Snapshot> {
  const png = await page.screenshot({ fullPage: true, animations: "disabled" });
  return sendSnapshot(options.baseURL ?? page.url(), name, png, options);
}
