import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  createCodeApi,
  dispatchEvent,
  firstLine,
  formatMergeRate,
  isNotFound,
  landOutcome,
  languageShares,
  shortGit,
  shortId,
  sortChanges,
  subscribe,
  validOwnerName,
  validRepoName,
  type Change,
} from "./code-api";

type Call = { url: string; init: RequestInit };

/** A fetch that records calls and answers from `respond`. */
function mockFetch(respond: (url: string, init: RequestInit) => Response) {
  const calls: Call[] = [];
  const fn = vi.fn(async (url: string | URL | Request, init: RequestInit = {}) => {
    calls.push({ url: String(url), init });
    return respond(String(url), init);
  });
  vi.stubGlobal("fetch", fn);
  return calls;
}

const json = (status: number, body: unknown, headers: Record<string, string> = {}) =>
  new Response(body === null ? null : JSON.stringify(body), { status, headers: { "content-type": "application/json", ...headers } });

afterEach(() => vi.unstubAllGlobals());

describe("requests", () => {
  it("builds paths, methods, bodies and headers", async () => {
    const calls = mockFetch(() => json(200, {}));
    const api = createCodeApi("http://api", "rlc_t");
    await api.repo("acme/web");
    await api.tree("acme/web", "release/1.0", "src/a b/c.ts");
    await api.log("acme/web", { path: "src", limit: 5, cursor: null });
    await api.compare("acme/web", "cs_1", "main");
    await api.land("acme/web", { base: "main", message: "m", files: { "a.txt": { content: "x" } } });
    await api.createOwner("acme", "org");
    await api.claim("acme/web", "agent-1", { labels: ["ui"], lease_s: 60 });
    await api.change("acme/web", 7);
    await api.change("acme/web", "chg_abc");
    await api.readme("acme/web", "main", "docs/README.md");
    await api.repos();
    expect(calls.map((c) => [c.init.method, c.url])).toEqual([
      ["GET", "http://api/v1/code/repos/acme/web"],
      ["GET", "http://api/v1/code/repos/acme/web/tree/release%2F1.0/src/a%20b/c.ts"],
      ["GET", "http://api/v1/code/repos/acme/web/log?path=src&limit=5"],
      ["GET", "http://api/v1/code/repos/acme/web/compare/cs_1...main"],
      ["POST", "http://api/v1/code/repos/acme/web/land"],
      ["POST", "http://api/v1/code/owners"],
      ["POST", "http://api/v1/code/repos/acme/web/work/claim"],
      ["GET", "http://api/v1/code/repos/acme/web/changes/7"],
      ["GET", "http://api/v1/code/changes/chg_abc"],
      ["GET", "http://api/v1/code/repos/acme/web/readme/main?path=docs%2FREADME.md"],
      ["GET", "http://api/v1/code/repos"],
    ]);
    const land = calls[4].init;
    expect((land.headers as Record<string, string>)["content-type"]).toBe("application/json");
    expect((land.headers as Record<string, string>).authorization).toBe("Bearer rlc_t");
    expect(JSON.parse(land.body as string)).toEqual({ base: "main", message: "m", files: { "a.txt": { content: "x" } } });
    expect(JSON.parse(calls[6].init.body as string)).toEqual({ agent: "agent-1", labels: ["ui"], lease_s: 60 });
    expect((calls[0].init.headers as Record<string, string>)["content-type"]).toBeUndefined();
    expect(calls.every((c) => c.init.cache === "no-store")).toBe(true);
  });

  it("reads the token on every request and runs `before` first", async () => {
    const calls = mockFetch(() => json(200, {}));
    let token: string | undefined;
    const order: string[] = [];
    const api = createCodeApi("http://api", () => token, async () => void order.push("before"));
    await api.info();
    token = "rlc_new";
    await api.info();
    expect((calls[0].init.headers as Record<string, string>).authorization).toBeUndefined();
    expect((calls[1].init.headers as Record<string, string>).authorization).toBe("Bearer rlc_new");
    expect(order).toEqual(["before", "before"]);
    expect(api.authHeaders()).toEqual({ authorization: "Bearer rlc_new" });
  });
});

