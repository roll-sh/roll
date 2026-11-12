import { afterEach, describe, expect, it, vi } from "vitest";

/** rolld as far as the workspace needs it: Redis, buckets and the workspace API. */
function fakeRolld() {
  const placed: { kind: string; ref: string; x: number; y: number }[] = [];
  const redis: { id: string; name: string }[] = [];
  const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
  const fetch = vi.fn(async (input: string, init?: RequestInit) => {
    const { pathname: path } = new URL(input);
    const method = init?.method ?? "GET";
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    if (path === "/v1/info") return json({ version: "t", node: "n", products: ["redis", "storage"] });
    if (path === "/v1/workspace/projects") return json([{ id: "prj_a", name: "shop", created_at: 1, resources: [] }]);
    const put = path.match(/^\/v1\/workspace\/projects\/prj_a\/resources\/(\w+)\/(.+)$/);
    if (put && method === "PUT") {
      placed.push({ kind: put[1], ref: decodeURIComponent(put[2]), ...body });
      return json({ kind: put[1], ref: decodeURIComponent(put[2]), ...body, added_at: 2 });
    }
    if (path === "/v1/redis/databases" && method === "POST") {
      const db = { id: `rdb_${redis.length + 1}`, name: body.name };
      redis.push(db);
      return json(db, 201);
    }
    if (path === "/v1/redis/databases") return json(redis.map(redisDb));
    if (path === "/v1/storage/buckets" && method === "POST") return json({ error: "a bucket named shop-files exists" }, 409);
    if (path === "/v1/storage/buckets") return json([]);
    if (path.endsWith("/usage")) return json({ cost: { total_usd: 0 } });
    return json({ error: "not here" }, 404);
  });
  return { fetch, placed };
}

const redisDb = (d: { id: string; name: string }) => ({
  ...d,
  host: "cache.roll.internal",
  port: 6379,
  redis_url: "redis://default:secret@cache.roll.internal:6379",
  rest_url: "",
  rest_token: "",
  region: "us-east-1",
  parent_id: null,
  parent_at: null,
  created_at: 1,
  status: { state: "idle" },
  replicas: [],
  max_memory_mb: 256,
  eviction_policy: "noeviction",
  durability: "async",
  idle_timeout_s: 300,
  history_retention_s: 0,
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
});

describe("a project's Deploy", () => {
  it("makes each staged resource in its product and places it where its card was; a failure stays staged with the reason", async () => {
    const rolld = fakeRolld();
    vi.stubGlobal("fetch", rolld.fetch);
    const { loadWorkspace, workspace, workspaceState } = await import("./workspace-store");
    await loadWorkspace();

    const cache = workspace.stage("prj_a", "redis", "shop-cache");
    const files = workspace.stage("prj_a", "bucket", "shop-files");
    expect([files.x, files.y]).not.toEqual([cache.x, cache.y]);
    await workspace.deploy("prj_a");

    expect(rolld.placed).toEqual([{ kind: "redis", ref: "rdb_1", x: cache.x, y: cache.y }]);
    const s = workspaceState();
    expect(s.projects[0].resources.map((r) => r.ref)).toEqual(["rdb_1"]);
    expect(s.inventory.get("redis:rdb_1")?.name).toBe("shop-cache");
    expect(s.staged.prj_a).toEqual([expect.objectContaining({ id: files.id, phase: "failed", error: "a bucket named shop-files exists" })]);
  });
});
