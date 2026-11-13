// roll previews' screenshot and page speed (docs/previews.md, "Surfaces"): one load of the
// page in the headless Chromium shell, driven over the DevTools protocol on a pipe (no port).
// Writes the PNG to $OUT, and to $OUT.json the timings and what went wrong while it loaded
// (uncaught exceptions, console errors, failed or 4xx/5xx requests). The page is the preview's code: what it
// reports about itself is bounded by roll, never trusted for more than showing.
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const chrome = spawn(process.env.CHROME, ["--headless", "--no-sandbox", "--hide-scrollbars", "--window-size=1280,800", "--remote-debugging-pipe", "about:blank"], {
  stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"],
});
const [, , , toChrome, fromChrome] = chrome.stdio;
let seq = 0;
let buf = Buffer.alloc(0);
const pending = new Map();
const listeners = new Set();
fromChrome.on("data", (d) => {
  buf = Buffer.concat([buf, d]);
  for (let i = buf.indexOf(0); i >= 0; i = buf.indexOf(0)) {
    const m = JSON.parse(buf.subarray(0, i).toString());
    buf = buf.subarray(i + 1);
    if (m.id && pending.has(m.id)) {
      const { ok, fail } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? fail(new Error(m.error.message)) : ok(m.result);
    } else if (m.method) {
      for (const l of [...listeners]) l(m);
    }
  }
});
const send = (method, params = {}, sessionId) =>
  new Promise((ok, fail) => {
    const id = ++seq;
    pending.set(id, { ok, fail });
    toChrome.write(JSON.stringify({ id, method, params, sessionId }) + "\0");
  });
