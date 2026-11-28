// Screenshots of the real dashboard for the marketing pages, against the made-up rolld in
// fixtures.mjs (every request to the API is answered by Playwright; nothing else is changed).
//
//   pnpm --filter @roll/dashboard build
//   (cd apps/dashboard && node server.mjs -H 127.0.0.1 -p 3918) &
//   NODE_PATH=/tmp/roll-pw/node_modules node apps/dashboard/scripts/marketing-shots/shots.mjs [name…]
//
// It needs playwright-core (`npm i --prefix /tmp/roll-pw playwright-core`, or run it with
// `npx -p playwright-core node …`) and Google Chrome. PNGs go to /tmp/roll-dash-shots, WebPs to
// src/assets/marketing/dash-<name>.webp (cwebp). `probe <path>` renders one page and lists what it
// asked for that the fixtures don't answer.

import { execFileSync } from "node:child_process";
import { mkdirSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { ORDERS_SQL, SHOP, answer, setScene } from "./fixtures.mjs";

const { chromium } = createRequire(import.meta.url)("playwright-core");

const BASE = process.env.DASH_URL ?? "http://127.0.0.1:3918";
const API = "http://127.0.0.1:7070";
const CHROME = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const PNG_DIR = "/tmp/roll-dash-shots";
// 1360×850 at 3×: the marketing tour zooms in to twice the page's own size, and stays sharp on a 2× screen.
const SCALE = 3;
const WEBP_DIR = join(dirname(fileURLToPath(import.meta.url)), "../../src/assets/marketing");

/** Each shot: the page, the scheme, and what to do before the picture. */
const SHOTS = {
  canvas: { path: `/dashboard/projects/${SHOP}` },
  // en-GB: the tail's times fit their column without an AM/PM.
  streams: { path: `/dashboard/streams`, wait: 6000, locale: "en-GB" },
  service: {
    path: "/dashboard/apps/svc_web",
    before: async (page) => {
      await page.getByText("Build steps", { exact: true }).click();
      await page.waitForTimeout(500);
    },
  },
  secrets: { path: `/dashboard/projects/${SHOP}/secrets` },
  search: { path: "/dashboard/search/sc_catalog#discover?view=search-log&tf=%40timestamp&range=24h&col=query&col=results&col=latency_ms&col=country&col=device", wait: 3000, scene: { loaded: true } },
  // The repo's pages load on the server when opened directly, out of the fixtures' reach: open
  // the code page and follow the link, so they load in the browser.
  code: {
    path: "/dashboard/code",
    before: async (page) => {
      await page.getByText("The storefront, its API and the fulfilment worker.").click();
      await page.waitForURL("**/acme/shop");
      await page.getByRole("navigation", { name: "Repository sections" }).getByRole("button", { name: /^queue/ }).click();
      await page.waitForTimeout(2500);
    },
  },
  // The agents surface's own demo owner (lib/agents-demo.ts, "for screenshots and the marketing
  // page"): simulated in memory, no API involved.
  agents: {
    path: "/dashboard/agents?demo",
    wait: 3000,
    before: async (page) => {
      // Fit twice: everything on the canvas in view.
      const fit = page.getByRole("button", { name: /^Fit/ });
      await fit.click();
      await page.waitForTimeout(500);
      await fit.click();
      await page.mouse.move(800, 600);
      await page.waitForTimeout(1500);
    },
  },
  postgres: {
    path: "/dashboard/databases",
    before: async (page) => {
      await page.locator("textarea").first().fill(ORDERS_SQL);
      await page.getByRole("button", { name: /^Run/ }).click();
      await page.waitForTimeout(2500);
    },
  },
};

async function open(browser, shot, unmatched) {
  const { path, scheme = "dark" } = shot;
  setScene(shot.scene);
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 850 }, deviceScaleFactor: SCALE, colorScheme: scheme, locale: shot.locale ?? "en-US", timezoneId: "Europe/Berlin" });
  await ctx.addInitScript(() => {
    localStorage.setItem("roll.code.token", "roll_admin_demo");
    localStorage.setItem("roll.operator", "1");
  });
  await ctx.route(`${API}/**`, (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const headers = cors(req);
    if (req.method() === "OPTIONS") return route.fulfill({ status: 204, headers });
    let sent = null;
    try {
      sent = req.postDataJSON();
    } catch {}
    const hit = answer(req.method(), url, req.headers(), sent);
    if (!hit) unmatched.add(`${req.method()} ${url.pathname}${url.search}`);
    const { status, body, sse } = hit ?? { status: 404, body: { error: "not_found" } };
    if (sse !== undefined) return route.fulfill({ status, headers, contentType: "text/event-stream", body: sse });
    return route.fulfill({ status, headers, contentType: "application/json", body: JSON.stringify(body) });
  });
  const page = await ctx.newPage();
  page.on("pageerror", (e) => unmatched.add(`PAGE ERROR ${e.message}`));
  page.on("console", (m) => m.type() === "error" && !m.text().includes("404") && unmatched.add(`CONSOLE ${m.text().slice(0, 400)}`));
  await page.goto(BASE + path, { waitUntil: "networkidle" });
  await page.waitForTimeout(shot.wait ?? 1500);
  return { ctx, page };
}

/** The dashboard sends credentials and an Authorization header, so a wildcard won't do. */
const cors = (req) => ({
  "access-control-allow-origin": req.headers().origin ?? BASE,
  "access-control-allow-credentials": "true",
  "access-control-allow-headers": req.headers()["access-control-request-headers"] ?? "authorization, content-type",
  "access-control-allow-methods": "GET, POST, PUT, PATCH, DELETE",
});

async function shoot(browser, name, shot) {
  const unmatched = new Set();
  const { ctx, page } = await open(browser, shot, unmatched);
  if (shot.before) await shot.before(page);
  const png = join(PNG_DIR, `${name}.png`);
  await page.screenshot({ path: png });
  await ctx.close();
  const webp = join(WEBP_DIR, `dash-${name}.webp`);
  execFileSync("cwebp", ["-quiet", "-q", "82", png, "-o", webp]);
  console.log(`${name}: ${png}, ${webp} (${Math.round(statSync(webp).size / 1024)} KB)`);
  for (const u of unmatched) console.log(`  unmatched: ${u}`);
}

mkdirSync(PNG_DIR, { recursive: true });
mkdirSync(WEBP_DIR, { recursive: true });
const browser = await chromium.launch({ executablePath: CHROME });
try {
  const [first, ...rest] = process.argv.slice(2);
  if (first === "probe") {
    const unmatched = new Set();
    const { ctx, page } = await open(browser, { path: rest[0] ?? "/dashboard", scheme: rest[1] }, unmatched);
    await page.screenshot({ path: join(PNG_DIR, "probe.png") });
    await ctx.close();
    for (const u of unmatched) console.log(u);
  } else {
    const names = first ? [first, ...rest] : Object.keys(SHOTS);
    for (const name of names) await shoot(browser, name, SHOTS[name]);
  }
} finally {
  await browser.close();
}
