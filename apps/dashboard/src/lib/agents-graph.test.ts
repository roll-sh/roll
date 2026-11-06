import { describe, expect, it } from "vitest";
import { collapseUnused, deriveGraph, keyOf, rolling } from "./agents-graph";
import { arrange, bounds, dropIndex, fitView, mergeOrder, moveTo, nextCard, ROOTS, tidyLayout, withOrder, zoomAt, type LayoutOptions, type Point, type Rect, type Size } from "./agents-layout";
import { applyEvents, emptyData, follow } from "./agents-store";
import { approval, ev, machine, node, project, run, terminal, thread } from "@/test/agents";
import { EMPTY_INBOX, type Thread } from "./agents-api";

function sample() {
  let d = follow(emptyData(), ["thr_1"]);
  d = applyEvents(d, [
    { type: "machine", cursor: 1, machine: machine() },
    { type: "machine", cursor: 2, machine: machine({ id: "mch_2", name: "build-box", online: false }) },
    ev("project", project(), 3),
    ev("project", project({ id: "prj_2", name: "web" }), 4, "mch_2"),
    ev("thread", thread({ status: "running", active_run: "run_1", created_at: 10 }), 5),
    ev("thread", thread({ id: "thr_2", title: "Fork", created_at: 20, lineage: { parent: "thr_1", relation: "fork", root: "thr_1", from_run: "run_1" } }), 6),
    ev("thread", thread({ id: "thr_3", title: "Docs", created_at: 30 }), 7),
    ev("thread", thread({ id: "thr_old", title: "Archived", archived_at: 5 }), 8),
    ev("run", run(), 9),
    ev("node", node({ type: "subagent", agent: "Explore", prompt: "find callers", child: null, result: null }, { id: "nod_s" }), 10),
    ev("node", node({ type: "subagent", agent: "Delegated", prompt: "x", child: "thr_2", result: null }, { id: "nod_d", ordinal: 2 }), 11),
    ev("request", approval(), 12),
    ev("terminal", terminal(), 13),
    ev("terminal", terminal({ id: "trm_login", project: null, title: "codex login" }), 14),
  ]);
  return d;
}