const once = (method) =>
  new Promise((ok) => {
    const l = (m) => {
      if (m.method === method) {
        listeners.delete(l);
        ok(m.params);
      }
    };
    listeners.add(l);
  });
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));
const SPEED = `JSON.stringify((() => {
  const n = performance.getEntriesByType("navigation")[0] || {};
  const fcp = performance.getEntriesByName("first-contentful-paint")[0];
  const bytes = [n, ...performance.getEntriesByType("resource")].reduce((s, e) => s + (e.transferSize || 0), 0);
  return { ttfb_ms: Math.round(n.responseStart || 0), fcp_ms: fcp ? Math.round(fcp.startTime) : null, load_ms: Math.round(n.loadEventEnd || n.duration || 0), bytes };
})())`;
const errors = [];
const failed = [];
const keep = (list, text) => list.length < 50 && list.push(String(text).slice(0, 300));
listeners.add((m) => {
  const p = m.params || {};
  if (m.method === "Runtime.exceptionThrown") keep(errors, p.exceptionDetails?.exception?.description || p.exceptionDetails?.text || "an uncaught exception");
  else if (m.method === "Runtime.consoleAPICalled" && p.type === "error") keep(errors, (p.args || []).map((a) => a.value ?? a.description ?? "").join(" "));
  else if (m.method === "Network.responseReceived" && p.response?.status >= 400) keep(failed, `${p.response.status} ${p.response.url}`);
  else if (m.method === "Network.loadingFailed" && !p.canceled) keep(failed, `${p.errorText} ${p.requestId}`);
});
// What a screen reader can't name, and what a document misses (WCAG's most common failures).
const A11Y = `JSON.stringify((() => {
  const named = (el) => (el.getAttribute("aria-label") || "").trim() || el.getAttribute("aria-labelledby") || (el.getAttribute("title") || "").trim();
  const text = (el) => (el.textContent || "").trim() || [...el.querySelectorAll("img[alt]")].some((i) => i.alt.trim());
  const snip = (el) => el.outerHTML.slice(0, 120);
  const out = (list) => ({ count: list.length, first: list[0] ? snip(list[0]) : null });
  const images = [...document.querySelectorAll("img:not([alt])")].filter((i) => !named(i));
  const fields = [...document.querySelectorAll("input, select, textarea")].filter((f) => {
    if (["hidden", "submit", "button", "reset", "image"].includes((f.getAttribute("type") || "").toLowerCase())) return false;
    return !named(f) && !f.closest("label") && !(f.id && document.querySelector('label[for="' + CSS.escape(f.id) + '"]'));
  });
  const buttons = [...document.querySelectorAll("button, [role=button]")].filter((b) => !named(b) && !text(b));
  const links = [...document.querySelectorAll("a[href]")].filter((a) => !named(a) && !text(a));
  return { images: out(images), fields: out(fields), buttons: out(buttons), links: out(links), lang: !!document.documentElement.getAttribute("lang"), title: !!document.title.trim() };
})())`;
// Before each capture: animations, transitions and the caret stopped, videos paused, and the
// project's $HIDE selectors blanked (their box kept), so only real changes count.
const HIDE = (() => {
  try {
    return JSON.parse(process.env.HIDE || "[]").filter((s) => typeof s === "string").slice(0, 20);
  } catch {
    return [];
  }
})();
const STILL = `(() => {
  const s = document.createElement("style");
  s.textContent = "*,*::before,*::after{animation:none!important;transition:none!important;caret-color:transparent!important;scroll-behavior:auto!important}"
    + ${JSON.stringify(HIDE)}.map((x) => x + "{visibility:hidden!important}").join("");
  (document.head || document.documentElement).appendChild(s);
  document.querySelectorAll("video").forEach((v) => { try { v.pause(); v.currentTime = 0; } catch {} });
  return true;
})()`;
const still = async (sessionId) => {
  await send("Runtime.evaluate", { expression: STILL }, sessionId).catch(() => null);
  await sleep(100);
};
(async () => {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  await send("Page.enable", {}, sessionId);
  await send("Runtime.enable", {}, sessionId);
  await send("Network.enable", {}, sessionId);
  const loaded = once("Page.loadEventFired");
  await send("Page.navigate", { url: process.env.URL }, sessionId);
  await Promise.race([loaded, sleep(45000)]);
  await sleep(300);
  const speed = await send("Runtime.evaluate", { expression: SPEED, returnByValue: true }, sessionId).catch(() => null);
  await still(sessionId);
  const shot = await send("Page.captureScreenshot", { format: "png" }, sessionId);
  fs.writeFileSync(process.env.OUT, Buffer.from(shot.data, "base64"));
  let report = {};
  try {
    report = JSON.parse(speed?.result?.value || "{}");
  } catch {}
  report.errors = errors;
  report.failed = failed;
  const a11y = await send("Runtime.evaluate", { expression: A11Y, returnByValue: true }, sessionId).catch(() => null);
  try {
    report.a11y = JSON.parse(a11y?.result?.value || "null");
  } catch {}
  fs.writeFileSync(process.env.OUT + ".json", JSON.stringify(report));
  // The same page at a phone's width (390 × 844, at twice the density), to $OUT.phone.png. Best
  // effort: the desktop one is kept whatever happens here.
  try {
    await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true }, sessionId);
    const again = once("Page.loadEventFired");
    await send("Page.reload", {}, sessionId);
    await Promise.race([again, sleep(30000)]);
    await sleep(300);
    await still(sessionId);
    const phone = await send("Page.captureScreenshot", { format: "png" }, sessionId);
    fs.writeFileSync(process.env.OUT + ".phone.png", Buffer.from(phone.data, "base64"));
  } catch {}
  // The project's other pages ($PAGES, paths roll checked), at the desktop width, each to
  // $OUT.<n>.png, and what went wrong while each loaded to $OUT.pages.json. Best effort too.
  const pages = [];
  try {
    await send("Emulation.clearDeviceMetricsOverride", {}, sessionId);
    for (const [n, path] of JSON.parse(process.env.PAGES || "[]").slice(0, 5).entries()) {
      const before = errors.length + failed.length;
      const done = once("Page.loadEventFired");
      await send("Page.navigate", { url: new URL(path, process.env.URL).href }, sessionId);
      await Promise.race([done, sleep(10000)]);
      await sleep(300);
      await still(sessionId);
      const shot = await send("Page.captureScreenshot", { format: "png" }, sessionId);
      fs.writeFileSync(`${process.env.OUT}.${n}.png`, Buffer.from(shot.data, "base64"));
      pages.push({ path, errors: errors.length + failed.length - before });
    }
  } catch {}
  fs.writeFileSync(process.env.OUT + ".pages.json", JSON.stringify(pages));
  chrome.kill();
  process.exit(0);
})().catch((e) => {
  console.error(e.message);
  chrome.kill();
  process.exit(1);
});
