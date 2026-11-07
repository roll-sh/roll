#!/usr/bin/env node
// The agents canvas's layout-quality checker. Loads every case of a matrix (owners × window sizes ×
// direction × side panel, plus the user's reported cases) in a real browser, reads the laid-out
// cards from the canvas (`__canvasCheck`, attached only when this script asks for it) and from the
// DOM, and checks hard constraints at Fit, Fit again and zoom steps:
//
//   a  no card under the prompt bar, zoom bar, minimap or past the canvas's edge (16 px clear)
//   b  no two cards overlap
//   c  after Fit, every running, waiting or failed card is wholly in view
//   d  no grid one column wide while the free area has room for two
//   e  everything fits: the content covers ≥ 60 % of the free area (unless it's at 100 %)
//   f  no dead area inside the content's box (an empty rectangle ≥ 15 % of it, ≥ 30 % each way)
//   g  a grid's rows align (1 px), grids side by side share their top line
//   h  everything fits: Fit again changes nothing
//   i  zooming never lays the canvas out again (each card keeps its world position)
//   l  Fit cuts the canvas though all of it reads at ≥ 65 %
//   k  what's drawn is where the layout put it (1 px), and text never grows as you zoom out
//   j  (--compare) Chromium and WebKit agree within 2 px
//
// Usage:
//   node scripts/canvas-check.mjs [--base URL] [--engine chromium|webkit] [--only REGEX]
//        [--out DIR] [--shots] [--jobs N] [--zooms 0.75,0.5,0.35]
//   node scripts/canvas-check.mjs --compare DIR      # j: chromium.json vs webkit.json in DIR
//   node scripts/canvas-check.mjs --report DIR       # the table again from DIR/<engine>.json
// env: CANVAS_CHECK_TOKENS="ada=/path/to/token,bea=…,northwind=…" (owner token files, never printed),
//      PLAYWRIGHT_CORE (a playwright-core directory), CHROMIUM (an executable).
// Each case's planner input is written to DIR/input/<case>.json for `canvas-replay.mjs`.
import { createRequire } from "node:module";
import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { checkFrame, moved } from "./canvas-check-lib.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  return i < 0 ? dflt : args[i + 1];
};
const flag = (name) => args.includes(`--${name}`);
const OUT = opt("out", join(process.env.HOME ?? ".", ".cache/canvas-check"));

// ── the matrix ──
const OWNERS = [
  { id: "ada", token: "ada", url: "" },
  { id: "bea", token: "bea", url: "" },
  { id: "busy", token: "ada", url: "?demo=busy&still" },
  { id: "d1000", token: "ada", url: "?demo=1000&still" },
  { id: "nw", token: "northwind", url: "" },
];
const SIZES = [
  [1440, 900],
  [2000, 1020],
  [1800, 1960],
  [390, 844],
];
/** Groups: one page each, its variants run in turn on it. */
function groups() {
  const out = [];
  for (const o of OWNERS)
    for (const [w, h] of SIZES) {
      const phone = w < 640;
      const variants = phone
        ? [{ dir: "vertical", panel: false }, { dir: "horizontal", panel: false }]
        : [{ dir: "horizontal", panel: false }, { dir: "horizontal", panel: true }, { dir: "vertical", panel: true }, { dir: "vertical", panel: false }];
      out.push({ name: `${o.id}-${w}x${h}`, owner: o, w, h, variants });
    }
  // The user's reported cases (ada): 1 the first view, 2 settings open on a tall window, 3 site
  // drilled into, 4 site drilled into with Part 2's conversation open.
  const ada = OWNERS[0];
  out.push({ name: "u1", owner: ada, w: 2000, h: 1020, variants: [{ dir: "horizontal", panel: false }] });
  out.push({ name: "u2", owner: ada, w: 1800, h: 1960, variants: [{ dir: "horizontal", panel: true }] });
  out.push({ name: "u3", owner: ada, w: 2000, h: 1447, variants: [{ dir: "horizontal", panel: false, drill: "site" }] });
  out.push({ name: "u4", owner: ada, w: 1600, h: 1066, variants: [{ dir: "horizontal", panel: false, drill: "site", open: "Part 2" }] });
  return out;
}

