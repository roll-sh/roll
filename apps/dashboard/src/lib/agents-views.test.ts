import { describe, expect, it } from "vitest";
import { MAX_CANVAS } from "./agents-api";
import { deriveGraph, type Graph } from "./agents-graph";
import { arrange, tidyLayout, type Size } from "./agents-layout";
import { applyEvents, emptyData, follow } from "./agents-store";
import {
  addView,
  ALL,
  applyView,
  countView,
  DEFAULT_VIEWS,
  deleteView,
  directionOf,
  encodeDoc,
  MAX_PINS,
  MAX_VIEWS,
  moveView,
  parseDoc,
  pin,
  PIN_MIN,
  renameView,
  resizePin,
  setViewDisplay,
  setViewFilter,
  setViewOrder,
  unpin,
  viewById,
  viewsOf,
  withDirection,
  type CanvasDoc,
} from "./agents-views";
import { approval, ev, machine, project, run, T0, terminal, thread } from "@/test/agents";

const NOW = T0 + 60_000;

function sample() {
  let d = follow(emptyData(), ["thr_1"]);
  d = applyEvents(d, [
    { type: "machine", cursor: 1, machine: machine() },
    { type: "machine", cursor: 2, machine: machine({ id: "mch_2", name: "build-box" }) },
    ev("project", project(), 3),
    ev("project", project({ id: "prj_2", name: "web" }), 4, "mch_2"),
    ev("thread", thread({ status: "running", active_run: "run_1", created_at: 10 }), 5),
    ev("thread", thread({ id: "thr_2", title: "Upgrade Next", status: "waiting", pending_requests: 1, created_at: 20 }), 6),
    ev("thread", thread({ id: "thr_3", title: "Docs pass", provider: "codex", created_at: 30, updated_at: T0 - 3 * 86_400_000 }), 7),
    ev("thread", thread({ id: "thr_4", project: "prj_2", title: "Broken build", status: "error", created_at: 40 }), 8, "mch_2"),
    ev("run", run(), 9),
    ev("request", approval({ id: "req_2", thread: "thr_2" }), 10),
    ev("terminal", terminal(), 11),
  ]);
  return d;
}

const keys = (g: Graph) => [...g.nodes.keys()].sort();

