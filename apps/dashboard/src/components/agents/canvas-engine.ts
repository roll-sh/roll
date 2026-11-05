/**
 * The agents canvas engine: one canvas 2D + `requestAnimationFrame` loop that draws the dot field,
 * the edges and the pulses along busy edges, and moves the DOM cards with the same world transform. Framework-agnostic like `RollMarkEngine`
 * (packages/brand/src/engine.ts), and it follows the brand engines' rules: colours resolved
 * through `resolveColor` (call `refreshColors` when the theme changes), paused offscreen, a
 * static but state-correct frame under reduced motion, ease-out motion, no flashing.
 *
 * Pointer: drag the background (or anything with space held, or the middle button) to pan. Like
 * Figma: scrolling pans (two fingers on a trackpad, any direction; shift turns a mouse wheel
 * sideways), and pinching zooms about the fingers (ctrl+wheel in Chromium and Firefox, gesture
 * events in Safari, two touches on a touch screen); ⌘/ctrl+wheel zooms with a mouse. Wheel events
 * inside `[data-scroll]` scroll that element instead.
 */

import { resolveColor } from "@roll/brand";
import { clampZoom, zoomAt, type Point, type Rect, type Size, type View } from "@/lib/agents-layout";

type RGB = [number, number, number];

export interface SceneMeta {
  busy: boolean;
  /** Where a new node grows from (its parent's key). */
  parent: string | null;
}

/**
 * How an edge runs: `right` from a parent's right edge into its child's left (the tree growing
 * right); `down` from the middle of a parent's bottom into the middle of its child's top (children
 * side by side under it); `list` down a trunk inside the parent's left edge and into the child's
 * left (children listed under it, indented).
 */
export type EdgeRoute = "right" | "down" | "list";

export interface SceneEdge {
  from: string;
  to: string;
  busy: boolean;
  dashed: boolean;
  /** `right` when absent. */
  route?: EdgeRoute;
  /** A colour token (`--id-teal`): the project's or machine's colour it's drawn in when it isn't busy. */
  tint?: string;
}

export interface EngineOptions {
  reducedMotion?: boolean;
  /** The view changed (pan, zoom, animation). */
  onView?: (v: View) => void;
  /** Measured card sizes changed. */
  onSizes?: (sizes: Map<string, Size>) => void;
  /** A click on empty canvas. */
  onBackground?: () => void;
}

/** A loop of the accent pulse along an edge. */
const PULSE_S = 1.6;
const DOT_SPACING = 28;

const rgba = (c: RGB, a: number) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;
const mixRgb = (a: RGB, b: RGB, t: number): RGB => [0, 1, 2].map((i) => Math.round(a[i] + (b[i] - a[i]) * t)) as RGB;

function cssVar(el: Element, name: string, fallback: string) {
  return getComputedStyle(el).getPropertyValue(name).trim() || fallback;
}

export class CanvasEngine {
  private ctx: CanvasRenderingContext2D | null;
  private w = 0;
  private h = 0;
  private dpr = 1;
  private raf = 0;
  private visible = true;
  private destroyed = false;

  view: View = { x: 0, y: 0, k: 1 };
  /** The `--zoom` last written on the world. */
  private zoomShown = Number.NaN;
  private viewFrom: View | null = null;
  private viewTo: View | null = null;
  private viewT0 = 0;
  private viewMs = 280;

  private els = new Map<string, HTMLElement>();
  private cur = new Map<string, Point>();
  private tgt = new Map<string, Point>();
  /** The layout's targets, before a drag holds one card. */
  private slots: ReadonlyMap<string, Point> = new Map();
  private size = new Map<string, Size>();
  private meta = new Map<string, SceneMeta>();
  private edges: SceneEdge[] = [];
  private anyBusy = false;
  private drag: { key: string; p: Point } | null = null;

  private ink: RGB = [126, 130, 139];
  private line: RGB = [46, 48, 53];
  private accent: RGB = [76, 141, 255];
  /** The canvas itself: what a port's centre is punched out with. */
  private ground: RGB = [13, 14, 16];
  /** Identity tokens resolved once per theme (`refreshColors` clears them). */
  private tints = new Map<string, RGB>();
  /** Dots are fainter on a light canvas (dark ink on white reads stronger). */
  private dotAlpha = 0.16;

