import { describe, expect, it, vi } from "vitest";
import { createLookupApi, lookupPath, memoryLookup, problemText } from "./lookup-api";
import { DEMO_LOOKUPS } from "./lookup-demo";

describe("lookup-api", () => {
  it("asks the relay with the query, bounded, and decodes rows and problems", async () => {
    const call = vi.fn(async () => ({ items: [{ id: "team-1", label: "Engineering", key: "ENG", color: "#5e6ad2" }], next: "c1" }));
    const api = createLookupApi(call);
    const page = await api.list("acme", "linear", "states", { q: "prog", parent: "team-1", ids: [], cursor: "c0" });
    expect(call).toHaveBeenCalledWith("GET", "/v1/code/agents/acme/lookup/linear/states?q=prog&parent=team-1&cursor=c0");
    expect(page).toEqual({ items: [{ id: "team-1", label: "Engineering", key: "ENG", color: "#5e6ad2" }], next: "c1" });
    expect(lookupPath("acme", "linear", "teams", { ids: Array.from({ length: 60 }, (_, i) => `t${i}`) })).toContain(`ids=${Array.from({ length: 50 }, (_, i) => `t${i}`).join("%2C")}`);
    expect(lookupPath("acme", "linear", "teams", { q: "x".repeat(200) })).toContain(`q=${"x".repeat(100)}`);

    call.mockResolvedValueOnce({ items: [], problem: { code: "rate_limited", retry_after: 30 } } as never);
    const limited = await api.list("acme", "linear", "teams");
    expect(limited.problem).toEqual({ code: "rate_limited", retry_after: 30 });
    expect(problemText(limited.problem!, "linear")).toEqual({ text: "Linear asked roll to wait 30 s", reconnect: false });
    expect(problemText({ code: "not_connected", message: "" }, "slack")).toEqual({ text: "Slack isn't connected", reconnect: true });

    call.mockResolvedValueOnce({ id: "lab-1", label: "Needs QA" } as never);
    expect(await api.create("acme", "linear", "labels", { name: "Needs QA", parent: "team-1" })).toEqual({ id: "lab-1", label: "Needs QA" });
    expect(call).toHaveBeenLastCalledWith("POST", "/v1/code/agents/acme/lookup/linear/labels", { name: "Needs QA", parent: "team-1" });
  });

  it("answers from memory with the server's rules: search, ids by id or key, pages, 404s, problems", async () => {
    const api = memoryLookup(DEMO_LOOKUPS);
    expect((await api.list("acme", "linear", "teams", { q: "op" })).items.map((i) => i.label)).toEqual(["Operations"]);
    expect((await api.list("acme", "linear", "teams", { ids: ["eng", "team_web"] })).items.map((i) => i.id)).toEqual(["team_eng", "team_web"]);
    const first = await api.list("acme", "linear", "users", { limit: 2 });
    expect(first.next).toBe("2");
    expect((await api.list("acme", "linear", "users", { limit: 2, cursor: first.next })).items[0].label).toBe("Alan Turing");
    expect((await api.list("acme", "linear", "states", { parent: "team_ops" })).items.every((s) => s.group === "Operations")).toBe(true);
    await expect(api.list("acme", "linear", "milestones")).rejects.toThrow(/milestones/);
    const made = await api.create("acme", "linear", "labels", { name: "Needs QA", parent: "team_eng" });
    expect((await api.list("acme", "linear", "labels", { parent: "team_eng", q: "qa" })).items).toEqual([made]);
    const down = memoryLookup(DEMO_LOOKUPS, { linear: { code: "not_connected", message: "no" } });
    expect(await down.list("acme", "linear", "teams")).toEqual({ items: [], problem: { code: "not_connected", message: "no" } });
  });
});