// ── reading a frame ──
/** Everything about the canvas as it is now: the layout (world → screen) and what the DOM draws. */
async function read(page) {
  return page.evaluate(() => {
    const c = document.querySelector('[aria-label="Agents canvas"]');
    if (!c || !c.__canvasCheck) return null;
    const box = c.getBoundingClientRect();
    const rel = (r) => ({ x: r.left - box.left, y: r.top - box.top, w: r.width, h: r.height });
    const visible = (el) => {
      if (!el || el.hidden) return null;
      const r = el.getBoundingClientRect();
      const s = getComputedStyle(el);
      return r.width && r.height && s.display !== "none" && s.visibility !== "hidden" && +s.opacity > 0.05 ? rel(r) : null;
    };
    const snap = c.__canvasCheck();
    // The prompt bar's dock spans the canvas: what's drawn is its children.
    const dock = document.querySelector("[data-prompt-bar]");
    const prompt = dock ? [...dock.children].map(visible).filter(Boolean) : [];
    const dom = {};
    for (const el of c.querySelectorAll("[data-card]:not([data-leaving])")) {
      const r = el.getBoundingClientRect();
      if (r.width) dom[el.dataset.key] = rel(r);
    }
    // The biggest title text, in world px (screen size ÷ zoom): it must never grow as the zoom drops.
    let title = 0;
    for (const el of c.querySelectorAll("[data-card] [data-title]")) {
      const s = parseFloat(getComputedStyle(el).fontSize);
      if (s > title) title = s;
    }
    return {
      canvas: { x: 0, y: 0, w: box.width, h: box.height },
      prompt,
      zoom: visible(c.querySelector("[data-zoom-bar]")),
      minimap: visible(c.querySelector("[data-minimap]")),
      waiting: !!c.closest("[data-waiting]"),
      dom,
      title,
      snap,
    };
  });
}

/** Wait until the view has arrived where it's going and nothing drawn moves. */
async function settle(page, ms = 10000) {
  const t0 = Date.now();
  let last = "";
  let same = 0;
  while (Date.now() - t0 < ms) {
    const sig = await page.evaluate(() => {
      const c = document.querySelector('[aria-label="Agents canvas"]');
      const s = c?.__canvasCheck?.();
      if (!s) return "none";
      const v = s.view, g = s.goal;
      const moving = !v || !g || Math.abs(v.k - g.k) > 1e-4 || Math.abs(v.x - g.x) > 0.5 || Math.abs(v.y - g.y) > 0.5;
      let h = 0;
      for (const el of c.querySelectorAll("[data-card]:not([data-leaving])")) {
        const r = el.getBoundingClientRect();
        h = (h * 31 + Math.round(r.left) * 7 + Math.round(r.top) * 13 + Math.round(r.width)) | 0;
      }
      return `${moving}|${h}|${s.nodes.length}`;
    });
    if (!sig.startsWith("true") && sig === last) {
      if (++same >= 2) return true;
    } else same = 0;
    last = sig;
    await page.waitForTimeout(160);
  }
  settleTimeouts++;
  return false;
}
/** How often a frame never came to rest (reported: a canvas that keeps moving is a failure too). */
let settleTimeouts = 0;

/** Wait for the data to stop changing (the demo's runs step for a while): node keys and what matters. */
async function steady(page, ms = 40000) {
  const t0 = Date.now();
  let last = "";
  let since = Date.now();
  while (Date.now() - t0 < ms) {
    const sig = await page.evaluate(() => {
      const s = document.querySelector('[aria-label="Agents canvas"]')?.__canvasCheck?.();
      return s ? s.nodes.map((n) => `${n.key}${n.important ? "!" : ""}${n.busy ? "*" : ""}`).sort().join(",") : "";
    });
    if (sig && sig === last) {
      if (Date.now() - since > 2500) return true;
    } else (since = Date.now()), (last = sig);
    await page.waitForTimeout(400);
  }
  return false;
}

// ── driving one group ──
async function press(page, key) {
  await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
  await page.keyboard.press(key);
}

async function pressFit(page) {
  await page.locator("[data-zoom-bar] button[aria-label^='Fit']").click();
}