describe("responses", () => {
  it("throws ApiError with the server's message and status", async () => {
    mockFetch(() => json(422, { error: "dashboard is reserved" }));
    const err = await createCodeApi("http://api").createOwner("dashboard", "user").catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(422);
    expect(err.message).toBe("dashboard is reserved");
    expect(isNotFound(err)).toBe(false);
  });

  it("names a status when the body isn't JSON", async () => {
    mockFetch(() => new Response("gateway down", { status: 502, statusText: "Bad Gateway" }));
    const err = await createCodeApi("http://api").info().catch((e) => e);
    expect(err.status).toBe(502);
    expect(err.message).toBe("502 Bad Gateway");
    expect(err.body).toBe("gateway down");
  });

  it("tells 404s apart and reports 401s", async () => {
    mockFetch((url) => (url.endsWith("/nope") ? json(404, { error: "repository acme/nope not found" }) : json(401, { error: "authentication required" })));
    const unauthorized = vi.fn();
    const api = createCodeApi("http://api", undefined, undefined, unauthorized);
    expect(isNotFound(await api.repo("acme/nope").catch((e) => e))).toBe(true);
    expect(unauthorized).not.toHaveBeenCalled();
    await expect(api.createWork("acme/web", { title: "x" })).rejects.toMatchObject({ status: 401 });
    expect(unauthorized).toHaveBeenCalledTimes(1);
  });

  it("turns a 409 land into a conflict result", async () => {
    mockFetch(() => json(409, { status: "conflict", error: "conflict on notes.txt", paths: ["notes.txt"], head: "cs_h", ref: "main" }));
    const r = await createCodeApi("http://api").land("acme/web", { changeset: "cs_x" });
    expect(r).toEqual({ status: "conflict", error: "conflict on notes.txt", paths: ["notes.txt"], head: "cs_h" });
    const r2 = await createCodeApi("http://api").landChange("chg_1");
    expect(r2.status).toBe("conflict");
  });

  it("keeps other 409s as errors", async () => {
    mockFetch(() => json(409, { error: "change 3 already landed" }));
    await expect(createCodeApi("http://api").landChange("chg_1")).rejects.toMatchObject({ status: 409 });
  });

  it("returns null for a 204 claim (no work)", async () => {
    mockFetch(() => new Response(null, { status: 204 }));
    expect(await createCodeApi("http://api").claim("acme/web", "a")).toBeNull();
  });

  it("reads raw blobs with the changeset header, null on 404", async () => {
    mockFetch((url) =>
      url.includes("missing") ? new Response(null, { status: 404 }) : new Response(new Uint8Array([0, 1, 2]), { status: 200, headers: { "x-roll-changeset": "cs_abc" } }),
    );
    const api = createCodeApi("http://api");
    expect(await api.blob("acme/web", "main", "missing.bin")).toBeNull();
    const b = await api.blob("acme/web", "main", "logo.bin");
    expect(Array.from(b!.bytes)).toEqual([0, 1, 2]);
    expect(b!.changeset).toBe("cs_abc");
    expect(api.blobUrl("acme/web", "main", "a/b c.txt")).toBe("http://api/v1/code/repos/acme/web/blob/main/a/b%20c.txt");
    expect(api.eventsUrl("acme/web")).toBe("http://api/v1/code/repos/acme/web/events");
  });
});

