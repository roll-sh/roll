#!/usr/bin/env node
// Replays the canvas planner (lib/agents-layout.ts) in node on the inputs `canvas-check.mjs` wrote
// (DIR/input/<case>.json: the tree, the cards' sizes, the screen and the chrome), and runs the same
// layout checks on Fit and Fit again — in milliseconds, so the planner can be changed and
// re-checked without a browser. The browser run stays the truth (drawing, engines, zoom steps).
//
// Usage: node scripts/canvas-replay.mjs DIR [--only REGEX] [--verbose]   (env CANVAS_LAYOUT=<a copy of agents-layout.ts>)
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { checkFrame } from "./canvas-check-lib.mjs";

// env CANVAS_LAYOUT: another copy of agents-layout.ts to try.
const { layoutCanvas, planWraps, fitTo, roomFor, bounds } = await import(process.env.CANVAS_LAYOUT ? pathToFileURL(process.env.CANVAS_LAYOUT).href : new URL("../src/lib/agents-layout.ts", import.meta.url).href);

const args = process.argv.slice(2);
const dir = args[0];
const only = args.includes("--only") ? new RegExp(args[args.indexOf("--only") + 1]) : null;
const verbose = args.includes("--verbose");

function frameFor(c, overview) {
  const { input } = c;
  const nodes = new Map(Object.entries(input.tree.nodes).map(([k, children]) => [k, { children }]));
  const tree = { roots: input.tree.roots, nodes };
  const size = (k) => input.sizes[k] ?? { w: 0, h: 0 };
  const sections = new Set(input.sections);
  const shape = { direction: input.direction, level: (k) => input.levels[k] ?? null, section: (k) => sections.has(k), spacing: input.spacing };
  const important = new Set(input.important);
  const matters = (k) => important.has(k);
  const t0 = performance.now();
  // The planner's screen as the canvas measures it: above the prompt bar, in steps of 32 px.
  const step = (v) => Math.max(32, Math.round(v / 32) * 32);
  const wraps = planWraps(tree, size, shape, { ...roomFor(step(c.viewport.w), step(c.viewport.h - c.covered)), overview }, matters);
  const ms = performance.now() - t0;
  const placed = layoutCanvas(tree, size, shape, wraps);
  // A phone's waiting request sends the zoom bar to the top: Fit frames below it (agents-canvas `fit`).
  const top = c.viewport.w < 640 && c.chrome.waiting ? 48 : 0;
  const room = { ...roomFor(c.viewport.w, c.viewport.h - c.covered - top), overview };
  const v = fitTo(placed.positions, size, room, matters);
  const fit = { ...v, y: v.y + top };
  const parent = new Map();
  for (const [k, n] of nodes) for (const ch of n.children) parent.set(ch, k);
  const snapNodes = [...nodes.keys()].map((k) => {
    const p = placed.positions.get(k);
    const s = size(k);
    return { key: k, parent: parent.get(k) ?? null, important: important.has(k), x: p?.x ?? null, y: p?.y ?? null, w: s.w, h: s.h };
  });
  const minimap = c.chrome.canvas.w >= 1200 && !fit.all ? (c.chrome.minimap ?? { x: c.chrome.canvas.w - 192, y: c.chrome.canvas.h - 52 - 120, w: 180, h: 120 }) : null;
  return {
    wraps,
    ms,
    frame: {
      ...c.chrome,
      minimap,
      dom: {},
      title: 0,
      snap: { view: fit, goal: fit, room, overview, plan: { k: fit.k, all: fit.all }, nodes: snapNodes, input },
    },
    bounds: bounds(placed.positions, size),
  };
}

let pass = 0;
let total = 0;
for (const f of readdirSync(join(dir, "input")).sort()) {
  const name = f.replace(/\.json$/, "");
  if (only && !only.test(name)) continue;
  const c = JSON.parse(readFileSync(join(dir, "input", f), "utf8"));
  if (!c.chrome) continue;
  total++;
  const one = frameFor(c, false);
  const r1 = checkFrame(one.frame, "fit", { fit: true });
  const fails = [...r1.fails];
  if (!one.frame.snap.plan.all) {
    const two = frameFor(c, true);
    fails.push(...checkFrame(two.frame, "fit2", { fit: true }).fails);
  }
  if (!fails.length) pass++;
  const m = r1.metrics;
  console.log(`${fails.length ? "FAIL" : "ok  "} ${name.padEnd(26)} zoom ${String(Math.round(m.k * 100)).padStart(3)}%${m.all ? " all " : " part"} cover ${m.cover ?? "-"} dead ${m.dead ?? "-"} ${JSON.stringify(one.wraps)} ${one.ms.toFixed(0)}ms`);
  if (verbose || fails.length) for (const x of fails) console.log(`       ${x}`);
}
console.log(`\n${pass}/${total} cases pass (replayed)`);
