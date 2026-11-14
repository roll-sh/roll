import { afterEach, describe, expect, it, vi } from "vitest";
import { a11yWords, parseSse, previewsApi, unmaskedNote } from "./previews-api";
import { ROLL_API } from "./roll-api";

/** A fetch that answers from a table (`METHOD /path?query`) and records what it was asked. */
function fakeFetch(answers: Record<string, [number, unknown]>) {
  const asked: { method: string; path: string; body: unknown }[] = [];
  const f = vi.fn(async (url: string, init: RequestInit) => {
    const method = init.method ?? "GET";
    const path = url.slice(ROLL_API.length);
    asked.push({ method, path, body: init.body ? JSON.parse(String(init.body)) : undefined });
    const [status, body] = answers[`${method} ${path}`] ?? [404, { error: "not here" }];
    return new Response(JSON.stringify(body), { status });
  });
  vi.stubGlobal("fetch", f);
  return asked;
}

afterEach(() => vi.unstubAllGlobals());

describe("previews api", () => {
  it("speaks the documented routes and bodies", async () => {
    const p = { id: "pv_abc", state: "ready" };
    const asked = fakeFetch({
      "GET /v1/previews?project=prj_1&state=sleeping": [200, [p]],
      "POST /v1/previews": [201, p],
      "GET /v1/previews/pv_abc": [200, p],
      "DELETE /v1/previews/pv_abc": [202, { ...p, desired: "deleted", state: "deleting" }],
      "POST /v1/previews/pv_abc/refresh": [200, p],
      "POST /v1/previews/pv_abc/sleep": [200, { ...p, state: "sleeping" }],
      "POST /v1/previews/pv_abc/wake": [200, p],
      "POST /v1/previews/pv_abc/reset": [200, p],
      "GET /v1/previews/pv_abc/logs?service=web&lines=50": [200, { service: "web", lines: ["listening"] }],
      "GET /v1/previews/pv_abc/activity": [200, [{ at: 1, message: "ready" }]],
      "GET /v1/previews/projects/prj_1/settings": [200, { enabled: false }],
      "PUT /v1/previews/projects/prj_1/settings": [200, { enabled: true }],
      "GET /v1/previews/projects/prj_1/usage": [200, { previews: 1, sleeping_usd_month: 0.4, active_usd_hour: 0.02, month_to_date_usd: 1.5, bytes: 9 }],
    });
    expect(await previewsApi.list({ project: "prj_1", state: "sleeping" })).toEqual([p]);
    await previewsApi.create("prj_1", "acme/web", { branch: "feat-x" });
    await previewsApi.create("prj_1", "acme/web", { change: "chg_1" });
    await previewsApi.get("pv_abc");
    expect((await previewsApi.remove("pv_abc")).desired).toBe("deleted");
    await previewsApi.refresh("pv_abc");
    expect((await previewsApi.sleep("pv_abc")).state).toBe("sleeping");
    await previewsApi.wake("pv_abc");
    await previewsApi.reset("pv_abc");
    await previewsApi.reset("pv_abc", ["postgres:prj_db"]);
    expect((await previewsApi.logs("pv_abc", { service: "web", lines: 50 })).lines).toEqual(["listening"]);
    await previewsApi.activity("pv_abc");
    await previewsApi.settings("prj_1");
    await previewsApi.putSettings("prj_1", { enabled: true, github: { comment: false } });
    expect((await previewsApi.usage("prj_1")).month_to_date_usd).toBe(1.5);

    const bodies = asked.filter((a) => a.body !== undefined).map((a) => [a.method, a.path, a.body]);
    expect(bodies).toEqual([
      ["POST", "/v1/previews", { project: "prj_1", repo: "acme/web", branch: "feat-x" }],
      ["POST", "/v1/previews", { project: "prj_1", repo: "acme/web", change: "chg_1" }],
      ["POST", "/v1/previews/pv_abc/reset", {}],
      ["POST", "/v1/previews/pv_abc/reset", { resources: ["postgres:prj_db"] }],
      ["PUT", "/v1/previews/projects/prj_1/settings", { enabled: true, github: { comment: false } }],
    ]);
  });

  it("throws the server's error", async () => {
    fakeFetch({
      "PUT /v1/previews/projects/prj_1/settings": [400, { error: "sleep_after_s: 60 to 604800" }],
      "POST /v1/previews": [409, { error: "max_previews (10) is reached" }],
    });
    await expect(previewsApi.putSettings("prj_1", { sleep_after_s: 5 })).rejects.toThrow("sleep_after_s: 60 to 604800");
    await expect(previewsApi.create("prj_1", "acme/web", { branch: "x" })).rejects.toThrow("max_previews (10) is reached");
  });
});

describe("following logs", () => {
  it("reads server-sent events split anywhere across chunks", () => {
    const a = parseSse("", "event: service\ndata: web\n\ndata: listening on 8000\n\nda");
    expect(a.events).toEqual([
      { event: "service", data: "web" },
      { event: "message", data: "listening on 8000" },
    ]);
    const b = parseSse(a.rest, "ta: GET /\r\n\r\n: keep-alive\n\n");
    expect(b.events).toEqual([{ event: "message", data: "GET /" }]);
    expect(b.rest).toBe("");
    // A \r\n split between two chunks is one line end, not an event's end.
    const c = parseSse("", "event: service\r");
    expect(parseSse(c.rest, "\ndata: web\r\n\r\n").events).toEqual([{ event: "service", data: "web" }]);
  });
});

describe("unmaskedNote", () => {
  it("says when previews would get production data as it is, and the way out", () => {
    expect(unmaskedNote("postgres", "branch", 0)).toMatch(/as it is: mask/);
    expect(unmaskedNote("postgres", "branch", 2)).toBeNull();
    expect(unmaskedNote("redis", "branch", 0)).toMatch(/can't be masked.*seed/);
    expect(unmaskedNote("stream", "branch", 0)).toMatch(/Schema only/);
    for (const [kind, mode] of [["stream", "schema"], ["bucket", "skip"], ["queue", "branch"], ["repo", "branch"]] as const) expect(unmaskedNote(kind, mode, 0)).toBeNull();
  });
});

describe("a11yWords", () => {
  it("says the session's own kinds and axe-core's rules in words", () => {
    const words = a11yWords({
      issues: [
        { kind: "images", count: 2, first: null },
        { kind: "color-contrast", count: 1, first: "<p>", impact: "serious", help: "Elements must meet minimum color contrast ratio thresholds" },
        { kind: "region", count: 3, first: null },
      ],
      lang: false,
      title: true,
    });
    expect(words).toEqual(["2 images without alt text", "1 element failing “Elements must meet minimum color contrast ratio thresholds” (serious)", "3 elements failing region", "no page language"]);
  });
});
