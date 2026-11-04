import { describe, expect, it, vi } from "vitest";
import { createAgentsApi } from "./agents-api";
import { httpServices, parseWorkRef } from "./agents-services";
import type { Repo } from "./code-api";
import { createSandboxesApi } from "./sandboxes-api";

/** A fetch that records requests and answers from `reply`. */
function recorder(reply: (method: string, url: string) => { status: number; body?: unknown }) {
  const calls: { method: string; url: string; body: unknown; auth: string | null }[] = [];
  const doFetch = vi.fn(async (url: string, init: RequestInit) => {
    const method = init.method ?? "GET";
    calls.push({ method, url, body: init.body ? JSON.parse(String(init.body)) : undefined, auth: new Headers(init.headers).get("authorization") });
    const r = reply(method, url);
    return new Response(r.body === undefined ? null : JSON.stringify(r.body), { status: r.status });
  }) as unknown as typeof fetch;
  return { calls, doFetch };
}

describe("agents services over HTTP", () => {
  it("starts a repo as a project, takes and lets go of work with the caller's token", async () => {
    const { calls, doFetch } = recorder((method, url) => {
      if (url.endsWith("/projects")) return { status: 200, body: { type: "created", id: "prj_9" } };
      if (method === "POST" && url.endsWith("/work")) return url.includes("thr_none") ? { status: 204 } : { status: 200, body: { id: "wi_1", repo: "acme/site", number: 4, title: "Dark mode" } };
      return { status: 204 };
    });
    const api = createAgentsApi("http://roll", "rlc_t", undefined, doFetch);
    const s = httpServices(api, createSandboxesApi("http://roll", "rlc_t", undefined, doFetch), async () => []);

    expect(await s.createProject("acme", "mch_1", { id: "cmd_1", repo: { name: "site", visibility: "private" } })).toEqual({ type: "created", id: "prj_9" });
    expect(calls[0]).toMatchObject({ method: "POST", url: "http://roll/v1/code/agents/acme/machines/mch_1/projects", body: { id: "cmd_1", repo: { name: "site", visibility: "private" } }, auth: "Bearer rlc_t" });

    expect(await s.takeWork("acme", "thr_1", { repo: "acme/site" })).toEqual({ id: "wi_1", repo: "acme/site", number: 4, title: "Dark mode" });
    expect(calls[1]).toMatchObject({ method: "POST", url: "http://roll/v1/code/agents/acme/threads/thr_1/work", body: { repo: "acme/site" } });
    // 204: nothing open to claim.
    expect(await s.takeWork("acme", "thr_none", { repo: "acme/site" })).toBeNull();

    await s.releaseWork("acme", "thr_1");
    expect(calls[3]).toMatchObject({ method: "DELETE", url: "http://roll/v1/code/agents/acme/threads/thr_1/work" });
  });

  it("a blocked or failed create answers the daemon's reply, not a throw", async () => {
    const { doFetch } = recorder(() => ({ status: 503, body: { type: "error", code: "machine_offline", message: "the machine isn't connected", error: "the machine isn't connected" } }));
    const s = httpServices(createAgentsApi("http://roll", undefined, undefined, doFetch));
    expect(await s.createProject("acme", "mch_1", { id: "cmd_1", repo: { name: "site" } })).toMatchObject({ type: "error", code: "machine_offline" });
  });

  it("offers only the owner's own repos to clone, most recently updated first", async () => {
    const repo = (owner: string, name: string, updated_at: number) => ({ owner, name, full_name: `${owner}/${name}`, description: null, visibility: "public", updated_at }) as Repo;
    const s = httpServices(undefined, undefined, async () => [repo("acme", "old", 1), repo("other", "theirs", 9), repo("acme", "new", 5)]);
    expect((await s.repos("acme")).map((r) => r.full_name)).toEqual(["acme/new", "acme/old"]);
  });

  it("reads what the work field accepts", () => {
    expect(parseWorkRef(" wi_abc123 ")).toEqual({ work: "wi_abc123" });
    expect(parseWorkRef("acme/site#12")).toEqual({ work: "acme/site#12" });
    expect(parseWorkRef("acme/site")).toEqual({ repo: "acme/site" });
    expect(parseWorkRef("#12")).toBeNull();
    expect(parseWorkRef("")).toBeNull();
  });
});
