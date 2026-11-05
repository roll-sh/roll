import { describe, expect, it } from "vitest";
import { collapseUnused, deriveGraph, keyOf, projectStats, rolling } from "./agents-graph";
import { arrange, bounds, centreAcross, dropIndex, fitView, mergeOrder, moveTo, nextCard, ROOTS, sideBySide, tidyLayout, withOrder, zoomAt, type LayoutOptions, type Point, type Rect, type Size } from "./agents-layout";
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

  it("leaves a thread's terminals to its drawer while its conversation is open beside the canvas", () => {
    const d = applyEvents(sample(), [ev("terminal", terminal({ id: "trm_t", thread: "thr_1", title: "pnpm dev" }), 20)]);
    expect(deriveGraph(d).nodes.get("t:thr_1")!.children).toContain("x:trm_t");
    const docked = deriveGraph(d, { docked: "thr_1" });
    expect(docked.nodes.has("x:trm_t")).toBe(false);
    // Only that thread's: the project's and the machine's stay.
    expect(docked.nodes.has("x:trm_1") && docked.nodes.has("x:trm_login")).toBe(true);
    expect(deriveGraph(d, { docked: "thr_3" }).nodes.has("x:trm_t")).toBe(true);
  });

  it("animates only what runs: not the machine and project above it, not what waits on you", () => {
    const g = deriveGraph(sample());
    expect(g.nodes.get("t:thr_1")!.busy).toBe(true);
    expect(g.nodes.get("p:prj_1")!.busy).toBe(false);
    expect(g.nodes.get("m:mch_1")!.busy).toBe(false);
    expect(g.nodes.get("r:req_1")!.busy).toBe(false);
    expect(g.edges.find((e) => e.to === "t:thr_1")!.busy).toBe(true);
    expect(g.edges.find((e) => e.to === "p:prj_1")!.busy).toBe(false);
    expect(g.edges.find((e) => e.to === "t:thr_3")!.busy).toBe(false);
    const waiting = applyEvents(sample(), [ev("thread", thread({ status: "waiting", active_run: "run_1", created_at: 10 }), 20)]);
    expect(deriveGraph(waiting).nodes.get("t:thr_1")!.busy).toBe(false);
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

  it("sums up a project like T3 does: its threads, the PRs they made, the agents in it", () => {
    const change = { repo: "acme/roll", number: 41, status: "ready" as const, url: "/acme/roll/changes/41", detail: null };
    const d = applyEvents(sample(), [
      ev("thread", thread({ id: "thr_3", title: "Docs", created_at: 30, provider: "codex", model: "gpt-5.5-codex", change, usage: { input_tokens: 1, output_tokens: 1, cached_tokens: 0, cost_usd: 0.5 } }), 20),
      ev("thread", thread({ id: "thr_2", title: "Fork", status: "waiting", created_at: 20, lineage: { parent: "thr_1", relation: "fork", root: "thr_1", from_run: "run_1" } }), 21),
    ]);
    const s = projectStats(d, "prj_1");
    expect(s).toMatchObject({ threads: 3, running: 1, waiting: 1, cost_usd: 0.5 });
    expect(s.changes.map((c) => c.number)).toEqual([41]);
    expect(s.agents.map((a) => [a.provider, a.threads])).toEqual([["claude", 2], ["codex", 1]]);
    // Archived threads don't count.
    expect(projectStats(d, "prj_1").threads).toBe(3);
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

  it("lays out a tidy tree to the right, parents centred on their children", () => {
    const p = tidyLayout(tree, size, { depthGap: 50, siblingGap: 10, rootGap: 30 });
    expect(p.get("a1")).toEqual({ x: 150, y: 0 });
    expect(p.get("a2")).toEqual({ x: 150, y: 50 });
    expect(p.get("a2x")).toEqual({ x: 500, y: 50 + 80 });
    // a's subtree is 40 + 10 + 200 = 250 tall; a sits in its middle.
    expect(p.get("a")).toEqual({ x: 0, y: 105 });
    expect(p.get("b")).toEqual({ x: 0, y: 280 });
    // No two siblings overlap.
    expect(p.get("a2")!.y).toBeGreaterThanOrEqual(p.get("a1")!.y + 40 + 10);
  });

  it("lines depths up in columns, so cousins of different widths never overlap", () => {
    const t = {
      roots: ["m"],
      nodes: new Map([
        ["m", { children: ["narrow", "wide"] }],
        ["narrow", { children: ["n1"] }],
        ["wide", { children: ["w1"] }],
        ["n1", { children: [] }],
        ["w1", { children: [] }],
      ]),
    };
    const sz = (k: string) => (k === "wide" ? { w: 400, h: 40 } : { w: 100, h: 40 });
    const p = tidyLayout(t, sz, { depthGap: 50, siblingGap: 10 });
    expect(p.get("n1")!.x).toBe(p.get("w1")!.x);
    expect(p.get("w1")!.x).toBe(p.get("wide")!.x + 400 + 50);
    const keys = [...p.keys()];
    for (const a of keys)
      for (const b of keys) {
        if (a === b) continue;
        const [pa, pb, sa, sb] = [p.get(a)!, p.get(b)!, sz(a), sz(b)];
        const apart = pa.x + sa.w <= pb.x || pb.x + sb.w <= pa.x || pa.y + sa.h <= pb.y || pb.y + sb.h <= pa.y;
        expect(apart, `${a} and ${b} overlap`).toBe(true);
      }
  });

  it("grows down too: depths are rows, siblings side by side, parents centred over their children", () => {
    const p = tidyLayout(tree, size, { direction: "vertical", depthGap: 50, siblingGap: 10, rootGap: 30 });
    expect(p.get("a1")).toEqual({ x: 0, y: 90 });
    expect(p.get("a2")).toEqual({ x: 110, y: 90 });
    // Row 1 is as tall as its tallest card (a2, 200).
    expect(p.get("a2x")).toEqual({ x: 210, y: 90 + 200 + 50 });
    // a's subtree is 100 + 10 + 300 = 410 wide; a sits over its middle.
    expect(p.get("a")).toEqual({ x: 155, y: 0 });
    expect(p.get("b")).toEqual({ x: 440, y: 0 });
  });

  it("grows down as a board: machines spread their projects across, a project lists its work down its column", () => {
    const t = {
      roots: ["m"],
      nodes: new Map([
        ["m", { children: ["p1", "p2"] }],
        ["p1", { children: ["t1", "t2"] }],
        ["p2", { children: [] }],
        ["t1", { children: ["r1"] }],
        ["t2", { children: [] }],
        ["r1", { children: [] }],
      ]),
    };
    const sz = (k: string) => (k === "r1" ? { w: 160, h: 60 } : k === "p2" ? { w: 100, h: 70 } : { w: 100, h: 40 });
    const spreads = (k: string) => k === "m";
    const p = tidyLayout(t, sz, { direction: "vertical", spreads, depthGap: 50, siblingGap: 20, indent: 30, listGap: 10 });
    // p1's column: r1 is indented twice, 30 + 30 + 160 = 220 wide; p2 100; the row 220 + 20 + 100 = 340, m over its middle.
    expect(p.get("m")).toEqual({ x: 120, y: 0 });
    // The projects' row is as tall as its tallest card (p2, 70); both start on it.
    expect(p.get("p1")).toEqual({ x: 0, y: 90 });
    expect(p.get("p2")).toEqual({ x: 240, y: 90 });
    // p1's work lists below the row, indented, each thread's satellites before the next thread.
    expect(p.get("t1")).toEqual({ x: 30, y: 90 + 70 + 10 });
    expect(p.get("r1")).toEqual({ x: 60, y: 170 + 40 + 10 });
    expect(p.get("t2")).toEqual({ x: 30, y: 220 + 60 + 10 });
    // Siblings that spread sit side by side; listed ones one under another.
    expect(sideBySide("vertical", ROOTS, spreads)).toBe(true);
    expect(sideBySide("vertical", "m", spreads)).toBe(true);
    expect(sideBySide("vertical", "p1", spreads)).toBe(false);
    expect(sideBySide("horizontal", "m", spreads)).toBe(false);
  });

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

  it("measures siblings along the axis they're laid out on, so dragging works either way", () => {
    const at: Point = { x: 100, y: 40 };
    const s: Size = { w: 200, h: 80 };
    expect(centreAcross(at, s, false)).toBe(80);
    expect(centreAcross(at, s, true)).toBe(200);
    // Dragged sideways past b in a top-down tree, a drops after it.
    const p = tidyLayout({ roots: ["m"], nodes: new Map([["m", { children: ["a", "b", "c"] }], ["a", { children: [] }], ["b", { children: [] }], ["c", { children: [] }]]) }, () => s, { direction: "vertical" });
    const centre = (k: string) => centreAcross(p.get(k)!, s, true);
    expect(moveTo(["a", "b", "c"], "a", dropIndex(["a", "b", "c"], "a", centre("b") + 1, centre))).toEqual(["b", "a", "c"]);
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
    const centres = new Map([["a", 20], ["b", 100], ["c", 180]]);
    const at = (y: number) => moveTo(["a", "b", "c"], "a", dropIndex(["a", "b", "c"], "a", y, (k) => centres.get(k)!));
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

describe("sandboxes and phones on the canvas", () => {
  it("a sandbox pulses while it boots, and not once it runs", () => {
    const sb = (status: "creating" | "running") => machine({ id: "mch_s", name: "sandbox", online: status === "running", info: null, sandbox: { backend: "docker", size: "standard", status, hourly_cents: 3 } });
    let d = applyEvents(emptyData(), [{ type: "machine", cursor: 1, machine: sb("creating") }]);
    expect(deriveGraph(d).nodes.get(keyOf.machine("mch_s"))?.busy).toBe(true);
    d = applyEvents(d, [{ type: "machine", cursor: 2, machine: sb("running") }]);
    expect(deriveGraph(d).nodes.get(keyOf.machine("mch_s"))?.busy).toBe(false);
  });

  it("a phone's fit stays readable and starts at the machines (top left)", () => {
    const wide = { x: 0, y: 0, w: 3000, h: 400 };
    const desk = fitView(wide, 1200, 800, 64, 1);
    expect(desk.k).toBeCloseTo((1200 - 128) / 3000);
    expect(desk.x + (wide.w / 2) * desk.k).toBeCloseTo(600);
    const phone = fitView(wide, 390, 700, 24, 1, 0.55);
    expect(phone.k).toBe(0.55);
    expect(phone.x).toBe(24);
    // What fits vertically stays centred.
    expect(phone.y + (wide.h / 2) * phone.k).toBeCloseTo(350);
  });

  it("a phone's fit starts on the first machine, wherever the direction puts it", () => {
    // Top-down: the machine sits centred over a wide row of work.
    const r = { x: 0, y: 0, w: 3000, h: 1600 };
    const machine = { x: 1364, y: 0, w: 272, h: 120 };
    const v = fitView(r, 390, 700, 24, 1, 0.55, machine);
    expect(v.k).toBe(0.55);
    // The machine is centred on screen, and the top shows.
    expect((machine.x + machine.w / 2) * v.k + v.x).toBeCloseTo(195);
    expect(v.y).toBe(24);
    // Already on screen from the start: nothing moves.
    expect(fitView(r, 390, 700, 24, 1, 0.55, { x: 0, y: 0, w: 272, h: 120 }).x).toBe(24);
    // Never past the far end.
    const end = fitView(r, 390, 700, 24, 1, 0.55, { x: 2900, y: 0, w: 100, h: 100 });
    expect(end.x).toBeCloseTo(390 - 24 - 3000 * 0.55);
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
