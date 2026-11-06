import { describe, expect, it } from "vitest";
import type { NodeBody, ToolKind } from "./agents-api";
import type { NodeTree } from "./agents-store";
import { foldLabel, foldTurn, splitAtSteers, stepsBefore, timelineItems } from "./agents-worklog";
import { node } from "@/test/agents";

let n = 0;
const leaf = (body: NodeBody, status: "running" | "completed" | "failed" = "completed", children: NodeTree[] = []): NodeTree => ({
  node: node(body, { id: `nod_${++n}`, status }),
  children,
});
const tool = (kind: ToolKind, title = "x", over: Partial<Extract<NodeBody, { type: "tool" }>> = {}) =>
  leaf({ type: "tool", name: kind, kind, title, input: null, output: null, exit_code: null, files: [], ...over });
const say = (text: string) => leaf({ type: "message", text });
const think = (text: string) => leaf({ type: "reasoning", text });

describe("the work log (T3's grouping)", () => {
  it("joins consecutive steps into one group; messages and requests stand alone; empty thoughts go", () => {
    const items = timelineItems([think("Look first"), tool("read"), tool("command"), say("Found it"), think(""), tool("edit"), leaf({ type: "request", request: "req_1" }), tool("command")]);
    expect(items.map((i) => (i.kind === "work" ? `work:${i.entries.length}` : i.t.node.body.type))).toEqual(["work:3", "message", "work:1", "request", "work:1"]);
  });

  it("folds an ended run's work under its final message", () => {
    const items = timelineItems([tool("read"), say("Looking"), tool("edit"), say("Done: the claim is atomic now.")]);
    const turn = foldTurn(items, { status: "completed" });
    expect(turn.fold).toHaveLength(3);
    expect(turn.rest).toHaveLength(1);
    // Still running, or no final message: nothing folds.
    expect(foldTurn(items, { status: "running" }).fold).toBeNull();
    expect(foldTurn(timelineItems([tool("read"), tool("edit")]), { status: "completed" }).fold).toBeNull();
    expect(foldLabel({ status: "completed", started_at: 0, completed_at: 133_000 })).toBe("Worked for 2m 13s");
    expect(foldLabel({ status: "interrupted", started_at: 0, completed_at: 12_000 })).toBe("You stopped after 12s");
  });

  it("finds the last steps before a request, a subagent's included", () => {
    const req = leaf({ type: "request", request: "req_1" });
    const inner = tool("command", "rg claim");
    const tree = [tool("read", "a"), leaf({ type: "subagent", agent: "Explore", prompt: "p", child: null, result: null }, "completed", [inner]), tool("edit", "b"), say("Asking"), req, tool("command", "after")];
    const { steps, total } = stepsBefore(tree, req.node.id, 2);
    expect(total).toBe(3);
    expect(steps.map((s) => (s.body.type === "tool" ? s.body.title : ""))).toEqual(["rg claim", "b"]);
  });
});

describe("steers in time (T3: a steer is a message where it came)", () => {
  const at = (id: string, started_at: number): NodeTree => ({ node: node({ type: "message", text: id }, { id, status: "completed", started_at }), children: [] });
  const items = timelineItems([at("a", 100), at("b", 200), at("c", 300)]);
  const ids = (xs: { items: ReturnType<typeof timelineItems> }[]) => xs.map((p) => p.items.map((i) => (i.kind === "node" ? i.t.node.id : i.id)));

  it("cuts the work where each steer came, after what started before it", () => {
    const r = splitAtSteers(items, { steers: ["first", "second", "late"], steered_at: [150, 150, 999] });
    expect(r.before).toEqual([]);
    expect(ids(r.parts)).toEqual([["a"], ["b", "c"], []]);
    expect(r.parts.map((p) => p.steers)).toEqual([["first", "second"], ["late"], []]);
  });

  it("puts one from before any work, or with no time (an older daemon), under the prompt", () => {
    expect(splitAtSteers(items, { steers: ["early"], steered_at: [50] }).before).toEqual(["early"]);
    const old = splitAtSteers(items, { steers: ["old"] });
    expect(old.before).toEqual(["old"]);
    expect(ids(old.parts)).toEqual([["a", "b", "c"]]);
  });
});