describe("merge queue", () => {
  it("builds the queue and change-action requests", async () => {
    const calls = mockFetch(() => json(200, {}));
    const api = createCodeApi("http://api");
    await api.queue("acme/web");
    await api.pauseQueue("acme/web");
    await api.resumeQueue("acme/web");
    await api.audit("acme/web");
    await api.enqueue("chg_1");
    await api.enqueue("chg_1", "urgent");
    await api.dequeue("chg_1");
    await api.approveRun("chg_1");
    await api.updateChange("chg_1", { draft: false });
    await api.landChange("chg_1");
    await api.landChange("chg_1", { bypass: true });
    await api.land("acme/web", { changeset: "cs_x", bypass: true });
    expect(calls.map((c) => [c.init.method, c.url, c.init.body ?? null])).toEqual([
      ["GET", "http://api/v1/code/repos/acme/web/queue", null],
      ["POST", "http://api/v1/code/repos/acme/web/queue/pause", null],
      ["POST", "http://api/v1/code/repos/acme/web/queue/resume", null],
      ["GET", "http://api/v1/code/repos/acme/web/audit", null],
      ["POST", "http://api/v1/code/changes/chg_1/enqueue", "{}"],
      ["POST", "http://api/v1/code/changes/chg_1/enqueue", '{"lane":"urgent"}'],
      ["DELETE", "http://api/v1/code/changes/chg_1/enqueue", null],
      ["POST", "http://api/v1/code/changes/chg_1/approve-run", null],
      ["PATCH", "http://api/v1/code/changes/chg_1", '{"draft":false}'],
      ["POST", "http://api/v1/code/changes/chg_1/land", null],
      ["POST", "http://api/v1/code/changes/chg_1/land", '{"bypass":true}'],
      ["POST", "http://api/v1/code/repos/acme/web/land", '{"changeset":"cs_x","bypass":true}'],
    ]);
  });

  it("returns a 202 as a queued result", async () => {
    const change = { id: "chg_1", state: "queued" };
    mockFetch(() => json(202, { status: "queued", position: 3, queue_id: "qe_1", change }));
    const api = createCodeApi("http://api");
    expect(await api.landChange("chg_1")).toEqual({ status: "queued", position: 3, queue_id: "qe_1", change });
    expect(await api.enqueue("chg_1")).toMatchObject({ status: "queued", position: 3 });
    expect(await api.land("acme/web", { base: "main", message: "m", files: {} })).toMatchObject({ status: "queued" });
  });

  it("turns a 422 with blockers into a blocked result", async () => {
    const blockers = [{ kind: "draft", message: "change 1 is a draft" }];
    mockFetch(() => json(422, { error: "change 1 can't be queued", blockers }));
    const api = createCodeApi("http://api");
    expect(await api.enqueue("chg_1")).toEqual({ status: "blocked", error: "change 1 can't be queued", blockers });
    expect(await api.landChange("chg_1")).toMatchObject({ status: "blocked" });
  });

  it("keeps a 403 bypass and other 422s as errors", async () => {
    mockFetch((url) =>
      url.endsWith("/land") ? json(403, { error: "bypassing the merge queue needs the admin token" }) : json(422, { error: "lane must be urgent or normal" }),
    );
    const api = createCodeApi("http://api");
    await expect(api.landChange("chg_1", { bypass: true })).rejects.toMatchObject({ status: 403 });
    await expect(api.enqueue("chg_1")).rejects.toMatchObject({ status: 422 });
  });

  it("settles only conflicts and blockers", () => {
    expect(landOutcome(new ApiError("x", 409, { paths: ["a"], head: "cs_h" }))).toEqual({ status: "conflict", error: null, paths: ["a"], head: "cs_h" });
    expect(landOutcome(new ApiError("blocked", 422, { blockers: [] }))).toEqual({ status: "blocked", error: "blocked", blockers: [] });
    expect(landOutcome(new ApiError("x", 409, { error: "already landed" }))).toBeNull();
    expect(landOutcome(new ApiError("x", 422, { error: "bad" }))).toBeNull();
    expect(landOutcome(new Error("x"))).toBeNull();
  });
});