  private pointer = { x: -9999, y: -9999, s: 0, active: false };
  private ripples: { x: number; y: number; t0: number }[] = [];
  private busyMix = 0;

  private pan: { id: number; x: number; y: number; moved: number } | null = null;
  private touches = new Map<number, { x: number; y: number }>();
  private pinch: { d: number; k: number } | null = null;
  /** Safari's trackpad pinch: the scale it started at. */
  private gesture: { k: number } | null = null;
  private space = false;

  private ro: ResizeObserver;
  private cardRo: ResizeObserver;
  private io: IntersectionObserver;
  private pendingSizes = new Map<string, Size>();
  private sizesQueued = false;

  constructor(
    private container: HTMLElement,
    private canvas: HTMLCanvasElement,
    private world: HTMLElement,
    private opts: EngineOptions = {},
  ) {
    this.ctx = canvas.getContext("2d");
    this.refreshColors();
    this.ro = new ResizeObserver(() => this.resize());
    this.ro.observe(container);
    this.cardRo = new ResizeObserver((entries) => {
      for (const e of entries) {
        const key = (e.target as HTMLElement).dataset.key;
        if (!key) continue;
        const el = e.target as HTMLElement;
        const s = { w: el.offsetWidth, h: el.offsetHeight };
        // Hidden (the canvas is under a conversation): keep the last real size.
        if (s.w === 0 && s.h === 0) continue;
        const prev = this.size.get(key);
        if (prev && prev.w === s.w && prev.h === s.h) continue;
        this.size.set(key, s);
        this.pendingSizes.set(key, s);
      }
      if (this.pendingSizes.size && !this.sizesQueued) {
        this.sizesQueued = true;
        queueMicrotask(() => {
          this.sizesQueued = false;
          const out = this.pendingSizes;
          this.pendingSizes = new Map();
          this.opts.onSizes?.(out);
        });
      }
      this.kick();
    });
    this.io = new IntersectionObserver(([e]) => {
      this.visible = !!e?.isIntersecting;
      this.kick();
    });
    this.io.observe(container);
    container.addEventListener("pointerdown", this.onDown);
    container.addEventListener("pointermove", this.onMove);
    container.addEventListener("pointerup", this.onUp);
    container.addEventListener("pointercancel", this.onUp);
    container.addEventListener("pointerleave", this.onLeave);
    container.addEventListener("wheel", this.onWheel, { passive: false });
    container.addEventListener("gesturestart", this.onGesture as EventListener);
    container.addEventListener("gesturechange", this.onGesture as EventListener);
    container.addEventListener("gestureend", this.onGesture as EventListener);
    window.addEventListener("keydown", this.onKey);
    window.addEventListener("keyup", this.onKey);
    document.addEventListener("visibilitychange", this.kick);
    this.resize();
  }

  destroy() {
    this.destroyed = true;
    cancelAnimationFrame(this.raf);
    this.ro.disconnect();
    this.cardRo.disconnect();
    this.io.disconnect();
    const c = this.container;
    c.removeEventListener("pointerdown", this.onDown);
    c.removeEventListener("pointermove", this.onMove);
    c.removeEventListener("pointerup", this.onUp);
    c.removeEventListener("pointercancel", this.onUp);
    c.removeEventListener("pointerleave", this.onLeave);
    c.removeEventListener("wheel", this.onWheel);
    c.removeEventListener("gesturestart", this.onGesture as EventListener);
    c.removeEventListener("gesturechange", this.onGesture as EventListener);
    c.removeEventListener("gestureend", this.onGesture as EventListener);
    window.removeEventListener("keydown", this.onKey);
    window.removeEventListener("keyup", this.onKey);
    document.removeEventListener("visibilitychange", this.kick);
  }

  setOptions(opts: EngineOptions) {
    // `undefined` keeps the current value.
    const defined = Object.fromEntries(Object.entries(opts).filter(([, v]) => v !== undefined));
    this.opts = { ...this.opts, ...defined };
    this.kick();
  }

