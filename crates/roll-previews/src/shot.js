// roll previews' screenshot and page speed (docs/previews.md, "Surfaces"): one load of the
// page in the headless Chromium shell, driven over the DevTools protocol on a pipe (no port).
// Writes the PNG to $OUT and the timings to $OUT.json. The page is the preview's code: what it
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
(async () => {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  await send("Page.enable", {}, sessionId);
  const loaded = once("Page.loadEventFired");
  await send("Page.navigate", { url: process.env.URL }, sessionId);
  await Promise.race([loaded, sleep(45000)]);
  await sleep(300);
  const speed = await send("Runtime.evaluate", { expression: SPEED, returnByValue: true }, sessionId).catch(() => null);
  const shot = await send("Page.captureScreenshot", { format: "png" }, sessionId);
  fs.writeFileSync(process.env.OUT, Buffer.from(shot.data, "base64"));
  if (speed && typeof speed.result?.value === "string") fs.writeFileSync(process.env.OUT + ".json", speed.result.value);
  chrome.kill();
  process.exit(0);
})().catch((e) => {
  console.error(e.message);
  chrome.kill();
  process.exit(1);
});