describe("reviews", () => {
  it("builds every review, thread, settings and review-app request", async () => {
    const calls = mockFetch(() => json(200, {}));
    const api = createCodeApi("http://api");
    await api.reviews("chg_1");
    await api.submitReview("chg_1", { verdict: "request_changes", body: "no", comments: [{ path: "a.rs", line: 3, start_line: 1, side: "new", body: "x" }] });
    await api.dismissReview("chg_1", "rv_1", "stale");
    await api.dismissReview("chg_1", "rv_1");
    await api.reviewStatus("chg_1");
    await api.threads("chg_1");
    await api.createThread("chg_1", { path: "src/a b.rs", line: 2, body: "hm" });
    await api.thread("th_1");
    await api.reply("th_1", "ok");
    await api.resolveThread("th_1");
    await api.unresolveThread("th_1");
    await api.applySuggestion("th_1");
    await api.applySuggestion("th_1", "cm_2");
    await api.reviewRequests("chg_1");
    await api.requestReview("chg_1", "roll");
    await api.requestReview("chg_1", "@ana", true);
    await api.reviewSettings("acme/web");
    await api.updateReviewSettings("acme/web", { required_approvals: 2 });
    await api.moveLane("chg_1", "urgent");
    expect(calls.map((c) => [c.init.method, c.url, c.init.body ?? null])).toEqual([
      ["GET", "http://api/v1/code/changes/chg_1/reviews", null],
      [
        "POST",
        "http://api/v1/code/changes/chg_1/reviews",
        '{"verdict":"request_changes","body":"no","comments":[{"path":"a.rs","line":3,"start_line":1,"side":"new","body":"x"}]}',
      ],
      ["POST", "http://api/v1/code/changes/chg_1/reviews/rv_1/dismiss", '{"reason":"stale"}'],
      ["POST", "http://api/v1/code/changes/chg_1/reviews/rv_1/dismiss", "{}"],
      ["GET", "http://api/v1/code/changes/chg_1/review-status", null],
      ["GET", "http://api/v1/code/changes/chg_1/threads", null],
      ["POST", "http://api/v1/code/changes/chg_1/threads", '{"path":"src/a b.rs","line":2,"body":"hm"}'],
      ["GET", "http://api/v1/code/threads/th_1", null],
      ["POST", "http://api/v1/code/threads/th_1/comments", '{"body":"ok"}'],
      ["POST", "http://api/v1/code/threads/th_1/resolve", null],
      ["POST", "http://api/v1/code/threads/th_1/unresolve", null],
      ["POST", "http://api/v1/code/threads/th_1/apply", "{}"],
      ["POST", "http://api/v1/code/threads/th_1/apply", '{"comment":"cm_2"}'],
      ["GET", "http://api/v1/code/changes/chg_1/review-requests", null],
      ["POST", "http://api/v1/code/changes/chg_1/review-requests", '{"reviewer":"roll"}'],
      ["POST", "http://api/v1/code/changes/chg_1/review-requests", '{"reviewer":"@ana","required":true}'],
      ["GET", "http://api/v1/code/repos/acme/web/review-settings", null],
      ["PATCH", "http://api/v1/code/repos/acme/web/review-settings", '{"required_approvals":2}'],
      ["PATCH", "http://api/v1/code/changes/chg_1/enqueue", '{"lane":"urgent"}'],
    ]);
  });

  it("builds the review-app requests", async () => {
    const calls = mockFetch(() => json(200, {}));
    const api = createCodeApi("http://api");
    await api.reviewApps("acme");
    await api.createReviewApp("acme", { name: "greptile", webhook_url: "https://g.dev/hook", events: ["change.opened"], required: true });
    await api.reviewApp("app_1");
    await api.updateReviewApp("app_1", { active: false });
    await api.rotateReviewApp("app_1");
    await api.deliveries("app_1");
    await api.redeliver("app_1", "dl_1");
    await api.deleteReviewApp("app_1");
    expect(calls.map((c) => [c.init.method, c.url, c.init.body ?? null])).toEqual([
      ["GET", "http://api/v1/code/owners/acme/review-apps", null],
      ["POST", "http://api/v1/code/owners/acme/review-apps", '{"name":"greptile","webhook_url":"https://g.dev/hook","events":["change.opened"],"required":true}'],
      ["GET", "http://api/v1/code/review-apps/app_1", null],
      ["PATCH", "http://api/v1/code/review-apps/app_1", '{"active":false}'],
      ["POST", "http://api/v1/code/review-apps/app_1/rotate", null],
      ["GET", "http://api/v1/code/review-apps/app_1/deliveries", null],
      ["POST", "http://api/v1/code/review-apps/app_1/deliveries/dl_1/redeliver", null],
      ["DELETE", "http://api/v1/code/review-apps/app_1", null],
    ]);
  });

  it("keeps review errors as errors (a 409 on a landed change isn't a land result)", async () => {
    mockFetch(() => json(409, { error: "change 3 has landed" }));
    const api = createCodeApi("http://api");
    await expect(api.submitReview("chg_1", { verdict: "approve" })).rejects.toMatchObject({ status: 409, message: "change 3 has landed" });
  });
});