  private tint(token: string): RGB {
    let rgb = this.tints.get(token);
    if (!rgb) {
      rgb = resolveColor(cssVar(this.container, token, "#2e3035"));
      this.tints.set(token, rgb);
    }
    return rgb;
  }

  /** Re-read the theme's colours (call after a light/dark switch). */
  refreshColors() {
    // Opaque tokens (resolveColor drops alpha): --dot, --edge, --canvas; docs/code/agents-design.md.
    const c = this.container;
    this.ink = resolveColor(cssVar(c, "--dot", "#7e828b"));
    this.line = resolveColor(cssVar(c, "--edge", "#2e3035"));
    this.accent = resolveColor(cssVar(c, "--accent", "#4c8dff"));
    this.ground = resolveColor(cssVar(c, "--canvas", "#0d0e10"));
    this.tints.clear();
    const lum = (0.2126 * this.ground[0] + 0.7152 * this.ground[1] + 0.0722 * this.ground[2]) / 255;
    this.dotAlpha = lum > 0.5 ? 0.26 : 0.24;
    this.kick();
  }

  get viewport(): Size {
    return { w: this.w, h: this.h };
  }

  // ── cards ──

  register(key: string, el: HTMLElement) {
    if (this.els.get(key) === el) return;
    const old = this.els.get(key);
    if (old) this.cardRo.unobserve(old);
    el.dataset.key = key;
    this.els.set(key, el);
    this.cardRo.observe(el);
    const p = this.cur.get(key);
    if (p) this.place(el, p);
  }

  unregister(key: string, el: HTMLElement) {
    if (this.els.get(key) !== el) return;
    this.cardRo.unobserve(el);
    this.els.delete(key);
  }

  sizeOf(key: string): Size | undefined {
    return this.size.get(key);
  }

  /** Where a node is drawn right now (world). */
  positionOf(key: string): Point | undefined {
    return this.cur.get(key);
  }

  /** The layout's targets; nodes ease there. New nodes grow out of their parent. */
  setScene(targets: ReadonlyMap<string, Point>, meta: ReadonlyMap<string, SceneMeta>, edges: SceneEdge[], sizes: ReadonlyMap<string, Size>) {
    for (const [k, s] of sizes) if (!this.size.has(k)) this.size.set(k, s);
    for (const k of [...this.cur.keys()]) if (!targets.has(k)) this.cur.delete(k);
    for (const [k, p] of targets) {
      if (!this.cur.has(k)) {
        const parent = meta.get(k)?.parent;
        const from = (parent && this.cur.get(parent)) || p;
        this.cur.set(k, this.opts.reducedMotion ? p : { ...from });
      }
    }
    this.slots = targets;
    this.tgt = new Map(targets);
    // The dragged card stays under the pointer while the others make room.
    if (this.drag) this.tgt.set(this.drag.key, this.drag.p);
    this.meta = new Map(meta);
    this.edges = edges;
    this.anyBusy = [...meta.values()].some((m) => m.busy);
    this.kick();
  }

  /** Hold a node under the pointer while it's dragged (`null` lets it go back to its target). */
  setDrag(key: string, p: Point | null) {
    this.drag = p ? { key, p } : null;
    // Above its siblings while it's carried.
    const el = this.els.get(key);
    if (el) el.style.zIndex = p ? "10" : "";
    if (p) {
      this.cur.set(key, p);
      this.tgt.set(key, p);
    } else {
      // Let go: it settles into its place in the layout.
      const slot = this.slots.get(key);
      if (slot) this.tgt.set(key, slot);
    }
    this.kick();
  }

  // ── view ──

  setView(v: View, animate = false) {
    const next = { x: v.x, y: v.y, k: clampZoom(v.k) };
    if (!animate || this.opts.reducedMotion) {
      this.viewFrom = this.viewTo = null;
      this.view = next;
      this.opts.onView?.(next);
    } else {
      this.viewFrom = { ...this.view };
      this.viewTo = next;
      this.viewT0 = performance.now();
    }
    this.kick();
  }

  zoomBy(f: number, sx = this.w / 2, sy = this.h / 2, animate = true) {
    this.setView(zoomAt(this.viewTo ?? this.view, (this.viewTo ?? this.view).k * f, sx, sy), animate);
  }

