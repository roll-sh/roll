import { describe, expect, it } from "vitest";
import { canStart, canStop, createSandboxesApi, formatMonthly, sandboxListD, type Sandbox } from "./sandboxes-api";
import { decode } from "./decode";

const sandbox = (over: Partial<Sandbox> = {}): Sandbox => ({
  id: "sbx_1",
  name: "box",
  machine: "mch_1",
  backend: "agent37",
  size: { name: "standard", cpu: 2, memory_gb: 4, disk_gb: 8, monthly_cents: 2000 },
  status: "running",
  desired: "running",
  error: null,
  created_at: 1,
  updated_at: 2,
  ...over,
});

/** A fetch that answers from a table and records what it was asked. */
function fakeFetch(answers: Record<string, [number, unknown]>) {
  const asked: { method: string; url: string; body: unknown; auth: string | null }[] = [];
  const f = (async (url: string, init: RequestInit) => {
    const method = init.method ?? "GET";
    asked.push({ method, url, body: init.body ? JSON.parse(String(init.body)) : undefined, auth: new Headers(init.headers).get("authorization") });
    const [status, body] = answers[`${method} ${url}`] ?? [404, { error: "not found" }];
    return new Response(status === 204 ? null : JSON.stringify(body), { status });
  }) as unknown as typeof fetch;
  return { f, asked };
}

describe("sandboxes api", () => {
  it("lists, creates, starts, stops, deletes and reads usage on the documented routes", async () => {
    const s = sandbox();
    const machine = { id: "mch_1", name: "box", online: false, sandbox: { backend: "agent37", size: "standard", status: "creating", monthly_cents: 2000 }, created_at: 1 };
    const { f, asked } = fakeFetch({
      "GET https://api/v1/code/agents/acme/sandboxes": [200, { sandboxes: [s], sizes: [s.size], max: 5, backend: "agent37" }],
      "POST https://api/v1/code/agents/acme/sandboxes": [201, { sandbox: { ...s, status: "creating" }, machine }],
      "POST https://api/v1/code/agents/acme/sandboxes/sbx_1/stop": [200, { ...s, status: "stopped", desired: "stopped" }],
      "POST https://api/v1/code/agents/acme/sandboxes/sbx_1/start": [200, s],
      "DELETE https://api/v1/code/agents/acme/sandboxes/sbx_1": [204, null],
      "DELETE https://api/v1/code/agents/acme/sandboxes/sbx_2": [202, { ...s, id: "sbx_2", status: "deleting", desired: "deleted" }],
      "GET https://api/v1/code/agents/acme/sandboxes/usage?month=2026-11": [
        200,
        { owner: "acme", month: "2026-11", sandboxes: [{ sandbox: "sbx_1", name: "box", size: "standard", monthly_cents: 2000, running_ms: 3_600_000, running_hours: 1, price_usd: 0.03 }], totals: { running_ms: 3_600_000, running_hours: 1, price_usd: 0.03 } },
      ],
    });
    const api = createSandboxesApi("https://api", "rlc_tok", undefined, f);
    const list = await api.list("acme");
    expect([list.backend, list.max, list.sandboxes[0].size.monthly_cents]).toEqual(["agent37", 5, 2000]);
    const made = await api.create("acme", { name: "box", env: { ANTHROPIC_API_KEY: "sk-ant" } });
    expect([made.sandbox.status, made.machine.sandbox?.status]).toEqual(["creating", "creating"]);
    expect(asked[1].body).toEqual({ name: "box", env: { ANTHROPIC_API_KEY: "sk-ant" } });
    expect(asked[1].auth).toBe("Bearer rlc_tok");
    expect((await api.stop("acme", "sbx_1")).desired).toBe("stopped");
    expect((await api.start("acme", "sbx_1")).status).toBe("running");
    expect(await api.remove("acme", "sbx_1")).toBeNull();
    expect((await api.remove("acme", "sbx_2"))?.status).toBe("deleting");
    expect((await api.usage("acme", "2026-11")).totals.running_hours).toBe(1);
  });

  it("throws the server's error and reports a 401", async () => {
    let unauthorized = 0;
    const { f } = fakeFetch({
      "POST https://api/v1/code/agents/acme/sandboxes": [422, { error: "acme has 5 sandboxes; delete one first" }],
      "GET https://api/v1/code/agents/acme/sandboxes": [401, { error: "unauthorized" }],
    });
    const api = createSandboxesApi("https://api", undefined, () => unauthorized++, f);
    await expect(api.create("acme")).rejects.toMatchObject({ status: 422, message: "acme has 5 sandboxes; delete one first" });
    await expect(api.list("acme")).rejects.toMatchObject({ status: 401 });
    expect(unauthorized).toBe(1);
  });

  it("decodes strictly what the server sends", () => {
    const wire = { sandboxes: [sandbox({ error: "agent37: no_capacity" })], sizes: [sandbox().size], max: 5, backend: null };
    expect(decode(sandboxListD, wire, { strict: true }).sandboxes[0].error).toBe("agent37: no_capacity");
    expect(() => decode(sandboxListD, { ...wire, sandboxes: [{ ...sandbox(), status: "asleep" }] })).toThrow();
  });

  it("formats prices and says what can be done", () => {
    expect(formatMonthly(2000)).toBe("$20/mo");
    expect(formatMonthly(476)).toBe("$4.76/mo");
    expect(canStop(sandbox())).toBe(true);
    expect(canStart(sandbox())).toBe(false);
    expect(canStart(sandbox({ status: "stopped", desired: "stopped" }))).toBe(true);
    expect(canStart(sandbox({ status: "stopped", desired: "deleted" }))).toBe(false);
  });
});