describe("agents graph", () => {
  it("hangs projects off machines, threads off projects, forks off their parent", () => {
    const g = deriveGraph(sample());
    expect(g.roots).toEqual(["m:mch_1", "m:mch_2"]); // by name: ada-mbp, build-box
    const kids = (k: string) => g.nodes.get(k)!.children;
    expect(kids("m:mch_1")).toEqual(["p:prj_1", "x:trm_login"]);
    expect(kids("p:prj_1")).toEqual(["t:thr_3", "t:thr_1", "x:trm_1"]);
    // The fork is a thread under thr_1; the delegated subagent node is drawn as that thread, not twice.
    expect(kids("t:thr_1")).toEqual(["t:thr_2", "s:nod_s", "r:req_1"]);
    expect(g.nodes.has("t:thr_old")).toBe(false);
    expect(g.edges.find((e) => e.to === "t:thr_2")).toMatchObject({ dashed: true, busy: false });
  });

  it("leaves a thread's terminals to its drawer and its requests to its conversation while that is open beside the canvas", () => {
    const d = applyEvents(sample(), [ev("terminal", terminal({ id: "trm_t", thread: "thr_1", title: "pnpm dev" }), 20)]);
    expect(deriveGraph(d).nodes.get("t:thr_1")!.children).toContain("x:trm_t");
    const docked = deriveGraph(d, { docked: "thr_1" });
    expect(docked.nodes.has("x:trm_t")).toBe(false);
    // Only that thread's: the project's and the machine's stay.
    expect(docked.nodes.has("x:trm_1") && docked.nodes.has("x:trm_login")).toBe(true);
    expect(deriveGraph(d, { docked: "thr_3" }).nodes.has("x:trm_t")).toBe(true);
    // Its requests are answered in the conversation: no second copy beside the card (it ran off the canvas, under the panel).
    expect(docked.nodes.has("r:req_1")).toBe(false);
    expect(deriveGraph(d, { docked: "thr_3" }).nodes.has("r:req_1")).toBe(true);
  });

  it("a login on the machine that finished cleanly leaves the canvas; a failed one stays", () => {
    const done = (code: number) => applyEvents(sample(), [ev("terminal", terminal({ id: "trm_login", project: null, title: "codex login", status: "exited", exit_code: code }), 20)]);
    expect(deriveGraph(done(0)).nodes.has("x:trm_login")).toBe(false);
    expect(deriveGraph(done(1)).nodes.has("x:trm_login")).toBe(true);
    // A project's terminal that exited stays (its output is what you look at).
    const d = applyEvents(sample(), [ev("terminal", terminal({ status: "exited", exit_code: 0 }), 20)]);
    expect(deriveGraph(d).nodes.has("x:trm_1")).toBe(true);
  });

  it("lets finished subagents go: a done subagent node and a delegated thread that's done leave the canvas", () => {
    const d = applyEvents(sample(), [
      ev("node", node({ type: "subagent", agent: "Explore", prompt: "find callers", child: null, result: "3 callers" }, { id: "nod_s", status: "completed" }), 30),
      ev("thread", thread({ id: "thr_d", title: "Delegated", created_at: 40, lineage: { parent: "thr_1", relation: "subagent", root: "thr_1", from_run: "run_1" } }), 31),
      ev("thread", thread({ id: "thr_w", title: "Delegated, working", status: "running", created_at: 41, lineage: { parent: "thr_1", relation: "subagent", root: "thr_1", from_run: "run_1" } }), 32),
    ]);
    const g = deriveGraph(d);
    expect(g.nodes.has("s:nod_s")).toBe(false);
    expect(g.nodes.has("t:thr_d")).toBe(false);
    expect(g.nodes.has("t:thr_w")).toBe(true);
    // A fork stays: it's the user's own thread.
    expect(g.nodes.has("t:thr_2")).toBe(true);
  });

  it("settled and snoozed threads leave the canvas; their active forks hang off the project", () => {
    const inbox = (over: Partial<Thread["inbox"]>) => ({ ...EMPTY_INBOX, ...over });
    const d = applyEvents(sample(), [
      ev("thread", thread({ id: "thr_3", title: "Docs", created_at: 30, inbox: inbox({ settle: { state: "settled", at: 40, by: "inactivity" } }) }), 30),
      ev("thread", thread({ id: "thr_s", title: "Later", created_at: 31, inbox: inbox({ snooze: { until: Date.now() + 60_000, at: 31 } }) }), 31),
      ev("thread", thread({ id: "thr_k", title: "Kept", created_at: 32, inbox: inbox({ settle: { state: "active", at: 40 } }) }), 32),
      ev("thread", thread({ id: "thr_f", title: "Fork of docs", created_at: 33, lineage: { parent: "thr_3", relation: "fork", root: "thr_3", from_run: null } }), 33),
    ]);
    const g = deriveGraph(d);
    expect(g.nodes.has("t:thr_3")).toBe(false);
    expect(g.nodes.has("t:thr_s")).toBe(false);
    expect(g.nodes.has("t:thr_k")).toBe(true);
    expect(g.nodes.get("t:thr_f")!.parent).toBe("p:prj_1");
  });

  it("moves an open thread's requests into the session and shows archived threads on request", () => {
    const g = deriveGraph(sample(), { expanded: new Set(["thr_1"]), showArchived: true });
    expect(g.nodes.has(keyOf.request("req_1"))).toBe(false);
    expect(g.nodes.has("t:thr_old")).toBe(true);
  });

  it("lists what's rolling", () => {
    const r = rolling(sample());
    expect(r.busy.map((t) => t.id)).toEqual(["thr_1"]);
    expect(r.requests.map((q) => q.id)).toEqual(["req_1"]);
  });
});

