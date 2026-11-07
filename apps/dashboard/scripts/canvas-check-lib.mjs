// The canvas checker's geometry and checks, shared by `canvas-check.mjs` (a real browser) and
// `canvas-replay.mjs` (the planner replayed in node). A frame is what the canvas showed: its size,
// the chrome over it, the cards as drawn (`dom`) and its `__canvasCheck` snapshot (`snap`).
// ── geometry ──
export const inflate = (r, d) => ({ x: r.x - d, y: r.y - d, w: r.w + 2 * d, h: r.h + 2 * d });
export const meets = (a, b, eps = 0) => a.x < b.x + b.w - eps && b.x < a.x + a.w - eps && a.y < b.y + b.h - eps && b.y < a.y + a.h - eps;
export const inside = (a, b, eps = 0.5) => a.x >= b.x - eps && a.y >= b.y - eps && a.x + a.w <= b.x + b.w + eps && a.y + a.h <= b.y + b.h + eps;
export const area = (r) => Math.max(0, r.w) * Math.max(0, r.h);
export const clip = (a, b) => {
  const x = Math.max(a.x, b.x);
  const y = Math.max(a.y, b.y);
  return { x, y, w: Math.min(a.x + a.w, b.x + b.w) - x, h: Math.min(a.y + a.h, b.y + b.h) - y };
};
export const union = (rs) => {
  if (!rs.length) return null;
  let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
  for (const r of rs) (x0 = Math.min(x0, r.x)), (y0 = Math.min(y0, r.y)), (x1 = Math.max(x1, r.x + r.w)), (y1 = Math.max(y1, r.y + r.h));
  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
};
export const fmt = (r) => `${Math.round(r.x)},${Math.round(r.y)} ${Math.round(r.w)}×${Math.round(r.h)}`;

/** The largest empty rectangle in the content's box, the cards (grown by `grow`) as filled (screen px). */
export function deadArea(rects, box, grow) {
  const cell = Math.max(6, Math.min(box.w, box.h) / 80);
  const cols = Math.max(1, Math.ceil(box.w / cell));
  const rows = Math.max(1, Math.ceil(box.h / cell));
  const full = new Uint8Array(cols * rows);
  for (const r0 of rects) {
    const r = inflate(r0, grow);
    const c0 = Math.max(0, Math.floor((r.x - box.x) / cell));
    const c1 = Math.min(cols, Math.ceil((r.x + r.w - box.x) / cell));
    const q0 = Math.max(0, Math.floor((r.y - box.y) / cell));
    const q1 = Math.min(rows, Math.ceil((r.y + r.h - box.y) / cell));
    for (let q = q0; q < q1; q++) full.fill(1, q * cols + c0, q * cols + c1);
  }
  // Largest rectangle of zeros: histogram per row, a stack per histogram.
  const hist = new Int32Array(cols);
  let best = { w: 0, h: 0, x: 0, y: 0, a: 0 };
  for (let q = 0; q < rows; q++) {
    for (let c = 0; c < cols; c++) hist[c] = full[q * cols + c] ? 0 : hist[c] + 1;
    const st = [];
    for (let c = 0; c <= cols; c++) {
      const hgt = c < cols ? hist[c] : 0;
      let start = c;
      while (st.length && st[st.length - 1][1] >= hgt) {
        const [s, hh] = st.pop();
        const a = hh * (c - s);
        if (a > best.a) best = { a, w: c - s, h: hh, x: s, y: q - hh + 1 };
        start = s;
      }
      st.push([start, hgt]);
    }
  }
  return { x: box.x + best.x * cell, y: box.y + best.y * cell, w: best.w * cell, h: best.h * cell };
}

