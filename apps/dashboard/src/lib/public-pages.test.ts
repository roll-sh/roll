import { isNotFound, isRedirect } from "@tanstack/react-router";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchShare } from "./agents-server";
import { fetchRepo } from "./code-server";
import { canBeOwner, ownerOr404, repoOr404 } from "./public-pages";
import { RESERVED_NAMES } from "./reserved-names";

afterEach(() => vi.unstubAllGlobals());

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

/** What a guard threw. */
function thrown(f: () => unknown): unknown {
  try {
    f();
  } catch (e) {
    return e;
  }
  throw new Error("didn't throw");
}

describe("canBeOwner", () => {
  it("is false for the app's own names, `_x` and `x.y`", () => {
    expect(canBeOwner("acme")).toBe(true);
    for (const n of ["dashboard", "Dashboard", "_next", "_roll", "icon.svg", "favicon.ico", "install.sh", "settings"]) expect(canBeOwner(n), n).toBe(false);
    for (const r of RESERVED_NAMES) expect(canBeOwner(r), r).toBe(false);
  });
});

describe("repo pages", () => {
  it("answer a missing repo with a 404", async () => {
    const calls = answer(404, { error: "not found" });
    const found = await fetchRepo("acme", "nope");
    expect(calls[0]).toMatch(/\/v1\/code\/repos\/acme\/nope$/);
    expect(isNotFound(thrown(() => repoOr404(found, "/acme/nope/tree/main", "")))).toBe(true);
  });

  it("let an existing repo through", async () => {
    answer(200, { full_name: "acme/web" });
    const found = await fetchRepo("acme", "web");
    expect(repoOr404(found, "/acme/web", "")).toEqual({ full_name: "acme/web" });
  });

  it("redirect a renamed repo permanently, keeping the rest of the path and the query", async () => {
    answer(301, { error: "moved", redirect: "acme/site" });
    const found = await fetchRepo("acme", "web");
    expect(found).toEqual({ redirect: "acme/site" });
    const r = thrown(() => repoOr404(found, "/acme/web/blob/main/my%20dir/a.ts", "?plain=1"));
    expect(isRedirect(r)).toBe(true);
    const { statusCode, href } = (r as { options: { statusCode: number; href: string } }).options;
    expect(statusCode).toBe(308);
    expect(href).toBe("/acme/site/blob/main/my%20dir/a.ts?plain=1");
  });

  it("fail (the error page) when the control plane is down", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("fetch failed");
      }),
    );
    await expect(fetchRepo("acme", "web")).rejects.toThrow("fetch failed");
  });
});

describe("owner pages", () => {
  it("never ask the API about the app's own names", () => {
    const calls = answer(404);
    expect(isNotFound(thrown(() => ownerOr404("dashboard")))).toBe(true);
    expect(isNotFound(thrown(() => ownerOr404("_next")))).toBe(true);
    expect(() => ownerOr404("acme")).not.toThrow();
    expect(calls).toEqual([]);
  });
});

describe("share pages", () => {
  it("find an unknown or revoked share missing", async () => {
    const calls = answer(404, { error: "share shr_x not found" });
    expect(await fetchShare("shr_x")).toBeNull();
    expect(calls[0]).toMatch(/\/v1\/code\/agents\/shares\/shr_x$/);
  });
});