describe("the canvas document", () => {
  it("parses a document from before views: All keeps its order and the built-in views show", () => {
    const doc = parseDoc({ order: { "m:mch_1": ["p:prj_2", "p:prj_1"] } });
    expect(doc.order).toEqual({ "m:mch_1": ["p:prj_2", "p:prj_1"] });
    expect(doc.pins).toEqual([]);
    expect(doc.views.map((v) => v.name)).toEqual(["Waiting on me", "Running"]);
    // An owner who deleted every view keeps none.
    expect(parseDoc({ order: {}, views: [] }).views).toEqual([]);
    // Junk is dropped, never fatal.
    for (const junk of [null, 3, "x", [1], { order: 4, views: "no", pins: {} }]) expect(parseDoc(junk).order).toEqual({});
  });

  it("round-trips views, filters, orders and pins, and drops what's malformed", () => {
    let doc: CanvasDoc = parseDoc({});
    doc = addView(doc, "  Build   box  ", { machines: ["mch_2"], status: ["error"], text: "build" }, "v_build");
    doc = setViewOrder(doc, "v_build", { "m:mch_2": ["p:prj_2"] });
    doc = pin(doc, "v_build", "conversation", "thr_4");
    doc = pin(doc, ALL, "terminal", "trm_1");
    const back = parseDoc(JSON.parse(JSON.stringify(encodeDoc(doc))));
    expect(back).toEqual(doc);
    expect(viewById(back, "v_build")).toMatchObject({ name: "Build box", filter: { machines: ["mch_2"], status: ["error"], text: "build" }, pins: [{ kind: "conversation", id: "thr_4", w: 520, h: 600 }] });

    const messy = parseDoc({
      views: [
        { id: "v_a", name: "A", filter: { status: ["running", "bogus"], machines: [1, "mch_1", "mch_1"] }, pins: [{ kind: "diff", id: "thr_1", w: 1, h: 99999 }, { kind: "nope", id: "x" }] },
        { id: "v_a", name: "duplicate" },
        { id: "bad id", name: "B" },
        { id: "v_c", name: "   " },
      ],
    });
    expect(messy.views).toEqual([{ id: "v_a", name: "A", filter: { status: ["running"], machines: ["mch_1"] }, order: {}, pins: [{ kind: "diff", id: "thr_1", w: PIN_MIN.diff.w, h: 1200 }], display: {} }]);
  });

  it("keeps each view's direction, a phone's apart from a wide screen's; old documents show the defaults", () => {
    // Before display options: across on a wide screen, down on a phone.
    const old = parseDoc({ order: {}, views: [{ id: "v_a", name: "A", filter: {} }] });
    expect(old.display).toEqual({});
    expect(directionOf(viewById(old, "v_a").display, false)).toBe("horizontal");
    expect(directionOf(viewById(old, "v_a").display, true)).toBe("vertical");
    // Turned top-down on the laptop: the phone keeps its own.
    let doc = setViewDisplay(old, ALL, withDirection(old.display, false, "vertical"));
    expect(directionOf(viewById(doc, ALL).display, false)).toBe("vertical");
    doc = setViewDisplay(doc, "v_a", withDirection(viewById(doc, "v_a").display, true, "horizontal"));
    expect(viewById(doc, "v_a").display).toEqual({ phoneDirection: "horizontal" });
    expect(directionOf(viewById(doc, "v_a").display, false)).toBe("horizontal");
    // Synced: it survives the trip through roll; nothing is written while it's the default.
    const saved = JSON.parse(JSON.stringify(encodeDoc(doc)));
    expect(saved.display).toEqual({ direction: "vertical" });
    expect(saved.views[0].display).toEqual({ phoneDirection: "horizontal" });
    expect(encodeDoc(old)).not.toHaveProperty("display");
    expect(parseDoc(saved)).toEqual(doc);
    // Unknown values show the default.
    expect(parseDoc({ display: { direction: "diagonal", phoneDirection: 3 } }).display).toEqual({});
    // A view saved from what's on screen keeps how it looked.
    expect(viewById(addView(doc, "Down", {}, "v_d", doc.display), "v_d").display).toEqual({ direction: "vertical" });
  });

  it("creates, renames, reorders and deletes views; All stays first and can't be removed", () => {
    let doc = parseDoc({});
    doc = addView(doc, "Web", { projects: ["prj_2"] }, "v_web");
    expect(viewsOf(doc).map((v) => v.id)).toEqual([ALL, "v_waiting", "v_running", "v_web"]);
    doc = renameView(doc, "v_web", "Frontend");
    expect(renameView(doc, "v_web", "  ")).toBe(doc);
    doc = moveView(doc, "v_web", 0);
    expect(viewsOf(doc).map((v) => v.name)).toEqual(["All", "Frontend", "Waiting on me", "Running"]);
    doc = setViewFilter(doc, "v_web", { projects: ["prj_2"], providers: ["codex"] });
    expect(viewById(doc, "v_web").filter).toEqual({ projects: ["prj_2"], providers: ["codex"] });
    doc = deleteView(doc, "v_running");
    expect(doc.views.map((v) => v.id)).toEqual(["v_web", "v_waiting"]);
    expect(deleteView(doc, ALL).views).toHaveLength(2);
    // A missing view falls back to All.
    expect(viewById(doc, "v_gone").id).toBe(ALL);
    // Bounded.
    for (let i = 0; i < 40; i++) doc = addView(doc, `V${i}`, {});
    expect(doc.views).toHaveLength(MAX_VIEWS);
  });

  it("pins once per kind, unpins, resizes within bounds, and keeps at most MAX_PINS per view", () => {
    let doc = parseDoc({});
    doc = pin(doc, "v_waiting", "conversation", "thr_2");
    doc = pin(doc, "v_waiting", "conversation", "thr_2");
    doc = pin(doc, "v_waiting", "diff", "thr_2");
    expect(viewById(doc, "v_waiting").pins.map((p) => p.kind)).toEqual(["conversation", "diff"]);
    expect(doc.pins).toEqual([]);
    doc = resizePin(doc, "v_waiting", "conversation", "thr_2", 10, 5000);
    expect(viewById(doc, "v_waiting").pins[0]).toMatchObject({ w: PIN_MIN.conversation.w, h: 1200 });
    doc = unpin(doc, "v_waiting", "diff", "thr_2");
    expect(viewById(doc, "v_waiting").pins).toHaveLength(1);
    for (let i = 0; i < 20; i++) doc = pin(doc, ALL, "conversation", `thr_${i}`);
    expect(doc.pins).toHaveLength(MAX_PINS);
    expect(doc.pins.at(-1)!.id).toBe("thr_19");
    // Pins whose thread is gone make room first.
    doc = pin(doc, ALL, "diff", "thr_x", (p) => p.id !== "thr_10");
    expect(doc.pins.map((p) => p.id)).not.toContain("thr_10");
  });

  it("stays under the relay's limit however much is arranged, dropping the oldest arrangements first", () => {
    let doc = parseDoc({ views: [] });
    const ids = (n: number, p: string) => Array.from({ length: n }, (_, i) => `t:thr_${p}_${String(i).padStart(20, "0")}`);
    const order: Record<string, string[]> = {};
    for (let i = 0; i < 400; i++) order[`p:prj_${i}`] = ids(30, String(i));
    doc = { ...doc, order };
    for (let v = 0; v < MAX_VIEWS; v++) {
      doc = addView(doc, "x".repeat(60), { machines: ids(16, "m"), projects: ids(16, "p"), providers: ids(16, "q"), status: ["running", "waiting", "error", "recent"], text: "y".repeat(500) }, `v_${v}`);
      for (let i = 0; i < 12; i++) doc = pin(doc, `v_${v}`, "conversation", `thr_${"z".repeat(40)}_${i}`);
      doc = setViewOrder(doc, `v_${v}`, Object.fromEntries(Array.from({ length: 150 }, (_, i) => [`p:v${v}_${i}`, ids(10, `${v}_${i}`)])));
    }
    const json = JSON.stringify(encodeDoc(doc));
    expect(new TextEncoder().encode(json).length).toBeLessThanOrEqual(MAX_CANVAS);
    const back = parseDoc(JSON.parse(json));
    // Every view and pin survives; only arrangements went, the oldest first.
    expect(back.views).toHaveLength(MAX_VIEWS);
    expect(back.views.every((v) => v.pins.length === 12)).toBe(true);
    if (Object.keys(back.order).length) expect(back.order["p:prj_399"]).toBeDefined();
    // A small document goes out whole.
    const small = addView(parseDoc({ order: { a: ["b"] } }), "Mine", {}, "v_m");
    expect(encodeDoc(small)).toEqual({ order: { a: ["b"] }, views: [...DEFAULT_VIEWS.map((v) => ({ id: v.id, name: v.name, filter: v.filter })), { id: "v_m", name: "Mine", filter: {} }] });
  });

});

