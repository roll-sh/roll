import { NextRequest } from "next/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MISSING, classify, proxy } from "./proxy";
import { RESERVED_NAMES } from "./lib/reserved-names";

afterEach(() => vi.unstubAllGlobals());

describe("classify", () => {
  it("finds owner and repo pages, skipping the app's routes and git traffic", () => {
    expect(classify("/acme")).toEqual({ kind: "owner", owner: "acme" });
    expect(classify("/acme/web")).toEqual({ kind: "repo", owner: "acme", repo: "web", rest: [] });
    expect(classify("/acme/web/blob/main/src/a.ts")).toEqual({ kind: "repo", owner: "acme", repo: "web", rest: ["blob", "main", "src", "a.ts"] });
    expect(classify("/acme/my%20repo")).toMatchObject({ repo: "my repo" });
    for (const p of ["/", "/dashboard", "/dashboard/code/acme/web", "/_next/static/x.js", "/icon.svg", "/favicon.ico", MISSING, "/settings/x"]) {
      expect(classify(p), p).toBeNull();
    }
    for (const p of ["/acme/web.git/info/refs", "/acme/web/info/refs", "/acme/web/git-upload-pack", "/acme/web/git-receive-pack", "/acme/web.git"]) {
      expect(classify(p), p).toBeNull();
    }
    expect(classify("/%E0%A4%A")).toBeNull();
    for (const r of RESERVED_NAMES) expect(classify(`/${r}/x`), r).toBeNull();
  });
});

function answer(status: number, body?: unknown) {
  const calls: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      calls.push(url);
      return new Response(body === undefined ? null : JSON.stringify(body), { status });
    }),
  );
  return calls;
}

describe("proxy", () => {
  it("rewrites a missing repo or owner to the 404 page", async () => {
    const calls = answer(404, { error: "not found" });
    const res = await proxy(new NextRequest("https://roll.sh/acme/nope/tree/main"));
    expect(calls[0]).toMatch(/\/v1\/code\/repos\/acme\/nope$/);
    expect(res.headers.get("x-middleware-rewrite")).toBe(`https://roll.sh${MISSING}`);
    const owner = await proxy(new NextRequest("https://roll.sh/nobody"));
    expect(calls[1]).toMatch(/\/v1\/code\/owners\/nobody$/);
    expect(owner.headers.get("x-middleware-rewrite")).toBe(`https://roll.sh${MISSING}`);
  });

  it("lets existing pages through", async () => {
    answer(200, { full_name: "acme/web" });
    const res = await proxy(new NextRequest("https://roll.sh/acme/web"));
    expect(res.headers.get("x-middleware-rewrite")).toBeNull();
    expect(res.headers.get("x-middleware-next")).toBe("1");
  });

  it("redirects a renamed repo, keeping the rest of the path and the query", async () => {
    answer(301, { error: "moved", redirect: "acme/site" });
    const res = await proxy(new NextRequest("https://roll.sh/acme/web/blob/main/a.ts?plain=1"));
    expect(res.status).toBe(308);
    expect(res.headers.get("location")).toBe("https://roll.sh/acme/site/blob/main/a.ts?plain=1");
  });

  it("renders normally when the control plane is down", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("fetch failed");
      }),
    );
    const res = await proxy(new NextRequest("https://roll.sh/acme/web"));
    expect(res.headers.get("x-middleware-next")).toBe("1");
  });

  it("doesn't call the API for app routes", async () => {
    const calls = answer(404);
    await proxy(new NextRequest("https://roll.sh/dashboard/code"));
    expect(calls).toEqual([]);
  });
});