  panBy(dx: number, dy: number, animate = false) {
    const v = this.viewTo ?? this.view;
    this.setView({ ...v, x: v.x + dx, y: v.y + dy }, animate);
  }

  // ── pointer ──

  private local(e: { clientX: number; clientY: number }) {
    const r = this.container.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  }

  private onKey = (e: KeyboardEvent) => {
    if (e.code !== "Space") return;
    const el = e.target as HTMLElement | null;
    if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT|BUTTON)$/.test(el.tagName))) return;
    const down = e.type === "keydown";
    if (down && this.container.matches(":hover")) e.preventDefault();
    this.space = down;
    this.container.style.cursor = down ? "grab" : "";
  };

  private onDown = (e: PointerEvent) => {
    const p = this.local(e);
    if (e.pointerType === "touch") {
      this.touches.set(e.pointerId, p);
      if (this.touches.size === 2) {
        const [a, b] = [...this.touches.values()];
        this.pinch = { d: Math.hypot(a.x - b.x, a.y - b.y), k: this.view.k };
        this.pan = null;
        return;
      }
    }
    const target = e.target as HTMLElement;
    // Controls over the canvas (the toolbar, zoom, dialogs) keep their clicks: capturing the
    // pointer for a pan would send the click to the canvas instead.
    if (e.button !== 1 && target.closest("button, a, input, textarea, select, label, [role=dialog]:not([data-surface]), [role=menu], [data-nodrag]")) return;
    const onCard = !!target.closest("[data-card]");
    if (!(e.button === 1 || ((e.button === 0 && !onCard) || (this.space && e.button === 0)))) return;
    if (e.button === 1 || this.space) e.preventDefault();
    this.pan = { id: e.pointerId, x: p.x, y: p.y, moved: 0 };
    this.container.setPointerCapture(e.pointerId);
    this.container.style.cursor = "grabbing";
  };

  private onMove = (e: PointerEvent) => {
    const p = this.local(e);
    this.pointer.x = p.x;
    this.pointer.y = p.y;
    this.pointer.active = true;
    if (this.pinch && this.touches.has(e.pointerId)) {
      this.touches.set(e.pointerId, p);
      const [a, b] = [...this.touches.values()];
      const d = Math.hypot(a.x - b.x, a.y - b.y);
      this.setView(zoomAt(this.view, (this.pinch.k * d) / Math.max(1, this.pinch.d), (a.x + b.x) / 2, (a.y + b.y) / 2));
      return;
    }
    if (this.pan && this.pan.id === e.pointerId) {
      const dx = p.x - this.pan.x;
      const dy = p.y - this.pan.y;
      this.pan.x = p.x;
      this.pan.y = p.y;
      this.pan.moved += Math.abs(dx) + Math.abs(dy);
      this.panBy(dx, dy);
      return;
    }
    this.kick();
  };

  private onUp = (e: PointerEvent) => {
    this.touches.delete(e.pointerId);
    if (this.touches.size < 2) this.pinch = null;
    if (!this.pan || this.pan.id !== e.pointerId) return;
    const click = this.pan.moved < 4;
    this.pan = null;
    this.container.style.cursor = this.space ? "grab" : "";
    if (this.container.hasPointerCapture(e.pointerId)) this.container.releasePointerCapture(e.pointerId);
    if (click && !(e.target as HTMLElement).closest("[data-card]")) {
      const p = this.local(e);
      if (!this.opts.reducedMotion) this.ripples.push({ x: p.x, y: p.y, t0: performance.now() / 1000 });
      this.opts.onBackground?.();
      this.kick();
    }
  };

  private onLeave = () => {
    this.pointer.active = false;
    this.kick();
  };

  private onWheel = (e: WheelEvent) => {
    const target = e.target as HTMLElement;
    if (!e.ctrlKey && !e.metaKey && target.closest("[data-scroll]")) return;
    e.preventDefault();
    const p = this.local(e);
    const lines = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? this.h : 1;
    let dx = e.deltaX * lines;
    let dy = e.deltaY * lines;
    if (e.ctrlKey || e.metaKey) {
      // A pinch sends small, smooth deltas; a ⌘-scrolled mouse wheel whole notches.
      const f = Math.exp(-Math.max(-60, Math.min(60, dy)) * (Math.abs(dy) < 50 ? 0.012 : 0.0025));
      this.setView(zoomAt(this.view, this.view.k * f, p.x, p.y));
      return;
    }
    // Windows and Linux send shift+wheel as vertical; macOS already turns it sideways.
    if (e.shiftKey && dx === 0) [dx, dy] = [dy, 0];
    this.panBy(-dx, -dy);
  };

  /** Safari (macOS) pinches as gesture events instead of ctrl+wheel. */
  private onGesture = (e: Event & { scale?: number; clientX?: number; clientY?: number }) => {
    e.preventDefault();
    if (e.type === "gestureend") {
      this.gesture = null;
      return;
    }
    if (e.type === "gesturestart" || !this.gesture) this.gesture = { k: this.view.k };
    const p = this.local({ clientX: e.clientX ?? this.w / 2, clientY: e.clientY ?? this.h / 2 });
    this.setView(zoomAt(this.view, this.gesture.k * (e.scale ?? 1), p.x, p.y));
  };

  // ── frames ──

  private resize() {
    const r = this.container.getBoundingClientRect();
    this.dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.w = r.width;
    this.h = r.height;
    this.canvas.width = Math.round(this.w * this.dpr);
    this.canvas.height = Math.round(this.h * this.dpr);
    this.canvas.style.width = `${this.w}px`;
    this.canvas.style.height = `${this.h}px`;
    this.kick();
  }

  kick = () => {
    if (this.destroyed || this.raf || !this.visible || document.hidden) return;
    this.raf = requestAnimationFrame(this.frame);
  };

  private place(el: HTMLElement, p: Point) {
    el.style.transform = `translate3d(${p.x}px, ${p.y}px, 0)`;
  }

  private frame = (ms: number) => {
    this.raf = 0;
    const reduced = !!this.opts.reducedMotion;
    const t = ms / 1000;
    let moving = false;

    // View animation (ease-out cubic).
    if (this.viewTo && this.viewFrom) {
      const u = Math.min(1, (ms - this.viewT0) / this.viewMs);
      const e = 1 - (1 - u) ** 3;
      const a = this.viewFrom;
      const b = this.viewTo;
      this.view = { x: a.x + (b.x - a.x) * e, y: a.y + (b.y - a.y) * e, k: a.k + (b.k - a.k) * e };
      if (u >= 1) this.viewFrom = this.viewTo = null;
      else moving = true;
      this.opts.onView?.(this.view);
    }
    this.world.style.transform = `translate3d(${this.view.x}px, ${this.view.y}px, 0) scale(${this.view.k})`;
    // The scale for CSS too (`--zoom`): a touch screen's card tools keep a finger's size with it.
    if (this.view.k !== this.zoomShown) {
      this.zoomShown = this.view.k;
      this.world.style.setProperty("--zoom", String(this.view.k));
    }

    // Cards ease to their targets.
    for (const [k, target] of this.tgt) {
      const c = this.cur.get(k) ?? target;
      let next = target;
      if (!reduced && (!this.drag || this.drag.key !== k)) {
        const dx = target.x - c.x;
        const dy = target.y - c.y;
        if (Math.abs(dx) > 0.3 || Math.abs(dy) > 0.3) {
          next = { x: c.x + dx * 0.22, y: c.y + dy * 0.22 };
          moving = true;
        }
      }
      this.cur.set(k, next);
      const el = this.els.get(k);
      if (el) this.place(el, next);
    }

    this.pointer.s += ((this.pointer.active && !reduced ? 1 : 0) - this.pointer.s) * 0.1;
    this.busyMix += ((this.anyBusy ? 1 : 0) - this.busyMix) * (reduced ? 1 : 0.04);
    this.ripples = this.ripples.filter((r) => t - r.t0 < 2.2);

    this.draw(reduced ? 0 : t, reduced);

    const live = !reduced && (this.anyBusy || this.pointer.s > 0.01 || this.ripples.length > 0 || this.busyMix > 0.01);
    if (moving || live) this.kick();
  };

  private draw(t: number, reduced: boolean) {
    const ctx = this.ctx;
    if (!ctx) return;
    const { x: vx, y: vy, k } = this.view;
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    ctx.clearRect(0, 0, this.w, this.h);

    // Dot field in world space, coarser as you zoom out.
    let step = DOT_SPACING;
    while (step * k < 14) step *= 2;
    const s = step * k;
    const x0 = (((vx % s) + s) % s) - s;
    const y0 = (((vy % s) + s) % s) - s;
    const r2 = 140 * 140;
    const ps = this.pointer;
    for (let y = y0; y <= this.h + s; y += s) {
      for (let x = x0; x <= this.w + s; x += s) {
        let boost = 0;
        let lit = 0;
        let px = x;
        let py = y;
        if (ps.s > 0.01) {
          const dx = x - ps.x;
          const dy = y - ps.y;
          const d2 = dx * dx + dy * dy;
          if (d2 < r2 * 4) {
            const f = Math.exp(-d2 / (r2 * 0.5)) * ps.s;
            boost += f;
            lit = Math.max(lit, f * 0.4);
            const d = Math.sqrt(d2) + 1;
            px += (dx / d) * f * 5;
            py += (dy / d) * f * 5;
          }
        }
        for (const r of this.ripples) {
          const age = t - r.t0;
          const d = Math.hypot(x - r.x, y - r.y);
          const rw = Math.exp(-((d - age * 520) ** 2) / 1800) * Math.exp(-age * 1.4);
          boost += rw;
          lit = Math.max(lit, rw * 0.7);
        }
        if (this.busyMix > 0.01 && !reduced) {
          // The DotField "thinking" sweep, quieter: work is happening somewhere on the canvas.
          const wave = Math.max(0, Math.cos((x + y * 0.6) / 220 - t * 1.6)) ** 12 * this.busyMix;
          boost += wave * 0.5;
          lit = Math.max(lit, wave * 0.35);
        }
        const c = lit > 0.01 ? mixRgb(this.ink, this.accent, Math.min(1, lit)) : this.ink;
        ctx.fillStyle = rgba(c, Math.min(0.9, this.dotAlpha + boost * 0.5));
        ctx.beginPath();
        ctx.arc(px, py, 0.9 + boost * 1.2, 0, Math.PI * 2);
        ctx.fill();
      }
    }

    const scr = (p: Point) => ({ x: p.x * k + vx, y: p.y * k + vy });
    const phase = (t / PULSE_S) % 1;

    // Edges, routed as the layout placed the child (`EdgeRoute`).
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    for (const e of this.edges) {
      const a = this.cur.get(e.from);
      const b = this.cur.get(e.to);
      const sa = this.size.get(e.from);
      const sb = this.size.get(e.to);
      if (!a || !b || !sa || !sb) continue;
      const path = edgePath(e.route ?? "right", { ...a, ...sa }, { ...b, ...sb }, scr, k);
      const p0 = path.start;
      const p3 = path.end;
      // Busy is the accent; otherwise the edge carries its project's (or machine's) colour, quietly.
      const stroke = e.busy ? rgba(this.accent, 0.5) : e.tint ? rgba(this.tint(e.tint), 0.42) : rgba(this.line, 1);
      ctx.setLineDash(e.dashed ? [3, 4] : []);
      ctx.lineWidth = 1.25;
      ctx.strokeStyle = stroke;
      ctx.beginPath();
      path.trace(ctx);
      ctx.stroke();
      ctx.setLineDash([]);
      // Ports: a small ring where the edge leaves its parent and meets its child.
      const pr = Math.max(1.6, 2.75 * Math.min(1, k));
      for (const q of [p0, p3]) {
        ctx.beginPath();
        ctx.arc(q.x, q.y, pr, 0, Math.PI * 2);
        ctx.fillStyle = rgba(this.ground, 1);
        ctx.fill();
        ctx.lineWidth = 1.25;
        ctx.strokeStyle = stroke;
        ctx.stroke();
      }
      if (e.busy && !reduced) {
        // A dot travelling down the edge with a short tail.
        for (let i = 0; i < 4; i++) {
          const u = phase - i * 0.035;
          if (u < 0) continue;
          const q = path.at(easeInOut(u));
          ctx.fillStyle = rgba(this.accent, (1 - i / 4) * 0.9);
          ctx.beginPath();
          ctx.arc(q.x, q.y, Math.max(1.2, 2.4 * Math.min(1, k)) * (1 - i * 0.18), 0, Math.PI * 2);
          ctx.fill();
        }
      }
    }
  }
}