// ── checks on one frame ──
export function checkFrame(f, label, { fit }) {
  const fails = [];
  const s = f.snap;
  const v = s.goal ?? s.view;
  const k = v.k;
  const toScreen = (n) => ({ x: n.x * k + v.x, y: n.y * k + v.y, w: n.w * k, h: n.h * k });
  const nodes = s.nodes.filter((n) => n.x !== null && n.w > 0);
  const L = new Map(nodes.map((n) => [n.key, toScreen(n)]));
  const D = f.dom;
  const chrome = [...f.prompt.map((r) => ["prompt bar", r]), ...(f.zoom ? [["zoom bar", f.zoom]] : []), ...(f.minimap ? [["minimap", f.minimap]] : [])];
  // The free area: the canvas above the prompt bar and the zoom bar (a phone's zoom bar at the top: below it).
  const barTop = Math.min(f.canvas.h, ...f.prompt.map((r) => r.y), ...(f.zoom && !f.waiting ? [f.zoom.y] : []));
  const top = f.waiting && f.zoom ? f.zoom.y + f.zoom.h : 0;
  const free = { x: 0, y: top, w: f.canvas.w, h: barTop - top };
  const all = !!s.plan?.all;
  const CLEAR = 16;
  const rectOf = (key) => D[key] ?? L.get(key);
  const say = (code, msg) => fails.push(`${code} ${label}: ${msg}`);
  const metrics = { k: +k.toFixed(3), all, cards: nodes.length, room: s.room && [s.room.w, Math.round(s.room.h), Math.round(s.room.pad)], plan: s.plan && [+s.plan.k.toFixed(3), s.plan.all], overview: s.overview, screen: s.input.screen, covered: Math.round(s.covered) };

  // a: clear of the chrome and the edges. Not everything fitting, cards may run off the right and
  // the bottom (the rest of the canvas), never the left or the top, and never sit under the chrome.
  {
    const bad = [];
    for (const n of nodes) {
      const r = rectOf(n.key);
      if (!meets(r, f.canvas)) continue;
      const runsOff = !all && (r.x + r.w > f.canvas.w || r.y + r.h > free.y + free.h);
      for (const [name, c] of chrome) if (meets(r, inflate(c, CLEAR)) && !runsOff) bad.push(`${n.key} under ${name}`);
      if (fit && all && !inside(r, inflate(free, -CLEAR))) bad.push(`${n.key} within ${CLEAR}px of an edge (${fmt(r)})`);
      if (fit && !all && (r.x < CLEAR - 0.5 || r.y < free.y + CLEAR - 0.5)) bad.push(`${n.key} cut at the left/top (${fmt(r)})`);
    }
    if (bad.length) say("a", `${bad.length} cards: ${bad.slice(0, 3).join("; ")}`);
  }
  // b: no overlaps (the layout's own rects).
  {
    const rs = nodes.map((n) => [n.key, { x: n.x, y: n.y, w: n.w, h: n.h }]).sort((p, q) => p[1].x - q[1].x);
    const bad = [];
    for (let i = 0; i < rs.length && bad.length < 4; i++)
      for (let j = i + 1; j < rs.length && rs[j][1].x < rs[i][1].x + rs[i][1].w; j++) if (meets(rs[i][1], rs[j][1], 0.5)) bad.push(`${rs[i][0]} × ${rs[j][0]}`);
    if (bad.length) say("b", bad.join("; "));
  }
  // c: what matters wholly in view after Fit.
  if (fit) {
    const view = clip(f.canvas, free);
    const bad = nodes.filter((n) => n.important).filter((n) => {
      const r = rectOf(n.key);
      return !inside(r, view) || chrome.some(([, c]) => meets(r, c));
    });
    // More of it than fits at 55 % (where titles still read) even packed edge to edge: not required.
    const need = nodes.filter((n) => n.important).reduce((a, n) => a + (n.w + 28) * (n.h + 24), 0) * 0.55 * 0.55;
    if (bad.length && need > area(view)) metrics.cWaived = `${bad.length} of ${nodes.filter((n) => n.important).length} important out of view (they can't all read at once)`;
    else if (bad.length) say("c", `${bad.length}/${nodes.filter((n) => n.important).length} important out of view: ${bad.slice(0, 3).map((n) => n.key).join(", ")}`);
  }
  // d: a grid one column wide with room for two.
  const kids = new Map();
  for (const n of nodes) if (n.parent && s.input.levels[n.parent] === "items") (kids.get(n.parent) ?? kids.set(n.parent, []).get(n.parent)).push(n);
  if (fit) {
    const reserve = f.minimap ? f.minimap.w + 12 : 0;
    for (const [p, ks] of kids) {
      if (ks.length < 2) continue;
      const cols = new Set(ks.map((n) => Math.round(n.x))).size;
      if (cols > 1) continue;
      const left = Math.min(...ks.map((n) => L.get(n.key).x));
      const cw = Math.max(...ks.map((n) => n.w));
      const need = (2 * cw + 28) * k;
      if (k < 0.995 && free.w - reserve - CLEAR - left >= need) say("d", `${p}: ${ks.length} cards in one column, ${Math.round(free.w - reserve - left)}px free for ${Math.round(need)}`);
    }
  }
  // l: not everything shown though all of it would read a few points smaller (≥ 65 %).
  if (fit && !all && s.room) {
    const w = union(nodes.map((n) => ({ x: n.x, y: n.y, w: n.w, h: n.h })));
    const z = Math.min(1, (s.room.w - 2 * s.room.pad) / w.w, (s.room.h - 2 * s.room.pad) / w.h);
    if (z >= 0.65) say("l", `cut at ${Math.round(k * 100)} % though everything fits at ${Math.round(z * 100)} %`);
  }
  // e, f: coverage and dead areas.
  const content = union(nodes.map((n) => L.get(n.key)));
  if (content && free.w > 0 && free.h > 0) {
    const cover = area(clip(content, free)) / area(free);
    metrics.cover = +cover.toFixed(2);
    if (fit && all && k < 0.995 && cover < 0.6) say("e", `content covers ${Math.round(cover * 100)} % of the free area (box ${fmt(content)}, free ${fmt(free)}, zoom ${Math.round(k * 100)} %)`);
    if (fit) {
      // Seen part of the content only: off screen isn't dead, it's the rest.
      const seen = clip(content, free);
      if (seen.w > 0 && seen.h > 0) {
        const dead = deadArea(nodes.map((n) => L.get(n.key)).filter((r) => meets(r, seen)), seen, 36 * k);
        metrics.dead = +(area(dead) / area(seen)).toFixed(2);
        if (area(dead) >= 0.15 * area(seen) && dead.w >= 0.3 * seen.w && dead.h >= 0.3 * seen.h) say("f", `empty ${fmt(dead)} in the content's ${fmt(seen)} (${Math.round(metrics.dead * 100)} %)`);
      }
    }
  }
  // g: rows aligned as drawn; grids side by side on one top line.
  {
    const bad = [];
    for (const [p, ks] of kids) {
      const rows = new Map();
      for (const n of ks) (rows.get(Math.round(n.y)) ?? rows.set(Math.round(n.y), []).get(Math.round(n.y))).push(n);
      for (const [, row] of rows) {
        if (row.length < 2) continue;
        const tops = row.map((n) => D[n.key]?.y).filter((y) => y !== undefined);
        if (tops.length > 1 && Math.max(...tops) - Math.min(...tops) > 1) bad.push(`${p} row off by ${(Math.max(...tops) - Math.min(...tops)).toFixed(1)}px`);
      }
    }
    // Grids of one parent's lanes that sit side by side.
    const byGrand = new Map();
    for (const [p, ks] of kids) {
      const gp = s.nodes.find((n) => n.key === p)?.parent;
      if (!gp) continue;
      const r = union(ks.map((n) => ({ x: n.x, y: n.y, w: n.w, h: n.h })));
      (byGrand.get(gp) ?? byGrand.set(gp, []).get(gp)).push([p, r]);
    }
    for (const [gp, gs] of byGrand)
      for (let i = 0; i < gs.length; i++)
        for (let j = i + 1; j < gs.length; j++) {
          const [a, b] = [gs[i][1], gs[j][1]];
          const side = a.x + a.w <= b.x || b.x + b.w <= a.x;
          const level = a.y < b.y + b.h && b.y < a.y + a.h;
          if (side && level && Math.abs(a.y - b.y) > 1) bad.push(`${gs[i][0]} and ${gs[j][0]} under ${gp} tops differ by ${Math.round(Math.abs(a.y - b.y))}`);
        }
    if (bad.length) say("g", bad.slice(0, 3).join("; "));
  }
  // k: drawn where laid out (catches a layout the engine never settled into, WebKit's staircase).
  {
    const bad = [];
    for (const n of nodes) {
      const d = D[n.key];
      const l = L.get(n.key);
      if (!d || !meets(d, f.canvas)) continue;
      if (Math.abs(d.x - l.x) > 1.5 || Math.abs(d.y - l.y) > 1.5) bad.push(`${n.key} drawn ${fmt(d)} laid out ${fmt(l)}`);
    }
    if (bad.length) say("k", `${bad.length} cards: ${bad.slice(0, 2).join("; ")}`);
  }
  return { fails, metrics };
}

export const positionsOf = (f) => new Map(f.snap.nodes.filter((n) => n.x !== null).map((n) => [n.key, [n.x, n.y]]));
export function moved(a, b) {
  const pa = positionsOf(a);
  const pb = positionsOf(b);
  const out = [];
  for (const [key, [x, y]] of pa) {
    const q = pb.get(key);
    if (q && (Math.abs(q[0] - x) > 0.5 || Math.abs(q[1] - y) > 0.5)) out.push(key);
  }
  return out;
}

