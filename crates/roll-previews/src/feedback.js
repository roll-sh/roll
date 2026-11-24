// roll previews' feedback button (docs/previews.md, "Feedback on the preview page"). The preview's
// proxy adds it to the pages of people who came in through roll. It holds no credential: it picks
// an element, takes a note, and opens roll, where the signed-in person posts it on the change.
(() => {
  if (window.__rollFeedback) return;
  window.__rollFeedback = true;
  const TARGET = __ROLL_TARGET__;
  // The project's flags a reviewer sets for their own view (Vercel's Flags Explorer): kept in a
  // cookie of this browser's, which roll's proxy turns into the services' `x-roll-flags`.
  const FLAGS = __ROLL_FLAGS__;
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
    const kind = t.type === "checkbox" || t.type === "radio" ? (t.checked ? "check" : "uncheck") : t.localName === "select" ? "choose in" : `type ${String(t.value || "").length} characters in`;
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
textarea{width:100%;min-height:72px;resize:vertical;padding:8px;border-radius:8px;border:1px solid rgba(255,255,255,.14);
 background:#000;color:#ededed}
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
.box{position:fixed;pointer-events:none;border:2px solid #4c8dff;border-radius:4px;background:rgba(76,141,255,.08);display:none}
@media (prefers-reduced-motion:no-preference){.box{transition:all .12s ease-out}}
</style>
<div class="box"></div>
<div class="list" hidden role="list" aria-label="Feedback on this page"></div>
<form class="list flags" hidden aria-label="Flags for your view"></form>
<div class="bar"><button class="count flagbtn" type="button" hidden aria-label="Flags for your view">Flags</button><button class="count" type="button" hidden aria-label="Feedback on this page"></button><button class="btn" type="button" aria-label="Leave feedback on this page"><span class="dot"></span>Feedback</button></div>
<form class="card" hidden>
  <div class="muted">On <code class="where"></code></div>
  <textarea placeholder="What should change here?" aria-label="Your feedback" required maxlength="4000"></textarea>
  <div class="row"><button type="button" class="cancel">Cancel</button><button type="submit" class="go">Post in roll</button></div>
</form>`;
  const $ = (s) => root.querySelector(s);
  const btn = $(".btn"), form = $("form.card"), box = $(".box"), note = $("textarea"), where = $(".where"), count = $(".count:not(.flagbtn)"), list = $("div.list");
  let picking = false, picked = null;
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
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    if (!picked || !note.value.trim()) return;
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
    window.open(`${TARGET}&${q}`, "_blank", "noopener");
    reset();
  });
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
          b.addEventListener("click", () => window.open(`${TARGET}&${new URLSearchParams({ [key]: String(n.id || "") })}`, "_blank", "noopener"));
          return b;
        };
        if (n.id) {
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
  fetch(`/__roll/feedback.json?path=${encodeURIComponent(location.pathname)}`, { credentials: "same-origin" })
    .then((r) => (r.ok ? r.json() : []))
    .then((notes) => Array.isArray(notes) && show(notes))
    .catch(() => {});
  const mount = () => document.body && !host.isConnected && document.body.appendChild(host);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", mount);
  else mount();
})();