describe("events", () => {
  it("routes queue events", () => {
    const queue = vi.fn();
    const ev = { state: "testing", paused: false, depth: 4, batch: null };
    dispatchEvent({ queue }, "queue", JSON.stringify(ev));
    dispatchEvent({ queue }, "queue", "{broken");
    expect(queue).toHaveBeenCalledExactlyOnceWith(ev);
  });

  it("routes review and thread events", () => {
    const review = vi.fn();
    const thread = vi.fn();
    dispatchEvent({ review, thread }, "review", '{"id":"rv_1","verdict":"approve"}');
    dispatchEvent({ review, thread }, "thread", '{"id":"th_1","resolved":true}');
    dispatchEvent({ review, thread }, "thread", "{oops");
    expect(review).toHaveBeenCalledExactlyOnceWith({ id: "rv_1", verdict: "approve" });
    expect(thread).toHaveBeenCalledExactlyOnceWith({ id: "th_1", resolved: true });
  });

  it("routes known kinds and drops the rest", () => {
    const landed = vi.fn();
    const work = vi.fn();
    dispatchEvent({ landed, work }, "landed", JSON.stringify({ changeset: "cs_1" }));
    dispatchEvent({ landed, work }, "work", '{"id":"wi_1"}');
    dispatchEvent({ landed, work }, "change", '{"id":"chg_1"}');
    dispatchEvent({ landed, work }, "bogus", "{}");
    dispatchEvent({ landed, work }, "landed", "{not json");
    expect(landed).toHaveBeenCalledExactlyOnceWith({ changeset: "cs_1" });
    expect(work).toHaveBeenCalledExactlyOnceWith({ id: "wi_1" });
  });

  it("subscribes over fetch with the token and stops on unsubscribe", async () => {
    const enc = new TextEncoder();
    let cancelled = false;
    const calls = mockFetch(
      () =>
        new Response(
          new ReadableStream({
            start(c) {
              c.enqueue(enc.encode(": keepalive\n\nevent: landed\ndata: {\"changeset\":\"cs_9\"}\n\n"));
            },
            cancel() {
              cancelled = true;
            },
          }),
          { status: 200, headers: { "content-type": "text/event-stream" } },
        ),
    );
    const api = createCodeApi("http://api", "rlc_s");
    const got: unknown[] = [];
    const opened: boolean[] = [];
    const stop = subscribe("acme/web", { landed: (e) => got.push(e), open: (o) => opened.push(o) }, api);
    await vi.waitFor(() => expect(got).toEqual([{ changeset: "cs_9" }]));
    expect(opened).toEqual([true]);
    expect(calls[0].url).toBe("http://api/v1/code/repos/acme/web/events");
    expect((calls[0].init.headers as Record<string, string>).authorization).toBe("Bearer rlc_s");
    stop();
    await vi.waitFor(() => expect(cancelled || (calls[0].init.signal as AbortSignal).aborted).toBe(true));
  });
});

describe("names and formatting", () => {
  it("validates owner names like the server", () => {
    for (const ok of ["a", "acme", "roll-labs", "a1", "a".repeat(39)]) expect(validOwnerName(ok), ok).toBe(true);
    for (const bad of ["", "-a", "a-", "a--b", "A", "a_b", "a.b", "a".repeat(40)]) expect(validOwnerName(bad), bad).toBe(false);
  });

  it("validates repo names like the server", () => {
    for (const ok of ["web", "My.Repo_1-x", "a".repeat(100)]) expect(validRepoName(ok), ok).toBe(true);
    for (const bad of ["", ".", "..", "x.git", "X.GIT", "a/b", "a b", "a".repeat(101)]) expect(validRepoName(bad), bad).toBe(false);
  });

  it("formats ids, messages and rates", () => {
    expect(shortId(`cs_${"a".repeat(64)}`)).toBe(`cs_${"a".repeat(12)}`);
    expect(shortId(null)).toBe("—");
    expect(shortGit("0123456789abcdef")).toBe("0123456");
    expect(shortGit(undefined)).toBe("—");
    expect(firstLine("  subject  \n\nbody")).toBe("subject");
    expect(firstLine("only")).toBe("only");
    expect([0, 0.05, 1.234, 12.6, 4321, null, Number.NaN].map((n) => formatMergeRate(n))).toEqual(["0/s", "0.05/s", "1.2/s", "13/s", "4,321/s", "—", "—"]);
  });

  it("sorts changes in play first, newest first within", () => {
    const c = (id: string, state: Change["state"], updated_at: number) => ({ id, state, updated_at }) as Change;
    const sorted = sortChanges([c("a", "landed", 5), c("b", "open", 1), c("c", "conflict", 0), c("d", "open", 9), c("e", "closed", 99), c("f", "queued", 2)]);
    expect(sorted.map((x) => x.id)).toEqual(["c", "f", "d", "b", "a", "e"]);
  });

  it("computes language shares", () => {
    expect(languageShares({ Rust: 300, TypeScript: 100, Empty: 0 })).toEqual([
      { name: "Rust", bytes: 300, share: 0.75 },
      { name: "TypeScript", bytes: 100, share: 0.25 },
    ]);
    expect(languageShares({})).toEqual([]);
  });
});