describe("agents layout", () => {
  const tree = {
    roots: ["a", "b"],
    nodes: new Map([
      ["a", { children: ["a1", "a2"] }],
      ["a1", { children: [] }],
      ["a2", { children: ["a2x"] }],
      ["a2x", { children: [] }],
      ["b", { children: [] }],
    ]),
  };
  const size = (k: string) => (k === "a2" ? { w: 300, h: 200 } : { w: 100, h: 40 });

  it("never overlaps cards of any size, either way, whatever the order", () => {
    // A machine with projects, threads of all sizes, satellites and a pinned card.
    const nodes = new Map<string, { children: string[] }>();
    const sizes = new Map<string, Size>();
    const add = (k: string, s: Size, kids: string[] = []) => {
      nodes.set(k, { children: kids });
      sizes.set(k, s);
    };
    add("m1", { w: 272, h: 120 }, ["p1", "p2", "x1"]);
    add("m2", { w: 272, h: 180 }, ["p3"]);
    add("p1", { w: 256, h: 64 }, ["t1", "t2", "t3"]);
    add("p2", { w: 256, h: 110 }, []);
    add("p3", { w: 256, h: 64 }, ["t4"]);
    add("x1", { w: 620, h: 380 });
    add("t1", { w: 296, h: 96 }, ["pc1", "r1", "s1"]);
    add("t2", { w: 296, h: 140 }, ["t2f"]);
    add("t3", { w: 296, h: 96 });
    add("t4", { w: 296, h: 120 }, ["r2"]);
    add("t2f", { w: 296, h: 96 });
    add("pc1", { w: 520, h: 600 });
    add("r1", { w: 352, h: 150 });
    add("r2", { w: 352, h: 210 });
    add("s1", { w: 240, h: 72 });
    const sz = (k: string) => sizes.get(k)!;
    const orders = [{}, withOrder(withOrder({}, ROOTS, ["m2", "m1"]), "t1", ["s1", "r1", "pc1"])];
    const layouts: LayoutOptions[] = [{ direction: "horizontal" }, { direction: "vertical" }, { direction: "vertical", spreads: (k) => k.startsWith("m") }];
    for (const opts of layouts)
      for (const order of orders) {
        const direction = opts.spreads ? "board" : opts.direction;
        const p = tidyLayout(arrange({ roots: ["m1", "m2"], nodes }, order), sz, opts);
        expect(p.size).toBe(nodes.size);
        const keys = [...p.keys()];
        for (const a of keys)
          for (const b of keys) {
            if (a >= b) continue;
            const [pa, pb, sa, sb] = [p.get(a)!, p.get(b)!, sz(a), sz(b)];
            const apart = pa.x + sa.w <= pb.x || pb.x + sb.w <= pa.x || pa.y + sa.h <= pb.y || pb.y + sb.h <= pa.y;
            expect(apart, `${direction}: ${a} and ${b} overlap`).toBe(true);
          }
        // Children are past their parent in the direction the tree grows.
        for (const [k, n] of nodes)
          for (const c of n.children) {
            if (direction !== "horizontal") expect(p.get(c)!.y).toBeGreaterThan(p.get(k)!.y + sz(k).h);
            else expect(p.get(c)!.x).toBeGreaterThan(p.get(k)!.x + sz(k).w);
          }
      }
  });

  it("moves between cards with the arrows along the tree, either way it grows", () => {
    // m → p1 (t1 → r1, t2), p2 (t3)
    const t = {
      roots: ["m"],
      nodes: new Map([
        ["m", { children: ["p1", "p2"] }],
        ["p1", { children: ["t1", "t2"] }],
        ["p2", { children: ["t3"] }],
        ["t1", { children: ["r1"] }],
        ["t2", { children: [] }],
        ["t3", { children: [] }],
        ["r1", { children: [] }],
      ]),
    };
    const sz = (k: string) => (k.startsWith("r") ? { w: 352, h: 150 } : k.startsWith("t") ? { w: 296, h: 96 } : { w: 256, h: 64 });
    const rectsOf = (p: Map<string, Point>) => new Map<string, Rect>([...p].map(([k, q]) => [k, { ...q, ...sz(k) }]));
    // Left to right: ← parent, → child, ↑ / ↓ along the column (across parents).
    const h = rectsOf(tidyLayout(t, sz));
    expect(nextCard(h, "t1", "left")).toBe("p1");
    expect(nextCard(h, "t1", "right")).toBe("r1");
    expect(nextCard(h, "t1", "down")).toBe("t2");
    expect(nextCard(h, "t2", "down")).toBe("t3");
    expect(nextCard(h, "t3", "up")).toBe("t2");
    expect(nextCard(h, "p1", "left")).toBe("m");
    expect(nextCard(h, "m", "left")).toBeNull();
    // The board: ↑ / ↓ walk a project's column (a thread, its request, the next thread), ← / → go to the next column.
    const v = rectsOf(tidyLayout(t, sz, { direction: "vertical", spreads: (k) => k === "m" }));
    expect(nextCard(v, "m", "down")).toBe("p1");
    expect(nextCard(v, "p1", "down")).toBe("t1");
    expect(nextCard(v, "t1", "down")).toBe("r1");
    expect(nextCard(v, "r1", "down")).toBe("t2");
    expect(nextCard(v, "t2", "up")).toBe("r1");
    expect(nextCard(v, "p1", "right")).toBe("p2");
    expect(nextCard(v, "t1", "right")).toBe("t3");
    expect(nextCard(v, "p1", "up")).toBe("m");
    expect(nextCard(v, "gone", "up")).toBeNull();
  });

  it("keeps the user's order among siblings; new work keeps its default place", () => {
    // The user put c above a; b and the new n were never moved.
    expect(mergeOrder(["n", "a", "b", "c"], ["c", "b", "a"])).toEqual(["n", "c", "b", "a"]);
    expect(mergeOrder(["a", "b"], ["gone", "b", "a"])).toEqual(["b", "a"]);
    expect(mergeOrder(["a", "b"], undefined)).toEqual(["a", "b"]);
    const o = withOrder(withOrder({}, ROOTS, ["b", "a"]), "a", ["a2", "a1"]);
    const t = arrange(tree, o);
    expect(t.roots).toEqual(["b", "a"]);
    expect(t.nodes.get("a")!.children).toEqual(["a2", "a1"]);
    const p = tidyLayout(t, size, { depthGap: 50, siblingGap: 10, rootGap: 30 });
    expect(p.get("b")!.y).toBeLessThan(p.get("a")!.y);
    expect(p.get("a2")!.y).toBeLessThan(p.get("a1")!.y);
  });

  it("drops a dragged card after the siblings it passed", () => {
    const rects = new Map(["a", "b", "c"].map((k, i) => [k, { x: 0, y: i * 80, w: 100, h: 40 }]));
    const at = (y: number) => moveTo(["a", "b", "c"], "a", dropIndex(["a", "b", "c"], "a", { x: 50, y }, rects, false));
    expect(at(30)).toEqual(["a", "b", "c"]);
    expect(at(120)).toEqual(["b", "a", "c"]);
    expect(at(400)).toEqual(["b", "c", "a"]);
  });

  it("fits and zooms about a point", () => {
    const r = bounds(new Map([["a", { x: 0, y: 0 }], ["b", { x: 900, y: 500 }]]), () => ({ w: 100, h: 100 }))!;
    expect(r).toEqual({ x: 0, y: 0, w: 1000, h: 600 });
    const v = fitView(r, 1100, 700, 50);
    expect(v.k).toBe(1);
    expect(v).toEqual({ k: 1, x: 50, y: 50 });
    const z = zoomAt(v, 2, 550, 350);
    // The world point under the cursor stays put.
    expect((550 - z.x) / z.k).toBeCloseTo((550 - v.x) / v.k);
    expect((350 - z.y) / z.k).toBeCloseTo((350 - v.y) / v.k);
  });
});