async function zoomTo(page, k, phone) {
  // A phone has no wheel (mobile WebKit): the zoom keys, ×1.25 / ×0.8 about the middle.
  if (phone) {
    for (let i = 0; i < 20; i++) {
      const now = await page.evaluate(() => document.querySelector('[aria-label="Agents canvas"]').__canvasCheck().goal.k);
      if (Math.abs(Math.log(now / k)) < Math.log(1.25) / 2) break;
      await press(page, now > k ? "-" : "+");
      await page.waitForTimeout(60);
    }
    return;
  }
  for (let i = 0; i < 40; i++) {
    const s = await page.evaluate(() => {
      const c = document.querySelector('[aria-label="Agents canvas"]');
      const r = c.getBoundingClientRect();
      return { k: c.__canvasCheck().goal.k, x: r.left + r.width / 2, y: r.top + r.height / 3 };
    });
    if (Math.abs(s.k - k) < 0.003) break;
    const dy = Math.max(-49, Math.min(49, -Math.log(k / s.k) / 0.012));
    await page.mouse.move(s.x, s.y);
    await page.keyboard.down("Control");
    await page.mouse.wheel(0, dy);
    await page.keyboard.up("Control");
    await page.waitForTimeout(30);
  }
  await page.mouse.move(1, 1);
}

async function setDirection(page, want) {
  const now = await page.evaluate(() => document.querySelector('[aria-label="Agents canvas"]').dataset.direction);
  if (now !== want) await page.locator("[data-zoom-bar] button[aria-label^='Top-down']").click();
  await page.waitForFunction((w) => document.querySelector('[aria-label="Agents canvas"]')?.dataset.direction === w, want, { timeout: 15000 });
}

async function setPanel(page, open) {
  const isOpen = () => page.evaluate(() => !!document.querySelector('[aria-label="Settings"], [data-sheet] [data-settings]') || /Agents settings|General/.test(document.querySelector("aside, [data-right-dock]")?.textContent ?? ""));
  const canvasW = () => page.evaluate(() => document.querySelector('[aria-label="Agents canvas"]').getBoundingClientRect().width);
  const before = await canvasW();
  if (open) {
    await press(page, "Control+,");
    await page.waitForFunction((b) => document.querySelector('[aria-label="Agents canvas"]').getBoundingClientRect().width < b - 100, before, { timeout: 5000 }).catch(() => {});
  } else {
    await press(page, "Escape");
    await page.waitForFunction((b) => document.querySelector('[aria-label="Agents canvas"]').getBoundingClientRect().width > b + 100, before, { timeout: 5000 }).catch(async () => {
      await press(page, "Escape");
    });
  }
  void isOpen;
  await page.waitForTimeout(500);
}