describe("roll's reviewer, billing and search", () => {
  it("builds the reviewer, learning, search, pricing, provider, billing and usage requests", async () => {
    const calls = mockFetch(() => json(200, {}));
    const api = createCodeApi("http://api", "rlc_t");
    await api.reviewer("acme/web");
    await api.updateReviewer("acme/web", { mode: "deep", speed: "fast" });
    await api.runAiReview("chg_1", { mode: "max", full: true });
    await api.runAiReview("chg_1");
    await api.aiJobs("chg_1");
    await api.learnings("acme/web");
    await api.addLearning("acme/web", { text: "don't flag unwrap in tests", path: "tests/**" });
    await api.deleteLearning("acme/web", "lrn_1");
    await api.search("acme/web", { q: "fn main", regex: true, path: "src/**", limit: 50 });
    await api.search("acme/web", { q: "a&b" });
    await api.pricing();
    await api.setReviewProvider("acme", { provider: "openai_compatible", api_key: "sk-x", base_url: "https://or/api/v1", model: "m" });
    await api.clearReviewProvider("acme");
    await api.billing("acme");
    await api.setBilling("acme", { monthly_cap_usd: null, on_cap: "standard_only" });
    await api.usage("acme", "2026-10");
    await api.usage("acme");
    expect(calls.map((c) => [c.init.method, c.url])).toEqual([
      ["GET", "http://api/v1/code/repos/acme/web/reviewer"],
      ["PATCH", "http://api/v1/code/repos/acme/web/reviewer"],
      ["POST", "http://api/v1/code/changes/chg_1/ai-review"],
      ["POST", "http://api/v1/code/changes/chg_1/ai-review"],
      ["GET", "http://api/v1/code/changes/chg_1/ai-jobs"],
      ["GET", "http://api/v1/code/repos/acme/web/learnings"],
      ["POST", "http://api/v1/code/repos/acme/web/learnings"],
      ["DELETE", "http://api/v1/code/repos/acme/web/learnings/lrn_1"],
      ["GET", "http://api/v1/code/repos/acme/web/search?q=fn+main&regex=true&path=src%2F**&limit=50"],
      ["GET", "http://api/v1/code/repos/acme/web/search?q=a%26b"],
      ["GET", "http://api/v1/code/pricing"],
      ["PUT", "http://api/v1/code/owners/acme/review-provider"],
      ["DELETE", "http://api/v1/code/owners/acme/review-provider"],
      ["GET", "http://api/v1/code/owners/acme/billing"],
      ["PUT", "http://api/v1/code/owners/acme/billing"],
      ["GET", "http://api/v1/code/owners/acme/usage?month=2026-10"],
      ["GET", "http://api/v1/code/owners/acme/usage"],
    ]);
    expect(JSON.parse(String(calls[1].init.body))).toEqual({ mode: "deep", speed: "fast" });
    expect(JSON.parse(String(calls[2].init.body))).toEqual({ mode: "max", full: true });
    expect(JSON.parse(String(calls[3].init.body))).toEqual({});
    expect(JSON.parse(String(calls[6].init.body))).toEqual({ text: "don't flag unwrap in tests", path: "tests/**" });
    // Clearing the cap sends an explicit null, not a missing field.
    expect(JSON.parse(String(calls[14].init.body))).toEqual({ monthly_cap_usd: null, on_cap: "standard_only" });
  });

  it("treats a missing provider as hosted, and other errors as errors", async () => {
    mockFetch(() => json(404, { error: "no review provider" }));
    expect(await createCodeApi("http://api").reviewProvider("acme")).toBeNull();
    // The owner's own read: 200 with a null provider means hosted too.
    const hosted = { owner: "acme", provider: null, base_url: null, model: null, secondary_model: null, key_last4: null, updated_at: null };
    mockFetch(() => json(200, hosted));
    expect(await createCodeApi("http://api").reviewProvider("acme")).toBeNull();
    mockFetch(() => json(200, { ...hosted, provider: "anthropic", key_last4: "9999", updated_at: 1 }));
    expect(await createCodeApi("http://api").reviewProvider("acme")).toMatchObject({ provider: "anthropic", key_last4: "9999" });
    mockFetch(() => json(403, { error: "not yours" }));
    await expect(createCodeApi("http://api").reviewProvider("acme")).rejects.toMatchObject({ status: 403 });
  });

  it("surfaces a bad regex as a 400 ApiError", async () => {
    mockFetch(() => json(400, { error: "bad regex: unclosed group" }));
    const err = await createCodeApi("http://api")
      .search("acme/web", { q: "(", regex: true })
      .catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(400);
    expect(err.message).toBe("bad regex: unclosed group");
  });
});