/** An edge on screen: where it starts and ends, how to draw it, and the point `u` (0–1) along it. */
export interface EdgePath {
  start: Point;
  end: Point;
  trace: (ctx: Pick<CanvasRenderingContext2D, "moveTo" | "lineTo" | "bezierCurveTo" | "arcTo">) => void;
  at: (u: number) => Point;
}

/** An edge between two cards (world rects) on screen (`scr` maps world to screen, `k` the zoom). */
export function edgePath(route: EdgeRoute, a: Rect, b: Rect, scr: (p: Point) => Point, k: number): EdgePath {
  if (route === "list") return edgeList(scr({ x: a.x + (b.x - a.x) / 2, y: a.y + a.h }), scr({ x: b.x, y: b.y + Math.min(b.h / 2, 22) }), k);
  const [p0, p1, p2, p3] =
    route === "down"
      ? edgeDown(scr({ x: a.x + a.w / 2, y: a.y + a.h }), scr({ x: b.x + b.w / 2, y: b.y }), k)
      : edgeRight(scr({ x: a.x + a.w, y: a.y + Math.min(a.h / 2, 28) }), scr({ x: b.x, y: b.y + Math.min(b.h / 2, 28) }), k);
  return {
    start: p0,
    end: p3,
    trace: (ctx) => {
      ctx.moveTo(p0.x, p0.y);
      ctx.bezierCurveTo(p1.x, p1.y, p2.x, p2.y, p3.x, p3.y);
    },
    at: (u) => bez(p0, p1, p2, p3, u),
  };
}

