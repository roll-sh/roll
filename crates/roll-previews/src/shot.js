// roll previews' screenshot and page speed (docs/previews.md, "Surfaces"): one load of the
// page in the headless Chromium shell, driven over the DevTools protocol on a pipe (no port).
// Writes the PNG to $OUT, and to $OUT.json the timings and what went wrong while it loaded
// (uncaught exceptions, console errors, failed or 4xx/5xx requests). The page is the preview's code: what it
// reports about itself is bounded by roll, never trusted for more than showing; roll's checks of
// it run where its code can't reach them.
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
// Core Web Vitals, observed from before the page's first script (they aren't in the timeline's
// entries): the last largest-contentful-paint candidate, and the largest session window of layout
// shifts (shifts under 1 s apart, 5 s at most, none right after input), as web-vitals counts it.
const VITALS = `(() => {
  const v = (window.__rollVitals = { lcp: null, cls: 0 });
  try {
    new PerformanceObserver((l) => { for (const e of l.getEntries()) v.lcp = e.startTime; }).observe({ type: "largest-contentful-paint", buffered: true });
  } catch {}
  let win = 0, first = 0, last = 0;
  try {
    new PerformanceObserver((l) => {
      for (const e of l.getEntries()) {
        if (e.hadRecentInput) continue;
        if (win && e.startTime - last < 1000 && e.startTime - first < 5000) win += e.value;
        else { win = e.value; first = e.startTime; }
        last = e.startTime;
        v.cls = Math.max(v.cls, win);
      }
    }).observe({ type: "layout-shift", buffered: true });
  } catch {}
})()`;
const SPEED = `JSON.stringify((() => {
  const n = performance.getEntriesByType("navigation")[0] || {};
  const fcp = performance.getEntriesByName("first-contentful-paint")[0];
  const bytes = [n, ...performance.getEntriesByType("resource")].reduce((s, e) => s + (e.transferSize || 0), 0);
  const v = window.__rollVitals;
  return {
    ttfb_ms: Math.round(n.responseStart || 0), fcp_ms: fcp ? Math.round(fcp.startTime) : null, load_ms: Math.round(n.loadEventEnd || n.duration || 0), bytes,
    lcp_ms: v && typeof v.lcp === "number" ? Math.round(v.lcp) : null, cls: v && typeof v.cls === "number" ? Math.round(v.cls * 1000) / 1000 : null,
    text: document.body ? String(document.body.innerText || "").slice(0, 40000) : "",
  };
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
// Every check below runs in a world of its own (an isolated world, as an extension's content
// scripts do): the page's DOM, but none of its JavaScript, so its code can't replace JSON,
// String, PerformanceObserver or axe and report what it likes. Without that world, no check.
const WORLD = "roll";
let mainFrame = null;
let world = null;
listeners.add((m) => {
  const p = m.params || {};
  if (m.method === "Runtime.executionContextCreated" && p.context?.name === WORLD && p.context.auxData?.frameId === mainFrame) world = p.context.id;
  else if (m.method === "Runtime.executionContextDestroyed" && p.executionContextId === world) world = null;
  else if (m.method === "Runtime.executionContextsCleared") world = null;
});
const check = (params, sessionId) => (world == null ? Promise.resolve(null) : send("Runtime.evaluate", { ...params, contextId: world }, sessionId).catch(() => null));
const checkIn = check;
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
// axe-core's WCAG rules (vendor/axe-core, handed to the container in $AXE), when it's there:
// the violations by rule (a language and a title are read as above, so not twice), else the
// checks above. The page can't stop it loading (DevTools evaluation is outside its CSP).
// What search engines look for on the page (Lighthouse's SEO audit): a description, a viewport,
// one h1, and no noindex in the page's own markup (roll's header keeps previews unindexed; the
// app's would hide production).
const SEO = `JSON.stringify({
  description: !!(document.querySelector('meta[name="description" i]')?.getAttribute("content") || "").trim(),
  viewport: !!document.querySelector('meta[name="viewport" i]'),
  h1: document.querySelectorAll("h1").length,
  noindex: [...document.querySelectorAll('meta[name="robots" i], meta[name="googlebot" i]')].some((m) => /noindex/i.test(m.getAttribute("content") || "")),
})`;
// The service's API, when the project names its OpenAPI document ($OPENAPI, a path on the page's
// own origin, JSON, at most 4 MB): every `METHOD /path` it declares (Optic's and Bump's API diff),
// and a line per fact of each (oasdiff's breaking changes): a parameter's or request body field's
// type, whether it's required and its allowed values, and each 2xx response field's type, with
// `$ref`s inside the document followed (fields to a depth of 4). At most 5000 lines; roll checks
// each line's shape (`roll_previews::openapi`).
const OPENAPI = /^\/[^\s#"'`]{0,199}$/.test(process.env.OPENAPI || "") ? process.env.OPENAPI : null;
const API = OPENAPI && `(async () => {
  const c = new AbortController();
  const t = setTimeout(() => c.abort(), 5000);
  try {
    const r = await fetch(new URL(${JSON.stringify(OPENAPI)}, location.origin), { credentials: "same-origin", signal: c.signal });
    if (!r.ok) return null;
    const body = await r.text();
    if (body.length > 4e6) return null;
    const doc = JSON.parse(body);
    const out = [];
    const MAX = 5000;
    const word = (s) => typeof s === "string" && s.length > 0 && s.length <= 100 && !/\\s/.test(s);
    const deref = (s) => {
      for (let n = 0; s && typeof s === "object" && typeof s.$ref === "string" && n < 10; n++) {
        if (!s.$ref.startsWith("#/")) return null;
        s = s.$ref.slice(2).split("/").reduce((o, k) => (o && typeof o === "object" ? o[k.replace(/~1/g, "/").replace(/~0/g, "~")] : undefined), doc);
      }
      return s && typeof s === "object" ? s : null;
    };
    const typeOf = (s, depth = 0) => {
      s = deref(s);
      if (!s || depth > 3) return "any";
      if (Array.isArray(s.type)) return s.type.filter((x) => typeof x === "string").join("|") || "any";
      if (s.type === "array") return "array<" + typeOf(s.items, depth + 1) + ">";
      return typeof s.type === "string" ? s.type : s.properties ? "object" : "any";
    };
    // Allowed values, sorted, when there are at most 200 (more isn't compared at all: a window of
    // them would make values fall in and out of it).
    const values = (s) => {
      const e = deref(s)?.enum;
      if (!Array.isArray(e) || e.length > 200) return [];
      return e.map((v) => JSON.stringify(v)).filter((v) => typeof v === "string" && v.length <= 80).sort();
    };
    // Each field of a schema, by path (\`a.b\`, \`items[].sku\`), with whether its object requires it.
    const fields = (s, prefix, emit, depth) => {
      s = deref(s);
      if (!s || depth > 4 || out.length >= MAX) return;
      for (const part of Array.isArray(s.allOf) ? s.allOf : []) fields(part, prefix, emit, depth + 1);
      const required = new Set(Array.isArray(s.required) ? s.required : []);
      for (const [k, v] of Object.entries(s.properties && typeof s.properties === "object" ? s.properties : {})) {
        if (!word(k)) continue;
        const name = prefix ? prefix + "." + k : k;
        emit(name, v, required.has(k));
        fields(v, name, emit, depth + 1);
      }
      if (s.type === "array" && s.items) fields(s.items, prefix + "[]", emit, depth + 1);
    };
    const json = (content) => (content && typeof content === "object" ? content["application/json"] || Object.values(content)[0] : null);
    for (const [path, item0] of Object.entries(doc && typeof doc.paths === "object" ? doc.paths : {})) {
      const item = deref(item0);
      if (!item || !path.startsWith("/") || /\\s/.test(path) || path.length > 200) continue;
      for (const m of ["get", "put", "post", "delete", "patch", "options", "head", "trace"]) {
        const o = deref(item[m]);
        if (!o || out.length >= MAX) continue;
        const op = m.toUpperCase() + " " + path;
        out.push(op);
        for (const p0 of [...(Array.isArray(item.parameters) ? item.parameters : []), ...(Array.isArray(o.parameters) ? o.parameters : [])]) {
          const p = deref(p0);
          if (!p || !word(p.name) || !word(p.in)) continue;
          const id = p.in + ":" + p.name;
          out.push(op + " param " + id + " type " + typeOf(p.schema));
          if (p.required) out.push(op + " param " + id + " required");
          for (const v of values(p.schema)) out.push(op + " param " + id + " enum " + v);
        }
        const req = json(deref(o.requestBody)?.content);
        if (req) {
          fields(req.schema, "", (name, v, required) => {
            out.push(op + " body " + name + " type " + typeOf(v));
            if (required) out.push(op + " body " + name + " required");
            for (const x of values(v)) out.push(op + " body " + name + " enum " + x);
          }, 0);
        }
        for (const [code, r0] of Object.entries(o.responses && typeof o.responses === "object" ? o.responses : {})) {
          if (!/^2(\\d\\d|XX)$/.test(code)) continue;
          const res = json(deref(r0)?.content);
          if (res) fields(res.schema, "", (name, v) => out.push(op + " response " + code + " " + name + " type " + typeOf(v)), 0);
        }
      }
      if (out.length >= MAX) break;
    }
    // Each line bounded as roll reads it (300 characters): one longer is dropped here, so the
    // report never grows past what roll would keep.
    return JSON.stringify(out.filter((l) => l.length <= 300).slice(0, MAX));
  } catch {
    return null;
  } finally {
    clearTimeout(t);
  }
})()`;
// The project's endpoints ($ENDPOINTS, GET paths on the page's own origin): each timed as the
// median of 10 GETs after one to warm up, its last status kept (0: no answer), at most 2 s a
// request and 20 s in all (an endpoint past that is timed with what it answered). The warm-up's
// JSON answer (up to 1 MiB) is read for its shape: one `path types` line per field ("users[].email
// string", "total number|null"), an empty array as "[]", an object keyed by ids as `*`, at most
// 100 lines, 8 levels, 20 elements an array and 100 keys an object. Values are never kept.
const ENDPOINTS = (() => {
  try {
    return JSON.parse(process.env.ENDPOINTS || "[]").filter((p) => typeof p === "string" && /^\/[^\s#"'`]{0,199}$/.test(p)).slice(0, 5);
  } catch {
    return [];
  }
})();
const TIMES = ENDPOINTS.length > 0 && `(async () => {
  const until = performance.now() + 20000;
  const shapeOf = (v) => {
    const types = new Map();
    const add = (p, t) => {
      if (!types.has(p)) {
        if (types.size >= 100) return false;
        types.set(p, new Set());
      }
      types.get(p).add(t);
      return true;
    };
    const tick = String.fromCharCode(96);
    const key = (k) => [...String(k)].filter((c) => c >= " " && c !== tick && c !== String.fromCharCode(127)).join("").slice(0, 60) || "?";
    const idLike = (k) => /^[0-9]+$/.test(k) || /^[0-9a-fA-F-]{8,}$/.test(k);
    const at = (p, k) => (p === "$" ? k : p + "." + k);
    const walk = (x, p, depth) => {
      const t = x === null ? "null" : Array.isArray(x) ? (x.length ? "array" : "[]") : typeof x;
      if (!["object", "array", "[]", "string", "number", "boolean", "null"].includes(t)) return;
      if (!add(p, t) || depth >= 8) return;
      if (t === "array") for (const e of x.slice(0, 20)) walk(e, p + "[]", depth + 1);
      else if (t === "object") {
        const ks = Object.keys(x).slice(0, 100);
        const ids = ks.length > 0 && ks.every(idLike);
        for (const k of ks) walk(x[k], at(p, ids ? "*" : key(k)), depth + 1);
      }
    };
    walk(v, "$", 0);
    return [...types].map(([p, ts]) => p + " " + [...ts].sort().join("|")).sort();
  };
  const get = async (path, shape) => {
    const c = new AbortController();
    const t = setTimeout(() => c.abort(), 2000);
    const at = performance.now();
    try {
      const r = await fetch(new URL(path, location.origin), { credentials: "same-origin", cache: "no-store", signal: c.signal });
      const body = await r.arrayBuffer();
      let s;
      if (shape && /json/i.test(r.headers.get("content-type") || "") && body.byteLength <= 1048576) {
        try {
          s = shapeOf(JSON.parse(new TextDecoder().decode(body)));
        } catch {}
      }
      return { ms: performance.now() - at, status: r.status, shape: s };
    } catch {
      return { ms: performance.now() - at, status: 0 };
    } finally {
      clearTimeout(t);
    }
  };
  const out = [];
  for (const path of ${JSON.stringify(ENDPOINTS)}) {
    if (performance.now() > until) break;
    const warm = await get(path, true);
    const runs = [];
    let status = 0;
    for (let i = 0; i < 10 && performance.now() < until; i++) {
      const r = await get(path);
      runs.push(r.ms);
      status = r.status;
    }
    if (!runs.length) continue;
    runs.sort((a, b) => a - b);
    out.push(Object.assign({ path, ms: Math.round(runs[Math.floor(runs.length / 2)]), status }, warm.shape ? { shape: warm.shape } : {}));
  }
  return JSON.stringify(out);
})()`;
const AXE_SRC = (() => {
  try {
    return process.env.AXE ? fs.readFileSync(process.env.AXE, "utf8") : "";
  } catch {
    return "";
  }
})();
const AXE_RUN = `axe.run(document, { resultTypes: ["violations"] }).then((r) => JSON.stringify({
  issues: r.violations.filter((v) => v.id !== "html-has-lang" && v.id !== "document-title").slice(0, 50).map((v) => ({
    kind: v.id, count: v.nodes.length, first: v.nodes[0] ? String(v.nodes[0].html).slice(0, 120) : null, impact: v.impact || null, help: String(v.help || "").slice(0, 200),
  })),
  lang: !!document.documentElement.getAttribute("lang"), title: !!document.title.trim(),
}))`;
const axeReport = async (sessionId) => {
  if (!AXE_SRC) return null;
  const loaded = await check({ expression: AXE_SRC + "\n;typeof axe === 'object'", returnByValue: true }, sessionId);
  if (loaded?.result?.value !== true) return null;
  const ran = await Promise.race([check({ expression: AXE_RUN, awaitPromise: true, returnByValue: true }, sessionId), sleep(20000).then(() => null)]);
  try {
    return JSON.parse(ran?.result?.value || "null");
  } catch {
    return null;
  }
};
// The page's links on its own origin, each asked with HEAD only (never followed with a GET, so a
// `/logout` link logs nothing out), at most 50, five at a time, 5 s each: the ones that answer
// 4xx/5xx or nothing (Netlify's link checker). A 405 means it's there.
const LINKS = `(async () => {
  const seen = new Set();
  for (const a of document.querySelectorAll("a[href]")) {
    let u;
    try { u = new URL(a.getAttribute("href"), location.href); } catch { continue; }
    if (u.origin !== location.origin || !/^https?:$/.test(u.protocol)) continue;
    u.hash = "";
    seen.add(u.href);
    if (seen.size >= 50) break;
  }
  const urls = [...seen];
  const check = async (u) => {
    const c = new AbortController();
    const t = setTimeout(() => c.abort(), 5000);
    try {
      return (await fetch(u, { method: "HEAD", redirect: "follow", signal: c.signal, credentials: "same-origin" })).status;
    } catch {
      return 0;
    } finally {
      clearTimeout(t);
    }
  };
  const broken = [];
  for (let i = 0; i < urls.length; i += 5) {
    const got = await Promise.all(urls.slice(i, i + 5).map(check));
    got.forEach((s, k) => { if (s === 0 || (s >= 400 && s !== 405 && s !== 501)) broken.push({ path: new URL(urls[i + k]).pathname, status: s }); });
  }
  return JSON.stringify({ checked: urls.length, broken: broken.slice(0, 20) });
})()`;
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
// The project's page states ($STATES: name → {page, steps}), reached by their steps before the
// capture (Chromatic's interaction tests): `click`, `type` and `press` on the page's own elements,
// `wait`. Run in the checks' world (the page's DOM, none of its JavaScript); the first step that
// finds nothing ends the state's steps and is reported, and the page is taken as it is then.
const STATES = (() => {
  try {
    const s = JSON.parse(process.env.STATES || "{}");
    return s && typeof s === "object" ? s : {};
  } catch {
    return {};
  }
})();
// The project's modes ($MODES: name → {color_scheme, locale, timezone}; Chromatic's Modes): the
// first page taken again in each (`mode:<name>`), the emulation undone after.
const MODES = (() => {
  try {
    const m = JSON.parse(process.env.MODES || "{}");
    return m && typeof m === "object" ? m : {};
  } catch {
    return {};
  }
})();
const KEYS = { Enter: 13, Escape: 27, Tab: 9, ArrowDown: 40, ArrowUp: 38, ArrowLeft: 37, ArrowRight: 39, Space: 32, Backspace: 8 };
const step = async (sessionId, s, until) => {
  const left = () => Math.max(until - Date.now(), 0);
  const check = (params, sid) => Promise.race([checkIn(params, sid), sleep(left()).then(() => null)]);
  const sel = (x) => JSON.stringify(String(x));
  if (typeof s.click === "string") {
    const r = await check({ expression: `(() => { const el = document.querySelector(${sel(s.click)}); if (!el) return false; el.scrollIntoView({ block: "center" }); el.click(); return true; })()`, returnByValue: true }, sessionId);
    return r?.result?.value === true ? null : `click ${s.click}: nothing matched`;
  }
  if (Array.isArray(s.type) && s.type.length === 2) {
    const [target, text] = s.type;
    const r = await check({
      expression: `(() => { const el = document.querySelector(${sel(target)}); if (!el || !("value" in el)) return false; el.focus();
        const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
        Object.getOwnPropertyDescriptor(proto, "value").set.call(el, ${sel(text)});
        el.dispatchEvent(new Event("input", { bubbles: true })); el.dispatchEvent(new Event("change", { bubbles: true })); return true; })()`,
      returnByValue: true,
    }, sessionId);
    return r?.result?.value === true ? null : `type ${target}: nothing to type into`;
  }
  if (typeof s.press === "string" && KEYS[s.press]) {
    const key = s.press === "Space" ? " " : s.press;
    // A key that types (Enter is "\r": it submits a form, presses a focused button) goes down
    // with its text, as a person's does; one that doesn't, as a raw key.
    const text = s.press === "Enter" ? "\r" : key.length === 1 ? key : undefined;
    for (const type of [text ? "keyDown" : "rawKeyDown", "keyUp"]) await send("Input.dispatchKeyEvent", { type, key, code: s.press, windowsVirtualKeyCode: KEYS[s.press], text: type === "keyUp" ? undefined : text }, sessionId).catch(() => null);
    return null;
  }
  if (typeof s.wait === "number") {
    await sleep(Math.min(Math.max(s.wait, 0), 5000));
    return null;
  }
  return "a step roll doesn't know";
};
const still = async (sessionId) => {
  await send("Runtime.evaluate", { expression: STILL }, sessionId).catch(() => null);
  await sleep(100);
};
// Whether the page moves by itself (Argos's and Chromatic's flake detection): a second capture a
// second after the first, stilled as it was. Chrome encodes the same pixels the same way, so
// different data is a page that changed with nobody touching it (a clock, a carousel, an
// animation `hide` didn't name), and a visual change there may be only that.
const moves = async (sessionId, first) => {
  await sleep(1000);
  const again = await send("Page.captureScreenshot", { format: "png" }, sessionId).catch(() => null);
  return !!again && again.data !== first.data;
};
// Storybook's stories ($STORIES: where the service serves Storybook, "/" or "/storybook/";
// Chromatic's stories), in a run of their own: its index (`index.json`, Storybook 7 and later;
// `stories.json` before), the first 40 stories (docs left out), each loaded alone
// (`iframe.html?id=…&viewMode=story`) at the desktop width and captured clipped to what it
// rendered (with a margin, at most 1280 by 2000), to $OUT.story.<n>.png; which story each is and
// what went wrong while it rendered, to $OUT.stories.json. 100 s in all. The index is the
// preview's code's: roll checks each id again.
const STORY_ID = /^[a-z0-9][a-z0-9-]{0,99}--[a-z0-9][a-z0-9-]{0,99}$/;
const ROOT_BOX = `JSON.stringify((() => {
  const root = document.querySelector("#storybook-root") || document.querySelector("#root");
  if (!root || !root.firstElementChild) return null;
  let x1 = Infinity, y1 = Infinity, x2 = -Infinity, y2 = -Infinity;
  for (const el of root.children) {
    const b = el.getBoundingClientRect();
    if (b.width <= 0 || b.height <= 0) continue;
    x1 = Math.min(x1, b.left + scrollX); y1 = Math.min(y1, b.top + scrollY);
    x2 = Math.max(x2, b.right + scrollX); y2 = Math.max(y2, b.bottom + scrollY);
  }
  return x2 > x1 && y2 > y1 ? { x: x1, y: y1, w: x2 - x1, h: y2 - y1 } : null;
})())`;
const stories = async (sessionId) => {
  const base = new URL(process.env.STORIES, process.env.URL);
  let entries = [];
  for (const name of ["index.json", "stories.json"]) {
    try {
      const r = await fetch(new URL(name, base), { signal: AbortSignal.timeout(10000) });
      const text = r.ok ? await r.text() : "";
      if (!text || text.length > 8 << 20) continue;
      const j = JSON.parse(text);
      entries = Object.values((j && (j.entries || j.stories)) || {});
      if (entries.length) break;
    } catch {}
  }
  const picked = entries.filter((e) => e && typeof e.id === "string" && STORY_ID.test(e.id) && (e.type == null || e.type === "story")).slice(0, 40);
  const out = [];
  const until = Date.now() + 100000;
  for (const e of picked) {
    if (Date.now() > until) break;
    const before = errors.length + failed.length;
    const done = once("Page.loadEventFired");
    await send("Page.navigate", { url: new URL(`iframe.html?id=${e.id}&viewMode=story`, base).href }, sessionId);
    await Promise.race([done, sleep(10000)]);
    // A story renders after the load: until its root holds something, 3 s at most.
    let box = null;
    for (let i = 0; i < 30 && !box; i++) {
      const r = await check({ expression: ROOT_BOX, returnByValue: true }, sessionId);
      try {
        box = JSON.parse(r?.result?.value || "null");
      } catch {}
      if (!box) await sleep(100);
    }
    await still(sessionId);
    const m = 8;
    const ok = box && [box.x, box.y, box.w, box.h].every(Number.isFinite);
    const clip = ok ? { x: Math.max(0, box.x - m), y: Math.max(0, box.y - m), width: Math.min(box.w + 2 * m, 1280), height: Math.min(box.h + 2 * m, 2000), scale: 1 } : null;
    const shot = await send("Page.captureScreenshot", clip ? { format: "png", clip, captureBeyondViewport: true } : { format: "png" }, sessionId).catch(() => null);
    if (!shot) continue;
    fs.writeFileSync(`${process.env.OUT}.story.${out.length}.png`, Buffer.from(shot.data, "base64"));
    out.push({ id: e.id, errors: errors.length + failed.length - before });
  }
  fs.writeFileSync(process.env.OUT + ".stories.json", JSON.stringify(out));
};
// A run an agent asked for ($TRY: {path, steps}; roll previews' `…/try`): the page loaded, its
// steps taken as a page state's are (10 at most, 15 s), then its picture to $OUT, and to
// $OUT.try.json its status, the step that found nothing, its visible text (its own line breaks
// kept) and what went wrong on the way.
const tryIt = async (sessionId) => {
  let t = {};
  try {
    t = JSON.parse(process.env.TRY);
  } catch {}
  const path = typeof t.path === "string" && /^\/(?!\/)[^\s#"'`\\]{0,199}$/.test(t.path) ? t.path : "/";
  let status = 0;
  listeners.add((m) => {
    if (m.method === "Network.responseReceived" && m.params?.type === "Document" && !status) status = m.params.response?.status || 0;
  });
  const done = once("Page.loadEventFired");
  await send("Page.navigate", { url: new URL(path, process.env.URL).href }, sessionId);
  await Promise.race([done, sleep(15000)]);
  await sleep(300);
  let stepFailed = null;
  const until = Date.now() + 15000;
  for (const s of (Array.isArray(t.steps) ? t.steps : []).slice(0, 10)) {
    if (Date.now() > until) break;
    stepFailed = await step(sessionId, s || {}, until);
    if (stepFailed) break;
    await sleep(150);
  }
  await sleep(300);
  await still(sessionId);
  const shot = await send("Page.captureScreenshot", { format: "png" }, sessionId).catch(() => null);
  if (shot) fs.writeFileSync(process.env.OUT, Buffer.from(shot.data, "base64"));
  const r = await check({ expression: `JSON.stringify(document.body ? String(document.body.innerText || "").slice(0, 40000) : "")`, returnByValue: true }, sessionId);
  let text = "";
  try {
    text = JSON.parse(r?.result?.value || '""');
  } catch {}
  fs.writeFileSync(process.env.OUT + ".try.json", JSON.stringify({ status, step_failed: stepFailed, text, errors: [...errors, ...failed].slice(0, 20) }));
};
(async () => {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  await send("Page.enable", {}, sessionId);
  // A dialog (`alert`, `confirm`, a "leave this page?") would hold the page until answered:
  // dismissed at once, as Playwright does.
  listeners.add((m) => {
    if (m.method === "Page.javascriptDialogOpening") send("Page.handleJavaScriptDialog", { accept: false }, sessionId).catch(() => null);
  });
  await send("Runtime.enable", {}, sessionId);
  await send("Network.enable", {}, sessionId);
  // What the pages served (documents, the origin's own scripts, fetch and XHR answers), for roll
  // to look for the preview's secret values in (this run is never given them): at most 40
  // bodies, 2 MiB each and 8 MiB in all, to $OUT.bodies.json. Paths only, never a query.
  const served = [];
  let servedBytes = 0;
  const servedWanted = new Map();
  const origin = new URL(process.env.URL).origin;
  listeners.add((m) => {
    const p = m.params || {};
    if (m.method === "Network.responseReceived" && ["Document", "Script", "XHR", "Fetch"].includes(p.type) && p.response?.status < 400) {
      try {
        const u = new URL(p.response.url);
        if (u.origin === origin) servedWanted.set(p.requestId, u.pathname);
      } catch {}
    } else if (m.method === "Network.loadingFinished" && servedWanted.has(p.requestId)) {
      const path = servedWanted.get(p.requestId);
      servedWanted.delete(p.requestId);
      // Each path once (a reload serves it again).
      if (served.length >= 40 || servedBytes >= 8 << 20 || (p.encodedDataLength || 0) > 4 << 20 || served.some((x) => x.path === path)) return;
      send("Network.getResponseBody", { requestId: p.requestId }, sessionId)
        .then((b) => {
          const bytes = Buffer.from(b.body || "", b.base64Encoded ? "base64" : "utf8");
          if (bytes.length > 2 << 20 || served.length >= 40 || servedBytes + bytes.length > 8 << 20 || served.some((x) => x.path === path)) return;
          servedBytes += bytes.length;
          served.push({ path: path.slice(0, 200), b64: bytes.toString("base64") });
        })
        .catch(() => {});
    }
  });
  mainFrame = (await send("Page.getFrameTree", {}, sessionId)).frameTree.frame.id;
  // In the checks' world, made with each document before its first script.
  await send("Page.addScriptToEvaluateOnNewDocument", { source: VITALS, worldName: WORLD }, sessionId).catch(() => null);
  if (process.env.STORIES) {
    await stories(sessionId);
    chrome.kill();
    process.exit(0);
  }
  if (process.env.TRY) {
    await tryIt(sessionId);
    chrome.kill();
    process.exit(0);
  }
  const loaded = once("Page.loadEventFired");
  await send("Page.navigate", { url: process.env.URL }, sessionId);
  await Promise.race([loaded, sleep(45000)]);
  await sleep(300);
  const speed = await check({ expression: SPEED, returnByValue: true }, sessionId);
  await still(sessionId);
  const shot = await send("Page.captureScreenshot", { format: "png" }, sessionId);
  fs.writeFileSync(process.env.OUT, Buffer.from(shot.data, "base64"));
  let report = {};
  try {
    report = JSON.parse(speed?.result?.value || "{}");
  } catch {}
  report.unstable = await moves(sessionId, shot);
  // Copies: what's caught later (the link check's own requests, other pages) isn't this page's.
  report.errors = errors.slice();
  report.failed = failed.slice();
  const links = await Promise.race([check({ expression: LINKS, awaitPromise: true, returnByValue: true }, sessionId), sleep(30000).then(() => null)]);
  try {
    report.links = JSON.parse(links?.result?.value || "null");
  } catch {}
  const seo = await check({ expression: SEO, returnByValue: true }, sessionId);
  try {
    report.seo = JSON.parse(seo?.result?.value || "null");
  } catch {}
  if (TIMES) {
    const times = await Promise.race([check({ expression: TIMES, awaitPromise: true, returnByValue: true }, sessionId), sleep(25000).then(() => null)]);
    try {
      report.endpoints = JSON.parse(times?.result?.value || "[]");
    } catch {}
  }
  if (API) {
    const api = await Promise.race([check({ expression: API, awaitPromise: true, returnByValue: true }, sessionId), sleep(6000).then(() => null)]);
    try {
      report.api = JSON.parse(api?.result?.value || "null");
    } catch {}
  }
  report.a11y = await axeReport(sessionId);
  if (!report.a11y) {
    const a11y = await check({ expression: A11Y, returnByValue: true }, sessionId);
    try {
      report.a11y = JSON.parse(a11y?.result?.value || "null");
    } catch {}
  }
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
  // The project's other pages ($PAGES, paths roll checked), at the desktop width, and the first
  // page at its other widths (`@768`), each to $OUT.<n>.png, and what went wrong while each
  // loaded to $OUT.pages.json. Best effort too.
  const pages = [];
  try {
    for (const [n, path] of JSON.parse(process.env.PAGES || "[]").slice(0, 10).entries()) {
      const width = /^@[1-9][0-9]*$/.test(path) ? Number(path.slice(1)) : null;
      const mode = /^mode:[a-z0-9-]{1,20}$/.test(path) ? MODES[path.slice(5)] : null;
      if (mode) {
        if (mode.color_scheme) await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: mode.color_scheme }] }, sessionId).catch(() => null);
        if (mode.locale) {
          await send("Emulation.setLocaleOverride", { locale: mode.locale }, sessionId).catch(() => null);
          await send("Network.setExtraHTTPHeaders", { headers: { "Accept-Language": mode.locale } }, sessionId).catch(() => null);
        }
        if (mode.timezone) await send("Emulation.setTimezoneOverride", { timezoneId: mode.timezone }, sessionId).catch(() => null);
      }
      const state = /^state:[a-z0-9-]{1,40}$/.test(path) ? STATES[path.slice(6)] : null;
      const at = state && typeof state.page === "string" && /^\/(?!\/)[^\s#"'`\\]{0,199}$/.test(state.page) ? state.page : null;
      if (width) await send("Emulation.setDeviceMetricsOverride", { width, height: 900, deviceScaleFactor: 1, mobile: width < 600 }, sessionId);
      else await send("Emulation.clearDeviceMetricsOverride", {}, sessionId);
      const before = errors.length + failed.length;
      const done = once("Page.loadEventFired");
      await send("Page.navigate", { url: width || mode ? process.env.URL : new URL(at || path, process.env.URL).href }, sessionId);
      await Promise.race([done, sleep(10000)]);
      await sleep(300);
      // A state's steps, at most 10, 15 s in all; the first that fails ends them.
      let stepFailed = null;
      if (state) {
        const until = Date.now() + 15000;
        for (const s of (Array.isArray(state.steps) ? state.steps : []).slice(0, 10)) {
          if (Date.now() > until) break;
          stepFailed = await step(sessionId, s || {}, until);
          if (stepFailed) break;
          await sleep(150);
        }
        await sleep(300);
      }
      await still(sessionId);
      const shot = await send("Page.captureScreenshot", { format: "png" }, sessionId);
      fs.writeFileSync(`${process.env.OUT}.${n}.png`, Buffer.from(shot.data, "base64"));
      const page = { path, errors: errors.length + failed.length - before, unstable: await moves(sessionId, shot) };
      if (mode) {
        await send("Emulation.setEmulatedMedia", { features: [] }, sessionId).catch(() => null);
        await send("Emulation.setLocaleOverride", {}, sessionId).catch(() => null);
        await send("Network.setExtraHTTPHeaders", { headers: {} }, sessionId).catch(() => null);
        await send("Emulation.setTimezoneOverride", { timezoneId: "" }, sessionId).catch(() => null);
      }
      if (stepFailed) page.step = stepFailed;
      // The same checks as the first page's, each bounded in time.
      const value = async (expression, wait) => {
        const r = await Promise.race([check({ expression, awaitPromise: !!wait, returnByValue: true }, sessionId), sleep(wait || 5000).then(() => null)]);
        try {
          return JSON.parse(r?.result?.value || "null");
        } catch {
          return null;
        }
      };
      const textOf = await value(`JSON.stringify(document.body ? String(document.body.innerText || "").slice(0, 40000) : "")`);
      if (typeof textOf === "string") page.text = textOf;
      page.seo = await value(SEO);
      page.links = await value(LINKS, 20000);
      page.a11y = (await axeReport(sessionId)) || (await value(A11Y));
      pages.push(page);
    }
  } catch {}
  fs.writeFileSync(process.env.OUT + ".pages.json", JSON.stringify(pages));
  // The open notes' elements ($CROPS: [{note, path, selector}], checked by roll), each found by
  // its selector in roll's world on its page at the desktop width, scrolled into view and cropped
  // with a margin, to $OUT.crop.<n>.png; which note each is to $OUT.crops.json. A selector that
  // finds nothing, or nothing visible, is left out. Best effort, as the pages are.
  const crops = [];
  try {
    const asked = JSON.parse(process.env.CROPS || "[]").filter((c) => c && typeof c.note === "string" && typeof c.path === "string" && typeof c.selector === "string").slice(0, 10);
    await send("Emulation.clearDeviceMetricsOverride", {}, sessionId);
    for (const c of asked) {
      if (!/^\/(?!\/)[^\s#"'`\\]{0,199}$/.test(c.path)) continue;
      const done = once("Page.loadEventFired");
      await send("Page.navigate", { url: new URL(c.path, process.env.URL).href }, sessionId);
      await Promise.race([done, sleep(10000)]);
      await sleep(300);
      await still(sessionId);
      const r = await Promise.race([
        check(
          {
            expression: `JSON.stringify((() => { const el = document.querySelector(${JSON.stringify(c.selector)}); if (!el) return null; el.scrollIntoView({ block: "center", inline: "center" }); const b = el.getBoundingClientRect(); return b.width > 0 && b.height > 0 ? { x: b.left + scrollX, y: b.top + scrollY, w: b.width, h: b.height } : null; })())`,
            returnByValue: true,
          },
          sessionId,
        ),
        sleep(5000).then(() => null),
      ]);
      let box = null;
      try {
        box = JSON.parse(r?.result?.value || "null");
      } catch {}
      if (!box || ![box.x, box.y, box.w, box.h].every(Number.isFinite)) continue;
      await sleep(150);
      // A margin around it, at most 1280 by 800 (a huge element is cropped to its top left).
      const m = 24;
      const x = Math.max(0, box.x - m), y = Math.max(0, box.y - m);
      const clip = { x, y, width: Math.min(box.w + 2 * m, 1280), height: Math.min(box.h + 2 * m, 800), scale: 1 };
      const shot = await send("Page.captureScreenshot", { format: "png", clip, captureBeyondViewport: true }, sessionId).catch(() => null);
      if (!shot) continue;
      fs.writeFileSync(`${process.env.OUT}.crop.${crops.length}.png`, Buffer.from(shot.data, "base64"));
      crops.push({ note: c.note, n: crops.length });
    }
  } catch {}
  fs.writeFileSync(process.env.OUT + ".crops.json", JSON.stringify(crops));
  await sleep(200);
  fs.writeFileSync(process.env.OUT + ".bodies.json", JSON.stringify(served));
  chrome.kill();
  process.exit(0);
})().catch((e) => {
  console.error(e.message);
  chrome.kill();
  process.exit(1);
});
