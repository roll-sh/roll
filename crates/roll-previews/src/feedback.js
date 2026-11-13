// roll previews' feedback button (docs/previews.md, "Feedback on the preview page"). The preview's
// proxy adds it to the pages of people who came in through roll. It holds no credential: it picks
// an element, takes a note, and opens roll, where the signed-in person posts it on the change.
(() => {
  if (window.__rollFeedback) return;
  window.__rollFeedback = true;
  const TARGET = __ROLL_TARGET__;
  const host = document.createElement("div");
  host.setAttribute("data-roll-feedback", "");
  host.style.cssText = "all:initial;position:fixed;right:16px;bottom:16px;z-index:2147483647";
  const root = host.attachShadow({ mode: "closed" });
  root.innerHTML = `<style>
:host{all:initial}
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
.row button{height:28px;padding:0 10px;border-radius:7px;border:1px solid rgba(255,255,255,.14);background:transparent;color:#ededed;cursor:pointer}
.row button.go{background:#4c8dff;border-color:#4c8dff;color:#fff}
.box{position:fixed;pointer-events:none;border:2px solid #4c8dff;border-radius:4px;background:rgba(76,141,255,.08);display:none}
@media (prefers-reduced-motion:no-preference){.box{transition:all .12s ease-out}}
</style>
<div class="box"></div>
<button class="btn" type="button" aria-label="Leave feedback on this page"><span class="dot"></span>Feedback</button>
<form class="card" hidden>
  <div class="muted">On <code class="where"></code></div>
  <textarea placeholder="What should change here?" aria-label="Your feedback" required maxlength="4000"></textarea>
  <div class="row"><button type="button" class="cancel">Cancel</button><button type="submit" class="go">Post in roll</button></div>
</form>`;
  const $ = (s) => root.querySelector(s);
  const btn = $(".btn"), form = $("form"), box = $(".box"), note = $("textarea"), where = $(".where");
  let picking = false, picked = null;

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
    const q = new URLSearchParams({
      fb_path: location.pathname + location.search,
      fb_selector: picked.selector,
      fb_box: picked.box,
      fb_viewport: `${innerWidth}x${innerHeight}`,
      fb_note: note.value.trim(),
    });
    window.open(`${TARGET}&${q}`, "_blank", "noopener");
    reset();
  });
  const mount = () => document.body && !host.isConnected && document.body.appendChild(host);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", mount);
  else mount();
})();