describe("blame, webhooks, statuses, tokens and MCP", () => {
  it("builds every request", async () => {
    const calls = mockFetch(() => json(200, {}));
    const api = createCodeApi("http://api", "rlc_t");
    await api.blame("acme/web", "release/1.0", "src/a b.ts");
    await api.search("acme/web", { q: "fn main", rev: "chg-head/x" });
    await api.webhooks("acme/web");
    await api.createWebhook("acme/web", { url: "https://ci.dev/h", events: ["landed", "push"] });
    await api.updateWebhook("acme/web", "wh_1", { active: false });
    await api.deleteWebhook("acme/web", "wh_1");
    await api.pingWebhook("acme/web", "wh_1");
    await api.webhookDeliveries("acme/web", "wh_1");
    await api.redeliverWebhook("acme/web", "wh_1", "dl_9");
    await api.postStatus("acme/web", "0123abc", { state: "pending", context: "ci/teamcity" });
    await api.statuses("acme/web", "cs_1");
    await api.createToken("acme", "agent");
    expect(calls.map((c) => [c.init.method, c.url])).toEqual([
      ["GET", "http://api/v1/code/repos/acme/web/blame/release%2F1.0/src/a%20b.ts"],
      ["GET", "http://api/v1/code/repos/acme/web/search?q=fn+main&rev=chg-head%2Fx"],
      ["GET", "http://api/v1/code/repos/acme/web/webhooks"],
      ["POST", "http://api/v1/code/repos/acme/web/webhooks"],
      ["PATCH", "http://api/v1/code/repos/acme/web/webhooks/wh_1"],
      ["DELETE", "http://api/v1/code/repos/acme/web/webhooks/wh_1"],
      ["POST", "http://api/v1/code/repos/acme/web/webhooks/wh_1/ping"],
      ["GET", "http://api/v1/code/repos/acme/web/webhooks/wh_1/deliveries"],
      ["POST", "http://api/v1/code/repos/acme/web/webhooks/wh_1/deliveries/dl_9/redeliver"],
      ["POST", "http://api/v1/code/repos/acme/web/statuses/0123abc"],
      ["GET", "http://api/v1/code/repos/acme/web/statuses/cs_1"],
      ["POST", "http://api/v1/code/owners/acme/tokens"],
    ]);
    expect(JSON.parse(String(calls[3].init.body))).toEqual({ url: "https://ci.dev/h", events: ["landed", "push"] });
    expect(JSON.parse(String(calls[4].init.body))).toEqual({ active: false });
    expect(JSON.parse(String(calls[9].init.body))).toEqual({ state: "pending", context: "ci/teamcity" });
    expect(JSON.parse(String(calls[11].init.body))).toEqual({ name: "agent" });
    expect(api.mcpUrl()).toBe("http://api/v1/code/mcp");
  });

  it("routes push events and both kinds of check event", () => {
    const push = vi.fn();
    const check = vi.fn();
    const pushed = { ref: "refs/heads/fix", before: null, after: "abc", pusher: "ana", accepted: true, reason: null };
    dispatchEvent({ push, check }, "push", JSON.stringify(pushed));
    dispatchEvent({ push, check }, "check", '{"change":"chg_1","check":{"name":"t"}}');
    dispatchEvent({ push, check }, "check", '{"changeset":"cs_1","status":{"context":"ci","state":"success"}}');
    expect(push).toHaveBeenCalledExactlyOnceWith(pushed);
    expect(check).toHaveBeenCalledTimes(2);
    expect(check.mock.calls[1][0]).toEqual({ changeset: "cs_1", status: { context: "ci", state: "success" } });
  });
});

describe("owner tokens", () => {
  it("lists and revokes by id", async () => {
    const calls = mockFetch((_, init) => (init.method === "DELETE" ? new Response(null, { status: 204 }) : json(200, [])));
    const api = createCodeApi("http://api");
    await api.tokens("acme");
    await api.revokeToken("acme", "tok_1");
    expect(calls.map((c) => [c.init.method, c.url])).toEqual([
      ["GET", "http://api/v1/code/owners/acme/tokens"],
      ["DELETE", "http://api/v1/code/owners/acme/tokens/tok_1"],
    ]);
  });
});