describe("projects without a card", () => {
  // ada-mbp: roll (a thread), docs (nothing), cli (a terminal); build-box: web (nothing).
  function world() {
    return applyEvents(emptyData(), [
      { type: "machine", cursor: 1, machine: machine() },
      { type: "machine", cursor: 2, machine: machine({ id: "mch_2", name: "build-box" }) },
      ev("project", project(), 3),
      ev("project", project({ id: "prj_docs", name: "docs" }), 4),
      ev("project", project({ id: "prj_cli", name: "cli" }), 5),
      ev("project", project({ id: "prj_2", name: "web" }), 6, "mch_2"),
      ev("thread", thread(), 7),
      ev("terminal", terminal({ id: "trm_cli", project: "prj_cli", thread: null }), 8),
    ]);
  }

  it("folds a project with nothing on the canvas into its machine's list, in name order", () => {
    const { graph, unused } = collapseUnused(deriveGraph(world()), () => false);
    expect(graph.nodes.get("m:mch_1")!.children).toEqual(["p:prj_cli", "p:prj_1"]);
    expect(graph.nodes.has("p:prj_docs")).toBe(false);
    expect(graph.edges.some((e) => e.to === "p:prj_docs")).toBe(false);
    // A project with only a terminal on the canvas keeps its card.
    expect(graph.nodes.has("p:prj_cli")).toBe(true);
    expect(unused.get("mch_1")).toEqual(["prj_docs"]);
    // A machine with no project card still shows, listing them.
    expect(graph.roots).toEqual(["m:mch_1", "m:mch_2"]);
    expect(graph.nodes.get("m:mch_2")!.children).toEqual([]);
    expect(unused.get("mch_2")).toEqual(["prj_2"]);
  });

  it("keeps what it's told to (pinned, focused, filtered on), and a project's first thread brings its card back", () => {
    const { graph, unused } = collapseUnused(deriveGraph(world()), (p) => p === "prj_docs");
    expect(graph.nodes.has("p:prj_docs")).toBe(true);
    expect(unused.has("mch_1")).toBe(false);
    const more = applyEvents(world(), [ev("thread", thread({ id: "thr_d", project: "prj_2", title: "Docs" }), 9, "mch_2")]);
    expect(collapseUnused(deriveGraph(more), () => false).graph.nodes.get("p:prj_2")!.children).toEqual(["t:thr_d"]);
  });

  it("leaves the graph as it is when every project has something, and projects as anchors alone", () => {
    const g = deriveGraph(sample());
    // Its only empty project (web) kept: nothing to fold.
    expect(collapseUnused(g, (p) => p === "prj_2").graph).toBe(g);
    const byProject = { ...g, roots: ["p:prj_2"], nodes: new Map([["p:prj_2", { ...g.nodes.get("p:prj_2")!, parent: null, children: [] }]]) };
    expect(collapseUnused(byProject, () => false).graph).toBe(byProject);
  });
});