async function runGroup(browser, g, ctxOpts) {
  const results = [];
  const phone = g.w < 640;
  const ctx = await browser.newContext({
    viewport: { width: g.w, height: g.h },
    colorScheme: "dark",
    deviceScaleFactor: 1,
    hasTouch: phone,
    isMobile: phone && ctxOpts.engine !== "firefox",
  });
  // Logged in as the owner; the canvas check hook on; no remembered view (the first view is Fit's).
  await ctx.addInitScript(
    ([k, t]) => {
      globalThis.__ROLL_CANVAS_CHECK = true;
      try {
        if (t) localStorage.setItem(k, t);
        for (const key of Object.keys(localStorage)) if (key.startsWith("roll:agents:view:")) localStorage.removeItem(key);
      } catch {}
    },
    [ctxOpts.key, ctxOpts.tokens[g.owner.token] ?? ""],
  );
  // The canvas document (direction, order) stays in this page: never written to the owner's.
  let doc = null;
  let rev = 1;
  await ctx.route(/\/agents\/canvas(\?|$)/, async (route) => {
    const req = route.request();
    if (req.method() === "PUT") {
      const body = JSON.parse(req.postData() || "{}");
      doc = body.canvas ?? doc;
      rev = (body.revision ?? rev) + 1;
      return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ canvas: doc, revision: rev, updated_at: 1_700_000_000_000 }) });
    }
    if (doc) return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ canvas: doc, revision: rev, updated_at: 1_700_000_000_000 }) });
    const res = await route.fetch();
    try {
      const j = await res.json();
      doc = j.canvas;
      rev = j.revision ?? 1;
    } catch {}
    return route.fulfill({ response: res });
  });
  const page = await ctx.newPage();
  await page.clock.setFixedTime(new Date("2026-11-07T12:00:00Z"));
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message.slice(0, 200)));
  try {
    await page.goto(`${ctxOpts.base}/dashboard/agents${g.owner.url}`, { waitUntil: "domcontentloaded", timeout: 120000 });
    // A view saved as a list or board: show it as the canvas (only in this page).
    await page.waitForSelector('[aria-label="Agents canvas"], [data-layout="canvas"]', { timeout: 120000 });
    const asList = page.locator('[data-layout="canvas"][aria-checked="false"]');
    if (await asList.count()) await asList.first().click();
    await page.waitForFunction(() => document.querySelectorAll('[aria-label="Agents canvas"] [data-card]').length >= 2 && document.querySelector('[aria-label="Agents canvas"]')?.__canvasCheck, null, { timeout: 120000 });
    await steady(page);
    await settle(page);
    let panelOpen = false;
    for (const v of g.variants) {
      const name = `${g.name}-${v.dir[0]}${v.panel ? "-panel" : ""}`;
      const frames = {};
      const r = { case: name, engine: ctxOpts.engine, fails: [], metrics: {}, notes: [] };
      const restless = settleTimeouts;
      try {
        if (v.panel !== panelOpen) {
          await setPanel(page, v.panel);
          panelOpen = v.panel;
        }
        await setDirection(page, v.dir);
        await settle(page);
        if (v.drill) {
          await pressFit(page);
          await settle(page);
          const proj = page.locator('[data-card][data-key^="p:"]').filter({ hasText: new RegExp(`^.{0,4}${v.drill}`) }).first();
          const b = await proj.boundingBox();
          await page.mouse.click(b.x + b.width / 2, b.y + b.height - 30);
          await page.waitForTimeout(1500);
          await steady(page, 8000);
          await settle(page);
        }
        if (v.open) {
          const chip = page.locator(`[data-card] button[aria-label*="Open ${v.open}"]`).first();
          if (await chip.count()) await chip.click();
          else {
            const card = page.locator('[data-card][data-key^="t:"]').filter({ hasText: new RegExp(`^.{0,40}${v.open}`) }).first();
            const c = await card.boundingBox();
            await page.mouse.click(c.x + c.width / 2, c.y + 30);
          }
          await page.waitForTimeout(1500);
          await page.mouse.move(1, 1);
          await settle(page);
        }
        const shot = async (tag) => {
          if (!ctxOpts.shots) return;
          const dir = join(OUT, "shots", ctxOpts.engine);
          mkdirSync(dir, { recursive: true });
          await page.screenshot({ path: join(dir, `${name}-${tag}.png`) });
        };
        frames.auto = await read(page);
        await shot("auto");
        // Moved by hand first, so Fit frames (pressed on a framed canvas it would go to the overview).
        await press(page, "ArrowLeft");
        await page.waitForTimeout(250);
        await pressFit(page);
        await settle(page);
        frames.fit1 = await read(page);
        await shot("fit");
        await pressFit(page);
        await settle(page);
        frames.fit2 = await read(page);
        await shot("fit2");
        if (frames.fit2.snap.overview) {
          await pressFit(page);
          await settle(page);
        }
        const base = await read(page);
        for (const z of ctxOpts.zooms) {
          await zoomTo(page, z, phone);
          await settle(page);
          frames[`z${Math.round(z * 100)}`] = await read(page);
          await shot(`z${Math.round(z * 100)}`);
        }
        // Back to Fit for the next variant.
        await pressFit(page);
        await settle(page);

        {
          const f = frames.fit1;
          writeFileSync(join(OUT, "input", `${name}.json`), JSON.stringify({ input: f.snap.input, viewport: f.snap.viewport, covered: f.snap.covered, chrome: { canvas: f.canvas, prompt: f.prompt, zoom: f.zoom, minimap: f.minimap, waiting: f.waiting } }));
        }
        const c1 = checkFrame(frames.auto, "auto", { fit: true });
        const c2 = checkFrame(frames.fit1, "fit", { fit: true });
        const c3 = checkFrame(frames.fit2, "fit2", { fit: true });
        r.fails.push(...c1.fails, ...c2.fails, ...c3.fails);
        r.metrics = { auto: c1.metrics, fit: c2.metrics, fit2: c3.metrics, wraps: frames.fit1.snap.wraps };
        // h: everything fits → Fit again changes nothing.
        if (frames.fit1.snap.plan?.all) {
          const a = frames.fit1.snap.goal;
          const b = frames.fit2.snap.goal;
          const m = moved(frames.fit1, frames.fit2);
          if (Math.abs(a.k - b.k) > 0.002 || Math.abs(a.x - b.x) > 1 || Math.abs(a.y - b.y) > 1 || m.length) {
            const was = new Map(frames.fit1.snap.nodes.map((n) => [n.key, n]));
            const why = frames.fit2.snap.nodes.filter((n) => { const o = was.get(n.key); return !o || o.w !== n.w || o.h !== n.h || o.important !== n.important; }).map((n) => { const o = was.get(n.key); return o ? `${n.key} ${o.w}×${o.h}→${n.w}×${n.h}` : `${n.key} new`; });
            r.fails.push(`h fit2: everything fit, yet Fit again moved the view (${(a.k * 100).toFixed(1)}→${(b.k * 100).toFixed(1)} %) or ${m.length} cards; changed: ${why.slice(0, 3).join(", ") || "none"}; wraps ${JSON.stringify(frames.fit1.snap.wraps)}→${JSON.stringify(frames.fit2.snap.wraps)} overview ${frames.fit1.snap.overview}→${frames.fit2.snap.overview}`);
          }
        }
        // i: zoom steps keep every card's world position; k: text never grows as the zoom drops.
        let lastTitle = base.title;
        for (const z of ctxOpts.zooms) {
          const f = frames[`z${Math.round(z * 100)}`];
          const m = moved(base, f);
          if (m.length) {
            // Why: a card's size or what matters changed (the cause), or nothing did (a re-plan on zoom).
            const was = new Map(base.snap.nodes.map((n) => [n.key, n]));
            const why = f.snap.nodes.filter((n) => { const o = was.get(n.key); return !o || o.w !== n.w || o.h !== n.h || o.important !== n.important; }).map((n) => { const o = was.get(n.key); return o ? `${n.key} ${o.w}×${o.h}${o.important ? "!" : ""}→${n.w}×${n.h}${n.important ? "!" : ""}` : `${n.key} new`; });
            const gone = base.snap.nodes.length - f.snap.nodes.length;
            r.fails.push(`i z${Math.round(z * 100)}: ${m.length} cards moved on zoom (${m.slice(0, 3).join(", ")}); changed: ${why.slice(0, 3).join(", ") || "none"}${gone ? `, ${gone} fewer cards` : ""}; wraps ${JSON.stringify(base.snap.wraps)}→${JSON.stringify(f.snap.wraps)} screen ${JSON.stringify(base.snap.input.screen)}→${JSON.stringify(f.snap.input.screen)} overview ${base.snap.overview}→${f.snap.overview}`);
          }
          const cz = checkFrame(f, `z${Math.round(z * 100)}`, { fit: false });
          r.fails.push(...cz.fails.filter((x) => /^[bgk]/.test(x)));
          if (f.title > lastTitle + 0.01) r.fails.push(`k z${Math.round(z * 100)}: title text grew to ${f.title}px (world) from ${lastTitle}px`);
          lastTitle = Math.max(lastTitle, f.title);
        }
        if (settleTimeouts > restless) r.fails.push(`k: the canvas didn't come to rest within 10 s (${settleTimeouts - restless} times)`);
        // For --compare: the drawn rects at each frame.
        r.frames = Object.fromEntries(Object.entries(frames).map(([t, f]) => [t, { goal: f.snap.goal, dom: f.dom }]));
      } catch (e) {
        r.fails.push(`! ${e.message.split("\n")[0]}`);
      }
      if (errors.length) r.notes.push(`page errors: ${errors.splice(0).join(" | ")}`);
      results.push(r);
      process.stderr.write(`${r.fails.length ? "✗" : "✓"} ${ctxOpts.engine} ${name} ${r.fails.length ? `(${r.fails.length})` : ""}\n`);
    }
  } catch (e) {
    results.push({ case: g.name, engine: ctxOpts.engine, fails: [`! ${e.message.split("\n")[0]}`], metrics: {}, notes: errors });
  } finally {
    await ctx.close();
  }
  return results;
}