/**
 * A listed child's edge (screen space): straight down the trunk from `p0` (under the parent,
 * inside its left edge), a rounded corner, then right into the child at `p3`.
 */
export function edgeList(p0: Point, p3: Point, k: number): EdgePath {
  const corner = { x: p0.x, y: p3.y };
  const down = Math.max(0, corner.y - p0.y);
  const across = Math.max(0, p3.x - corner.x);
  const r = Math.min(8 * k, down, across);
  const total = down + across || 1;
  return {
    start: p0,
    end: p3,
    trace: (ctx) => {
      ctx.moveTo(p0.x, p0.y);
      if (r > 0.5) ctx.arcTo(corner.x, corner.y, p3.x, p3.y, r);
      else ctx.lineTo(corner.x, corner.y);
      ctx.lineTo(p3.x, p3.y);
    },
    at: (u) => {
      const d = Math.min(1, Math.max(0, u)) * total;
      return d <= down ? { x: p0.x, y: p0.y + d } : { x: corner.x + (d - down), y: corner.y };
    },
  };
}

/** A bezier leaving `p0` to the right and arriving at `p3` from the left (screen space). */
export function edgeRight(p0: Point, p3: Point, k: number): [Point, Point, Point, Point] {
  const dx = Math.max(24 * k, (p3.x - p0.x) / 2);
  return [p0, { x: p0.x + dx, y: p0.y }, { x: p3.x - dx, y: p3.y }, p3];
}

/** A bezier leaving `p0` downwards and arriving at `p3` from above (screen space). */
export function edgeDown(p0: Point, p3: Point, k: number): [Point, Point, Point, Point] {
  const dy = Math.max(24 * k, (p3.y - p0.y) / 2);
  return [p0, { x: p0.x, y: p0.y + dy }, { x: p3.x, y: p3.y - dy }, p3];
}

const easeInOut = (u: number) => (u < 0.5 ? 2 * u * u : 1 - (-2 * u + 2) ** 2 / 2);

function bez(p0: Point, p1: Point, p2: Point, p3: Point, u: number): Point {
  const v = 1 - u;
  const a = v * v * v;
  const b = 3 * v * v * u;
  const c = 3 * v * u * u;
  const d = u * u * u;
  return { x: a * p0.x + b * p1.x + c * p2.x + d * p3.x, y: a * p0.y + b * p1.y + c * p2.y + d * p3.y };
}
