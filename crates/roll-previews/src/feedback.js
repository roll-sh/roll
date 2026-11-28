// roll previews' feedback button (docs/previews.md, "Feedback on the preview page"). The preview's
// proxy adds it to the pages of people who came in through roll. It holds no credential: it picks
// an element, takes a note, and opens roll, where the signed-in person posts it on the change. A
// share link's guest (the link takes notes) posts it from here instead, with a name, through the
// preview's own host (`/__roll/notes`, the link's cookie); they see only their link's notes.
(() => {
  if (window.__rollFeedback) return;
  window.__rollFeedback = true;
  const TARGET = __ROLL_TARGET__;
  const GUEST = __ROLL_GUEST__;
  // The project's flags a reviewer sets for their own view (Vercel's Flags Explorer): kept in a
  // cookie of this browser's, which roll's proxy turns into the services' `x-roll-flags`.
  const FLAGS = __ROLL_FLAGS__;
  // This page on main's preview (Vercel's deployment switcher): a team visitor's, where roll
  // previews serves the page (`/__roll/main`); elsewhere (roll apps' hosts) the name is unset.
  const MAIN = typeof __ROLL_MAIN__ !== "undefined" && __ROLL_MAIN__;
  // What went wrong on the page, sent with a note (the person sees it in roll before posting it):
  // uncaught errors since this script ran, and requests answered 400 or more, earlier ones too
  // (the browser's own resource timings, buffered). Paths only, never a query, each cut short.
  // Nothing of the page's is wrapped (console, fetch): its devtools stay its own.
  const errors = [], failed = new Map();
  const cut = (s, n) => String(s).replace(/\s+/g, " ").trim().slice(0, n);
  // What the reviewer did before the note (Jam's user actions): the last 20 clicks (by a
  // selector), inputs (how many characters, never what), choices, form submits, Enter and Escape,
  // and page changes, each with how long before the note. Nothing of roll's own toolbar, nor the
  // element picked for the note. The page's own functions are left as they are (a page change is
  // noticed by looking at the address twice a second).
  const steps = [];
  const step = (what) => {
    steps.push([Date.now(), cut(what, 120)]);
    if (steps.length > 20) steps.shift();
  };
  const mine = (e) => e.composedPath().includes(host) || picking;
  const sel = (t) => (t instanceof Element ? selector(t) : "");
  addEventListener("click", (e) => { if (!mine(e) && e.target instanceof Element) step(`click ${sel(e.target)}`); }, true);
  addEventListener("change", (e) => {
    const t = e.target;
    if (mine(e) || !(t instanceof Element)) return;
    const kind = t.type === "checkbox" || t.type === "radio" ? (t.checked ? "check" : "uncheck") : t.localName === "select" ? (t.multiple ? "choose in" : `choose option ${t.selectedIndex + 1} in`) : `type ${String(t.value || "").length} characters in`;
    step(`${kind} ${sel(t)}`);
  }, true);
  addEventListener("submit", (e) => { if (!mine(e)) step(`submit ${sel(e.target)}`); }, true);
  addEventListener("keydown", (e) => { if (!mine(e) && (e.key === "Enter" || e.key === "Escape")) step(`press ${e.key} in ${sel(e.target)}`); }, true);
  let at = location.pathname;
  setInterval(() => {
    if (location.pathname !== at) {
      at = location.pathname;
      step(`go to ${at}`);
    }
  }, 500);
  const keep = (s) => { if (s && !errors.includes(s)) { errors.push(s); if (errors.length > 5) errors.shift(); } };
  addEventListener("error", (e) => { if (e.message) keep(cut(e.message, 200)); });
  addEventListener("unhandledrejection", (e) => keep(cut(`Unhandled rejection: ${e.reason instanceof Error ? e.reason.message : e.reason}`, 200)));
  const pathOf = (u) => { try { const x = new URL(u, location.href); return x.origin === location.origin ? x.pathname : x.host + x.pathname; } catch { return ""; } };
  try {
    new PerformanceObserver((l) => {
      for (const r of l.getEntries()) {
        const p = pathOf(r.name);
        // roll's own (this button's) aren't the page's.
        if (r.responseStatus >= 400 && !p.startsWith("/__roll/")) { failed.delete(p); failed.set(p, `${cut(p, 160)} ${r.responseStatus}`); }
      }
    }).observe({ type: "resource", buffered: true });
  } catch {}
  const host = document.createElement("div");
  host.setAttribute("data-roll-feedback", "");
  host.style.cssText = "all:initial;position:fixed;right:16px;bottom:16px;z-index:2147483647";
  const root = host.attachShadow({ mode: "closed" });
  root.innerHTML = `<style>
:host{all:initial}
[hidden]{display:none!important}
*{box-sizing:border-box;font:12.5px/1.4 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}
.btn{display:flex;align-items:center;gap:6px;height:30px;padding:0 12px;border-radius:999px;border:1px solid rgba(255,255,255,.14);
 background:#0a0a0a;color:#ededed;cursor:pointer;box-shadow:0 8px 24px rgba(0,0,0,.35)}
.btn:hover{border-color:rgba(255,255,255,.28)}
.btn:focus-visible,textarea:focus-visible,button:focus-visible{outline:2px solid #4c8dff;outline-offset:2px}
.dot{width:6px;height:6px;border-radius:50%;background:#4c8dff}
.card{width:300px;padding:12px;border-radius:12px;border:1px solid rgba(255,255,255,.14);background:#0a0a0a;color:#ededed;
 box-shadow:0 12px 32px rgba(0,0,0,.45);display:grid;gap:8px}
.muted{color:#8f8f8f;font-size:11.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:11px}
textarea,input.name{width:100%;padding:8px;border-radius:8px;border:1px solid rgba(255,255,255,.14);background:#000;color:#ededed}
textarea{min-height:72px;resize:vertical}
.row{display:flex;gap:6px;justify-content:flex-end}
.resolve{height:22px;padding:0 8px;border-radius:6px;border:1px solid rgba(255,255,255,.14);background:transparent;color:#8f8f8f;cursor:pointer;font-size:11px}
.resolve:hover{color:#ededed}
.row button{height:28px;padding:0 10px;border-radius:7px;border:1px solid rgba(255,255,255,.14);background:transparent;color:#ededed;cursor:pointer}
.row button.go{background:#4c8dff;border-color:#4c8dff;color:#fff}
.bar{display:flex;gap:6px;justify-content:flex-end}
.count{height:30px;min-width:30px;padding:0 9px;border-radius:999px;border:1px solid rgba(255,255,255,.14);background:#0a0a0a;color:#ededed;
 cursor:pointer;box-shadow:0 8px 24px rgba(0,0,0,.35);font-variant-numeric:tabular-nums}
.list{width:300px;max-height:320px;overflow:auto;padding:6px;border-radius:12px;border:1px solid rgba(255,255,255,.14);background:#0a0a0a;
 color:#ededed;box-shadow:0 12px 32px rgba(0,0,0,.45);margin-bottom:8px}
.note{padding:8px;border-radius:8px}
.note:hover{background:rgba(255,255,255,.05)}
.note p{margin:4px 0 0;white-space:pre-wrap;word-break:break-word}
.reply{margin:6px 0 0 8px;padding-left:8px;border-left:1px solid rgba(255,255,255,.14)}
.acts{display:flex;gap:6px;margin-top:6px}
.shot{display:flex;align-items:center;gap:8px}
.shot img{height:40px;max-width:96px;border-radius:4px;border:1px solid rgba(255,255,255,.14);object-fit:cover}
.review .head{display:flex;align-items:center;gap:8px}
.review .pct{margin-left:auto;font-variant-numeric:tabular-nums;color:#ededed}
.review .pics{display:flex;gap:6px;margin-top:6px}
.review .pics a{flex:1;min-width:0;display:grid;gap:2px;color:#8f8f8f;text-decoration:none;font-size:10.5px}
.review .pics img{width:100%;height:64px;object-fit:cover;object-position:top;border-radius:4px;border:1px solid rgba(255,255,255,.14);background:#000}
.review .bad{color:#ff6166}
.review .accept{color:#4c8dff}
.box{position:fixed;pointer-events:none;border:2px solid #4c8dff;border-radius:4px;background:rgba(76,141,255,.08);display:none}
@media (prefers-reduced-motion:no-preference){.box{transition:all .12s ease-out}}
</style>
<div class="box"></div>
<div class="list" hidden role="list" aria-label="Feedback on this page"></div>
<form class="list flags" hidden aria-label="Flags for your view"></form>
<div class="list review" hidden role="list" aria-label="This page against main's"></div>
<div class="bar"><button class="count reviewbtn" type="button" hidden aria-label="This page against main's" aria-expanded="false">Review</button><button class="count mainbtn" type="button" hidden aria-label="This page on main's preview">Main</button><button class="count flagbtn" type="button" hidden aria-label="Flags for your view">Flags</button><button class="count" type="button" hidden aria-label="Feedback on this page"></button><button class="btn" type="button" aria-label="Leave feedback on this page"><span class="dot"></span>Feedback</button></div>
<form class="card" hidden>
  <div class="muted">On <code class="where"></code></div>
  <input class="name" placeholder="Your name" aria-label="Your name" maxlength="40" hidden>
  <textarea placeholder="What should change here?" aria-label="Your feedback" required maxlength="4000"></textarea>
  <div class="shot"><button type="button" class="resolve attach">Attach my screen</button><img alt="Your screen" hidden><span class="muted shotsaid"></span></div>
  <div class="muted said" hidden></div>
  <div class="row"><button type="button" class="cancel">Cancel</button><button type="submit" class="go">Post in roll</button></div>
</form>`;
  const $ = (s) => root.querySelector(s);
  const btn = $(".btn"), form = $("form.card"), box = $(".box"), note = $("textarea"), where = $(".where"), count = $(".count:not(.flagbtn):not(.mainbtn):not(.reviewbtn)"), list = $("div.list");
  let picking = false, picked = null;
  const name = $("input.name"), said = $(".said");
  // The reviewer's own picture of this tab (Jam's, Marker.io's), only when they click: the
  // browser asks them, one frame is kept and the capture stops at once; refused, the note still
  // goes. Sent to this preview's host straight away, which keeps it for the note (its token).
  const attach = $(".attach"), bar = $(".bar"), thumb = $(".shot img"), shotSaid = $(".shotsaid");
  let screen = null;
  const SIDE = 1600, MAX = 1 << 20;
  const frame = async (stream) => {
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.srcObject = stream;
    await video.play();
    // A frame drawn after roll's card was hidden (two, where the browser can say).
    for (let i = 0; i < 2; i++) await new Promise((r) => (video.requestVideoFrameCallback ? video.requestVideoFrameCallback(() => r()) : requestAnimationFrame(() => r())));
    const k = Math.min(1, SIDE / Math.max(video.videoWidth, video.videoHeight));
    const c = document.createElement("canvas");
    c.width = Math.max(1, Math.round(video.videoWidth * k));
    c.height = Math.max(1, Math.round(video.videoHeight * k));
    c.getContext("2d").drawImage(video, 0, 0, c.width, c.height);
    const blob = (type, q) => new Promise((r) => c.toBlob(r, type, q));
    let b = await blob("image/webp", 0.8);
    if (!b || b.type !== "image/webp") b = await blob("image/jpeg", 0.8);
    if (b && b.size > MAX) b = await blob("image/jpeg", 0.5);
    return b;
  };
  attach.addEventListener("click", async () => {
    if (!navigator.mediaDevices?.getDisplayMedia) { shotSaid.textContent = "This browser can't take one"; return; }
    attach.disabled = true;
    shotSaid.textContent = "";
    let stream = null, b = null;
    try {
      stream = await navigator.mediaDevices.getDisplayMedia({ preferCurrentTab: true, video: true, audio: false });
      // roll's own card and buttons aren't the page's (the picked element's outline stays).
      form.style.visibility = bar.style.visibility = "hidden";
      b = await frame(stream);
    } catch {
      b = null;
    } finally {
      if (stream) for (const t of stream.getTracks()) t.stop();
      form.style.visibility = bar.style.visibility = "";
      attach.disabled = false;
    }
    if (!b) { shotSaid.textContent = "Not attached"; return; }
    const url = URL.createObjectURL(b);
    thumb.src = url;
    thumb.hidden = false;
    attach.textContent = "Take it again";
    shotSaid.textContent = "Sending…";
    const sent = fetch("/__roll/screen", { method: "POST", credentials: "same-origin", headers: { "content-type": b.type }, body: b })
      .then(async (r) => {
        if (!r.ok) throw new Error((await r.text()).slice(0, 120) || `${r.status}`);
        const j = await r.json();
        shotSaid.textContent = "Attached";
        return typeof j.screen === "string" ? j.screen : "";
      })
      .catch((err) => { shotSaid.textContent = `Not attached: ${err.message}`; thumb.hidden = true; return ""; });
    screen = sent;
  });
  if (GUEST) {
    name.hidden = false;
    name.required = true;
    try { name.value = localStorage.getItem("roll_guest_name") || ""; } catch {}
    form.querySelector(".go").textContent = "Send";
  }
  const mainBtn = $(".mainbtn");
  if (MAIN && !GUEST) {
    mainBtn.hidden = false;
    mainBtn.addEventListener("click", async () => {
      mainBtn.disabled = true;
      let url = null;
      try {
        const r = await fetch(`/__roll/main?path=${encodeURIComponent(location.pathname + location.search)}`, { credentials: "same-origin" });
        const j = r.ok ? await r.json() : null;
        if (j && typeof j.url === "string" && /^https?:\/\//.test(j.url)) url = j.url;
      } catch {}
      mainBtn.disabled = false;
      if (url) window.open(url, "_blank", "noopener");
      else mainBtn.textContent = "No main preview";
    });
  }
  // Review (Chromatic's UI Review, on the page): this page against main's preview, as roll's
  // record has it: how much each of its entries differs (the first page's widths, modes and
  // browsers too, a state that starts here), main's picture and this one's, and the problems it
  // has that main's page doesn't. Accepting is roll's: "Accept in roll" opens the sheet at that
  // page, so nothing on this page (its own script included) can accept for anyone.
  const reviewBtn = $(".reviewbtn"), reviewList = $("div.review");
  const LABEL = (k) => (k === "" ? "This page" : k.startsWith("@") ? `At ${k.slice(1)}px` : k.startsWith("mode:") ? `Mode ${k.slice(5)}` : k.startsWith("browser:") ? `In ${k.slice(8)}` : k.startsWith("state:") ? `State ${k.slice(6)}` : k);
  const el = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text != null) n.textContent = text; return n; };
  const plural = (n, one, many) => `${n} new ${n === 1 ? one : many}`;
  const reviewEntry = (pg, branch) => {
    const item = el("div", "note");
    item.setAttribute("role", "listitem");
    const head = el("div", "head");
    head.append(el("code", null, LABEL(String(pg.page || ""))));
    const pct = typeof pg.percent === "number" ? pg.percent : null;
    head.append(el("span", "pct", pct === null ? "not compared" : pct > 0 ? `${Math.max(pct, 0.1).toFixed(1)}% changed` : `same as ${branch}`));
    item.append(head);
    const added = pg.added || {}, problems = pg.problems || {};
    const said = [];
    if (added.a11y > 0) said.push(plural(added.a11y, "accessibility problem", "accessibility problems"));
    if (added.errors > 0) said.push(plural(added.errors, "console error or failed request", "console errors or failed requests"));
    if (added.links > 0) said.push(plural(added.links, "broken link", "broken links"));
    if (added.seo > 0) said.push(plural(added.seo, "SEO problem", "SEO problems"));
    if (said.length) item.append(el("p", "bad", `${said.join(" · ")} against ${branch}`));
    const detail = [];
    if (added.a11y > 0) for (const i of problems.a11y || []) detail.push(`${i.kind} ×${i.count}`);
    if (added.links > 0) for (const b of problems.links || []) detail.push(`${b.path} ${b.status || "no answer"}`);
    if (added.errors > 0) for (const e of [problems.first_error, problems.first_failed]) if (e) detail.push(e);
    if (problems.errors > 0) detail.push(`${problems.errors} ${problems.errors === 1 ? "error" : "errors"} while it loaded`);
    for (const d of detail.slice(0, 6)) item.append(el("div", "muted", d));
    if (pg.unstable) item.append(el("div", "muted", "It moves by itself: the difference may be only that"));
    const pics = pg.pictures || {};
    if (pct > 0 && pics.after) {
      const row = el("div", "pics");
      for (const [k, name] of [["before", branch], ["after", "This push"], ["diff", "Where it differs"]]) {
        if (typeof pics[k] !== "string" || !pics[k].startsWith("/__roll/")) continue;
        const a = el("a");
        a.href = pics[k];
        a.target = "_blank";
        a.rel = "noopener";
        const img = el("img");
        img.alt = `${name}: ${LABEL(String(pg.page || ""))}`;
        img.loading = "lazy";
        img.src = pics[k];
        a.append(img, el("span", null, name));
        row.append(a);
      }
      item.append(row);
    }
    const acts = el("div", "acts");
    if (pg.accepted_by) acts.append(el("span", "muted", `Accepted by ${pg.accepted_by}`));
    else if (pg.refused) acts.append(el("span", "muted", `Refused by ${pg.refused.by}${pg.refused.reason ? `: ${pg.refused.reason}` : ""}`));
    if (pct > 0 && !pg.accepted_by && TARGET) {
      const b = el("button", "resolve accept", "Accept in roll");
      b.type = "button";
      b.addEventListener("click", () => window.open(`${TARGET}&${new URLSearchParams({ fb_page: String(pg.page || "") })}`, "_blank", "noopener"));
      acts.append(b);
    }
    if (acts.childNodes.length) item.append(acts);
    return item;
  };
  const review = (r) => {
    if (!r || !r.compared || !Array.isArray(r.pages) || !r.pages.length) return;
    const branch = typeof r.branch === "string" ? r.branch : "main";
    const most = Math.max(0, ...r.pages.map((p) => (typeof p.percent === "number" ? p.percent : 0)));
    const added = r.pages.reduce((n, p) => n + Object.values(p.added || {}).reduce((m, k) => m + (k > 0 ? k : 0), 0), 0);
    reviewBtn.textContent = `Review${most > 0 ? ` · ${Math.max(most, 0.1).toFixed(1)}%` : ""}${added ? ` · ${added} new` : ""}`;
    reviewBtn.title = most > 0 || added ? `This page differs from ${branch}'s preview${added ? `: ${added} new ${added === 1 ? "problem" : "problems"}` : ""}` : `This page looks like ${branch}'s preview`;
    reviewBtn.hidden = false;
    reviewList.replaceChildren(...r.pages.map((p) => reviewEntry(p, branch)));
  };
  if (MAIN && !GUEST) {
    reviewBtn.addEventListener("click", () => {
      reviewList.hidden = !reviewList.hidden;
      reviewBtn.setAttribute("aria-expanded", String(!reviewList.hidden));
    });
    let seen = null;
    const loadReview = () => {
      if (seen === location.pathname) return;
      seen = location.pathname;
      reviewBtn.hidden = reviewList.hidden = true;
      fetch(`/__roll/review.json?path=${encodeURIComponent(location.pathname)}`, { credentials: "same-origin" })
        .then((r) => (r.ok ? r.json() : null))
        .then(review)
        .catch(() => {});
    };
    loadReview();
    // A client-side page change: this page's review, not the last one's.
    setInterval(loadReview, 1000);
  }
  // Flags: each a select (the app's own choice, or one of its options); applied for this
  // browser only, by reloading with the cookie set.
  const flagBtn = $(".flagbtn"), flagForm = $(".flags");
  if (Array.isArray(FLAGS) && FLAGS.length) {
    flagBtn.hidden = false;
    const now = Object.fromEntries(
      (document.cookie.split("; ").find((c) => c.startsWith("roll_flags=")) || "roll_flags=")
        .slice(11)
        .split("|")
        .filter(Boolean)
        .map((p) => p.split(":")),
    );
    if (Object.keys(now).length) flagBtn.textContent = `Flags · ${Object.keys(now).length}`;
    for (const f of FLAGS) {
      const row = document.createElement("label");
      row.className = "note";
      row.style.cssText = "display:flex;gap:8px;align-items:center;justify-content:space-between";
      const name = document.createElement("code");
      name.textContent = f.name;
      const sel = document.createElement("select");
      sel.name = f.name;
      sel.style.cssText = "background:#000;color:#ededed;border:1px solid rgba(255,255,255,.14);border-radius:6px;height:24px";
      for (const v of ["", ...f.options]) {
        const o = document.createElement("option");
        o.value = v;
        o.textContent = v || "the app's own";
        o.selected = (now[f.name] || "") === v;
        sel.append(o);
      }
      row.append(name, sel);
      flagForm.append(row);
    }
    const apply = document.createElement("div");
    apply.className = "row";
    apply.innerHTML = '<button type="submit" class="go">Apply to my view</button>';
    flagForm.append(apply);
    flagBtn.addEventListener("click", () => {
      flagForm.hidden = !flagForm.hidden;
    });
    flagForm.addEventListener("submit", (e) => {
      e.preventDefault();
      const pairs = [...flagForm.querySelectorAll("select")].filter((s) => s.value).map((s) => `${s.name}:${s.value}`);
      document.cookie = pairs.length ? `roll_flags=${pairs.join("|")}; path=/; SameSite=Lax; max-age=2592000` : "roll_flags=; path=/; max-age=0";
      location.reload();
    });
  }

  // A short selector a person can read: an id, or the tag, two classes and its place, four levels up.
  const selector = (el) => {
    const parts = [];
    for (let n = el; n && n.nodeType === 1 && parts.length < 4; n = n.parentElement) {
      if (n.id && /^[A-Za-z][\w-]*$/.test(n.id)) { parts.unshift(`#${n.id}`); break; }
      let s = n.localName;
      const cls = [...n.classList].filter((c) => /^[A-Za-z_][\w-]*$/.test(c)).slice(0, 2);
      if (cls.length) s += "." + cls.join(".");
      const same = n.parentElement ? [...n.parentElement.children].filter((c) => c.localName === n.localName) : [];
      if (same.length > 1) s += `:nth-of-type(${same.indexOf(n) + 1})`;
      parts.unshift(s);
      if (n.localName === "body") break;
    }
    return parts.join(" > ");
  };
  const outline = (el) => {
    const r = el.getBoundingClientRect();
    Object.assign(box.style, { display: "block", left: `${r.left}px`, top: `${r.top}px`, width: `${r.width}px`, height: `${r.height}px` });
  };
  const stop = () => {
    picking = false;
    document.documentElement.style.cursor = "";
    document.removeEventListener("mousemove", move, true);
    document.removeEventListener("click", pick, true);
  };
  const reset = () => {
    stop();
    picked = null;
    screen = null;
    thumb.hidden = true;
    thumb.removeAttribute("src");
    attach.textContent = "Attach my screen";
    shotSaid.textContent = "";
    said.hidden = true;
    form.hidden = true;
    btn.hidden = false;
    box.style.display = "none";
    note.value = "";
  };
  const ours = (e) => e.composedPath().includes(host);
  const move = (e) => { if (!ours(e) && e.target instanceof Element) outline(e.target); };
  const pick = (e) => {
    if (ours(e) || !(e.target instanceof Element)) return;
    e.preventDefault();
    e.stopPropagation();
    stop();
    const r = e.target.getBoundingClientRect();
    picked = { el: e.target, selector: selector(e.target), box: [r.left + scrollX, r.top + scrollY, r.width, r.height].map(Math.round).join(",") };
    outline(e.target);
    where.textContent = `${location.pathname} · ${picked.selector}`;
    btn.hidden = true;
    form.hidden = false;
    note.focus();
  };
  btn.addEventListener("click", () => {
    picking = true;
    document.documentElement.style.cursor = "crosshair";
    document.addEventListener("mousemove", move, true);
    document.addEventListener("click", pick, true);
  });
  $(".cancel").addEventListener("click", reset);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && (picking || !form.hidden)) reset(); });
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!picked || !note.value.trim()) return;
    // Its picture's token (sent when it was taken; waited for only if it's still on its way).
    const shot = screen ? await screen : "";
    const ctx = {
      steps: steps.map(([t, what]) => `${Math.max(0, Math.round((Date.now() - t) / 1000))}s before · ${what}`),
      errors,
      requests: [...failed.values()].slice(-5),
    };
    if (GUEST) {
      try { localStorage.setItem("roll_guest_name", name.value.trim()); } catch {}
      const go = form.querySelector(".go");
      go.disabled = true;
      fetch("/__roll/notes", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ name: name.value.trim(), path: location.pathname, selector: picked.selector, viewport: `${innerWidth}x${innerHeight}`, note: note.value.trim(), screen: shot, ...ctx }),
      })
        .then(async (r) => {
          if (!r.ok) throw new Error((await r.text()).slice(0, 200) || `${r.status}`);
          reset();
          load();
        })
        .catch((err) => { said.hidden = false; said.textContent = `Not sent: ${err.message}`; })
        .finally(() => { go.disabled = false; });
      return;
    }
    // The path only: a query can hold a token (a sign-in callback, a reset link), and the note
    // may end up on a public pull request.
    const q = new URLSearchParams({
      fb_path: location.pathname,
      fb_selector: picked.selector,
      fb_box: picked.box,
      fb_viewport: `${innerWidth}x${innerHeight}`,
      fb_note: note.value.trim(),
      fb_errors: JSON.stringify(errors),
      fb_steps: JSON.stringify(steps.map(([t, what]) => `${Math.max(0, Math.round((Date.now() - t) / 1000))}s before · ${what}`)),
      fb_requests: JSON.stringify([...failed.values()].slice(-5)),
      fb_ua: navigator.userAgent.slice(0, 300),
    });
    if (shot) q.set("fb_screen", shot);
    // In a small window of roll's that posts it and closes (its form alone), the page's own list
    // read again when this one has the focus back; the whole dashboard where a popup can't open.
    // `noopener` either way: the page gets no handle on roll's window.
    openRoll(q);
    reset();
  });
  // roll's small window for a note, a reply or a resolve (its form or thread alone), the page's
  // own list read again when this one has the focus back; the whole dashboard where a popup can't
  // open (a phone's width). `noopener` either way: the page gets no handle on roll's window.
  function openRoll(q) {
    const NOTE = TARGET.replace(/\/dashboard\/projects\/[^/?]+\/previews\?/, "/dashboard/previews/note?");
    const popup = matchMedia("(min-width: 640px)").matches && NOTE !== TARGET;
    window.open(`${popup ? NOTE : TARGET}&${q}`, "_blank", popup ? "noopener,popup,width=480,height=640" : "noopener");
    if (popup) addEventListener("focus", load, { once: true });
  }
  // What's been said about this page already (who, what, on which element), as text only.
  const ago = (at) => {
    const m = Math.round((Date.now() - at) / 60000);
    return m < 60 ? `${Math.max(m, 1)}m ago` : m < 2880 ? `${Math.round(m / 60)}h ago` : `${Math.round(m / 1440)}d ago`;
  };
  const show = (notes) => {
    if (!notes.length) return;
    count.textContent = String(notes.length);
    count.title = `${notes.length} ${notes.length === 1 ? "note" : "notes"} on this page`;
    count.hidden = false;
    list.replaceChildren(
      ...notes.slice().reverse().map((n) => {
        const item = document.createElement("div");
        item.className = "note";
        item.setAttribute("role", "listitem");
        const meta = document.createElement("div");
        meta.className = "muted";
        meta.textContent = `${n.by} · ${ago(n.at)}${n.selector ? ` · ${n.selector}` : ""}`;
        const text = document.createElement("p");
        text.textContent = n.note;
        item.append(meta, text);
        // Its replies, oldest first, as text only.
        for (const r of Array.isArray(n.replies) ? n.replies : []) {
          const reply = document.createElement("div");
          reply.className = "reply";
          const who = document.createElement("div");
          who.className = "muted";
          who.textContent = `${r.by} · ${ago(r.at)}`;
          const said = document.createElement("p");
          said.textContent = r.note;
          reply.append(who, said);
          item.append(reply);
        }
        // Replied to and resolved in roll, by someone who may (a writer, or whoever left it; the
        // page holds no credential): resolved, it folds away here.
        const act = (label, key) => {
          const b = document.createElement("button");
          b.type = "button";
          b.className = "resolve";
          b.textContent = label;
          b.addEventListener("click", () => openRoll(new URLSearchParams({ [key]: String(n.id || "") })));
          return b;
        };
        if (n.id && !GUEST) {
          const acts = document.createElement("div");
          acts.className = "acts";
          acts.append(act("Reply", "fb_reply"), act("Resolve", "fb_resolve"));
          item.append(acts);
        }
        item.addEventListener("mouseenter", () => {
          let el = null;
          try { el = n.selector ? document.querySelector(n.selector) : null; } catch { el = null; }
          if (el) outline(el);
        });
        item.addEventListener("mouseleave", () => { if (!picked) box.style.display = "none"; });
        return item;
      }),
    );
  };
  count.addEventListener("click", () => { list.hidden = !list.hidden; });
  const load = () =>
    fetch(`/__roll/feedback.json?path=${encodeURIComponent(location.pathname)}`, { credentials: "same-origin" })
      .then((r) => (r.ok ? r.json() : []))
      .then((notes) => Array.isArray(notes) && show(notes))
      .catch(() => {});
  load();
  const mount = () => document.body && !host.isConnected && document.body.appendChild(host);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", mount);
  else mount();
})();