describe("what a view shows", () => {
  const g = () => deriveGraph(sample());
  const view = (filter: Parameters<typeof applyView>[2]["filter"], pins: Parameters<typeof applyView>[2]["pins"] = []) => applyView(g(), sample(), { filter, pins }, NOW);

  it("All shows the whole canvas", () => {
    const whole = g();
    expect(applyView(whole, sample(), { filter: {}, pins: [] }, NOW)).toBe(whole);
  });

  it("a status filter keeps the matching threads with what they hang off and their satellites", () => {
    expect(keys(view({ status: ["waiting"] }))).toEqual(["m:mch_1", "p:prj_1", "r:req_2", "t:thr_2"].sort());
    expect(keys(view({ status: ["running"] }))).toEqual(["m:mch_1", "p:prj_1", "t:thr_1"].sort());
    expect(keys(view({ status: ["error", "waiting"] }))).toEqual(["m:mch_1", "m:mch_2", "p:prj_1", "p:prj_2", "r:req_2", "t:thr_2", "t:thr_4"].sort());
    // Recent: touched in the last day (thr_3 was three days ago).
    expect(view({ status: ["recent"] }).nodes.has("t:thr_3")).toBe(false);
    expect(view({ status: ["recent"] }).nodes.has("t:thr_1")).toBe(true);
    // Edges only between what's kept; roots only what's kept.
    const v = view({ status: ["waiting"] });
    expect(v.roots).toEqual(["m:mch_1"]);
    expect(v.edges.map((e) => `${e.from}>${e.to}`).sort()).toEqual(["m:mch_1>p:prj_1", "p:prj_1>t:thr_2", "t:thr_2>r:req_2"]);
    expect(v.nodes.get("p:prj_1")!.children).toEqual(["t:thr_2"]);
  });

  it("machines and projects alone keep them whole; providers and text narrow threads; fields combine", () => {
    expect(keys(view({ machines: ["mch_2"] }))).toEqual(["m:mch_2", "p:prj_2", "t:thr_4"]);
    expect(keys(view({ projects: ["prj_1"] }))).toEqual(["m:mch_1", "p:prj_1", "r:req_2", "t:thr_1", "t:thr_2", "t:thr_3", "x:trm_1"].sort());
    expect(keys(view({ providers: ["codex"] }))).toEqual(["m:mch_1", "p:prj_1", "t:thr_3"]);
    // Text matches the title, the project and the machine, case-insensitively.
    expect(view({ text: "NEXT" }).nodes.has("t:thr_2")).toBe(true);
    expect(keys(view({ text: "web" }))).toEqual(["m:mch_2", "p:prj_2", "t:thr_4"]);
    expect(keys(view({ text: "build-box", status: ["running"] }))).toEqual([]);
    expect(countView(sample(), { status: ["waiting"] }, NOW)).toBe(1);
    expect(countView(sample(), { machines: ["mch_1"] }, NOW)).toBe(3);
  });

  it("pinned cards always show, beside their thread, and never overlap anything", () => {
    const pins = [
      { kind: "conversation" as const, id: "thr_3", w: 520, h: 600 },
      { kind: "diff" as const, id: "thr_3", w: 680, h: 520 },
      { kind: "terminal" as const, id: "trm_1", w: 700, h: 420 },
      { kind: "conversation" as const, id: "thr_gone", w: 520, h: 600 },
    ];
    // The view is about errors, but the pinned docs thread and terminal still show.
    const v = view({ status: ["error"] }, pins);
    expect(v.nodes.get("t:thr_3")!.children).toEqual(["pc:thr_3", "pd:thr_3"]);
    expect(v.nodes.get("pc:thr_3")).toMatchObject({ kind: "pin", id: "thr_3", parent: "t:thr_3" });
    // A pinned terminal takes its card's place.
    expect(v.nodes.has("x:trm_1")).toBe(false);
    expect(v.nodes.get("p:prj_1")!.children).toContain("px:trm_1");
    expect(v.nodes.has("pc:thr_gone")).toBe(false);
    expect(v.edges).toContainEqual({ from: "t:thr_3", to: "pc:thr_3", busy: false, dashed: false });

    // Laid out with their own sizes: no two cards overlap.
    const size = (k: string): Size => {
      const p = pins.find((x) => k === `${x.kind === "conversation" ? "pc" : x.kind === "diff" ? "pd" : "px"}:${x.id}`);
      return p ? { w: p.w, h: p.h } : k.startsWith("t:") ? { w: 296, h: 96 } : { w: 260, h: 80 };
    };
    for (const graph of [v, applyView(g(), sample(), { filter: {}, pins }, NOW)]) {
      const pos = tidyLayout(arrange(graph, {}), size);
      const rects = [...pos].map(([k, p]) => ({ k, ...p, ...size(k) }));
      expect(rects.length).toBe(graph.nodes.size);
      for (const a of rects)
        for (const b of rects) {
          if (a.k >= b.k) continue;
          const apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
          expect(apart, `${a.k} overlaps ${b.k}`).toBe(true);
        }
    }
  });

  it("a thread with its conversation pinned answers its requests inside the card", () => {
    const d = sample();
    const graph = deriveGraph(d, { expanded: new Set(["thr_2"]) });
    const v = applyView(graph, d, { filter: {}, pins: [{ kind: "conversation", id: "thr_2", w: 520, h: 600 }] }, NOW);
    expect(v.nodes.has("r:req_2")).toBe(false);
    expect(v.nodes.get("t:thr_2")!.children).toEqual(["pc:thr_2"]);
  });
});