// ── reporting ──
function report(results) {
  const codes = "abcdefghikl!".split("");
  const lines = [];
  let pass = 0;
  for (const r of results.sort((a, b) => a.case.localeCompare(b.case))) {
    const by = Object.fromEntries(codes.map((c) => [c, r.fails.filter((f) => f.startsWith(c)).length]));
    if (!r.fails.length) pass++;
    const m = r.metrics?.fit ?? {};
    lines.push(`${r.fails.length ? "FAIL" : "ok  "} ${r.case.padEnd(26)} ${codes.map((c) => (by[c] ? c : ".")).join("")}  zoom ${String(Math.round((m.k ?? 0) * 100)).padStart(3)}%${m.all ? " all " : " part"} cover ${m.cover ?? "-"} dead ${m.dead ?? "-"} wraps ${JSON.stringify(r.metrics?.wraps ?? {})}`);
    for (const f of r.fails) lines.push(`       ${f}`);
  }
  lines.push(`\n${pass}/${results.length} cases pass`);
  return lines.join("\n");
}

function compare(dir) {
  const a = JSON.parse(readFileSync(join(dir, "chromium.json"), "utf8"));
  const b = new Map(JSON.parse(readFileSync(join(dir, "webkit.json"), "utf8")).map((r) => [r.case, r]));
  const out = [];
  let bad = 0;
  for (const r of a) {
    const w = b.get(r.case);
    if (!w?.frames || !r.frames) continue;
    const diffs = [];
    for (const [t, f] of Object.entries(r.frames)) {
      if (!["auto", "fit1", "fit2"].includes(t)) continue;
      const g = w.frames[t];
      if (!g) continue;
      let n = 0, worst = 0, at = "";
      for (const [key, d] of Object.entries(f.dom)) {
        const e = g.dom[key];
        if (!e) continue;
        const dd = Math.max(Math.abs(d.x - e.x), Math.abs(d.y - e.y), Math.abs(d.w - e.w), Math.abs(d.h - e.h));
        if (dd > 2) (n++, dd > worst && ((worst = dd), (at = key)));
      }
      if (n) diffs.push(`${t}: ${n} cards differ (worst ${Math.round(worst)}px ${at})`);
    }
    if (diffs.length) bad++;
    out.push(`${diffs.length ? "FAIL" : "ok  "} j ${r.case.padEnd(26)} ${diffs.slice(0, 3).join("; ")}`);
  }
  out.push(`\n${out.length - bad}/${out.length} cases agree`);
  return out.join("\n");
}

// ── main ──
if (flag("compare")) {
  console.log(compare(opt("compare")));
  process.exit(0);
}
if (flag("report")) {
  const engine = opt("engine", "chromium");
  console.log(report(JSON.parse(readFileSync(join(opt("report"), `${engine}.json`), "utf8"))));
  process.exit(0);
}

const engine = opt("engine", "chromium");
const base = opt("base", "http://localhost:3474");
const only = opt("only") ? new RegExp(opt("only")) : null;
const zooms = opt("zooms", "1.25,0.75,0.5,0.35").split(",").map(Number);
const jobs = +opt("jobs", "3");
const pwDir = process.env.PLAYWRIGHT_CORE ?? [join(here, "../node_modules/playwright-core"), "/pw/node_modules/playwright-core", join(process.env.HOME ?? "", "opendesk-video-work/node_modules/playwright-core")].find((d) => existsSync(join(d, "package.json")));
const pw = createRequire(join(pwDir, "package.json"))("./index.js");
const key = /const KEY = "([^"]+)"/.exec(readFileSync(join(here, "../src/lib/code-token.ts"), "utf8"))[1];
const tokens = Object.fromEntries(
  (process.env.CANVAS_CHECK_TOKENS ?? "")
    .split(",")
    .filter(Boolean)
    .map((p) => p.split("="))
    .map(([o, f]) => [o, readFileSync(f, "utf8").trim()]),
);
mkdirSync(join(OUT, "input"), { recursive: true });
const browser =
  engine === "webkit"
    ? await pw.webkit.launch()
    : await pw.chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
const todo = groups().filter((g) => !only || only.test(g.name) || g.variants.some((v) => only.test(`${g.name}-${v.dir[0]}${v.panel ? "-panel" : ""}`)));
for (const g of todo) if (only) g.variants = g.variants.filter((v) => only.test(g.name) || only.test(`${g.name}-${v.dir[0]}${v.panel ? "-panel" : ""}`));
const results = [];
const queue = [...todo];
await Promise.all(
  Array.from({ length: Math.min(jobs, queue.length) }, async () => {
    while (queue.length) results.push(...(await runGroup(browser, queue.shift(), { engine, base, key, tokens, zooms, shots: flag("shots") })));
  }),
);
await browser.close();
// Merge into what an earlier (--only) run left, so the table is always the whole matrix.
const file = join(OUT, `${engine}.json`);
const prev = only && existsSync(file) ? JSON.parse(readFileSync(file, "utf8")) : [];
const merged = [...prev.filter((r) => !results.some((x) => x.case === r.case)), ...results];
writeFileSync(file, JSON.stringify(merged));
console.log(report(only ? results : merged));
