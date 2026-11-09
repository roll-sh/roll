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

export interface SceneEdge {
  from: string;
  to: string;
  busy: boolean;
  dashed: boolean;
  /**
   * The line as the layout routed it (world px, parent side first: `routeEdges`); its ends follow
   * the cards while they ease.
   */
  points?: readonly Point[];
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
/** An edge to a card that left fades this long, with the card (`CARD_OUT`). */
const EDGE_OUT_MS = 160;
/** How far past the screen (px) a card counts as off it (`data-off`: its animations pause). */
const OFF_MARGIN = 160;
/** How long after the view last moved it counts as resting (`data-moving` goes). */
const VIEW_REST_MS = 160;
/** At most this many cards moving at once are each a layer of their own while they move (`place`). */
const OWN_LAYERS_MOVING = 12;
/** How far from the pointer (px) dots swell and move. */
const POINTER_REACH = 280;
const DOT_SPACING = 28;
/** The busy sweep: crests along x + 0.6y (screen px), `SWEEP_WAVE`·2π apart, moving at `SWEEP_SPEED` radians a second. */
const SWEEP_WAVE = 220;
const SWEEP_SPEED = 1.6;
/** Below this the sweep doesn't change a dot (its boost would be under 0.002). */
const SWEEP_FLOOR = 0.004;
/** How far (screen px) the sweep keeps from the cards' extent. */
const SWEEP_CLEAR = 56;
/** Over how far past that (screen px) the sweep fades in. */
const SWEEP_FADE = 220;

/** How much the busy sweep lifts a dot at screen (x, y): 0–`mix`. */
const sweepAt = (x: number, y: number, t: number, mix: number) => Math.max(0, Math.cos((x + y * 0.6) / SWEEP_WAVE - t * SWEEP_SPEED)) ** 12 * mix;

/**
 * Where the busy sweep lifts dots at all, as stretches of x + 0.6y between `lo` and `hi`: about a
 * quarter of the screen. Outside them the field is the plain pattern.
 */
function sweepBands(t: number, mix: number, lo: number, hi: number): [number, number][] {
  const floor = SWEEP_FLOOR / mix;
  if (floor >= 1) return [];
  const half = Math.acos(floor ** (1 / 12)) * SWEEP_WAVE;
  const period = 2 * Math.PI * SWEEP_WAVE;
  const phase = t * SWEEP_SPEED * SWEEP_WAVE;
  const out: [number, number][] = [];
  for (let c = phase + Math.ceil((lo - half - phase) / period) * period; c - half <= hi; c += period) out.push([c - half, c + half]);
  return out;
}
/** How long the zoom rests before a touch screen's `--zoom` follows it. */
const ZOOM_CSS_MS = 120;

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
  /**
   * The `--zoom` last written on the world. Only a touch screen's CSS reads it, and writing it
   * restyles every card (it's inherited: ~100 ms at 200 cards), so it's written only there, once the
   * zoom rests.
   */
  private zoomShown = Number.NaN;
  private zoomCss = 0;
  private touchCss = typeof window !== "undefined" && typeof window.matchMedia === "function" ? window.matchMedia("(hover: none)") : null;
  private viewFrom: View | null = null;
  private viewTo: View | null = null;
  private viewT0 = 0;
  private viewMs = 280;

  private els = new Map<string, HTMLElement>();
  /** Cards marked `data-off`, kept here: reading the attribute back off every card each frame cost a few ms at 1000 threads. */
  private offCards = new WeakSet<HTMLElement>();
  /**
   * Cards measured with their contents laid out. Only those go `data-off`: off, a card's contents
   * are skipped and it measures as the placeholder, so a card that went off before it was ever
   * measured laid out as 280 × 120 and grew under a Fit that had framed it.
   */
  private measured = new Set<string>();
  /** When the view last moved (ms, `performance.now()`): until `VIEW_REST_MS` after, it's moving. */
  private viewMovedAt = -Infinity;
  private cur = new Map<string, Point>();
  private tgt = new Map<string, Point>();
  /** The layout's targets, before a drag holds one card. */
  private slots: ReadonlyMap<string, Point> = new Map();
  private size = new Map<string, Size>();
  private shownSize = new Map<string, Size>();
  private meta = new Map<string, SceneMeta>();
  private edges: SceneEdge[] = [];
  /**
   * Edges to a card that left the graph: drawn where it was, fading out with it (`CARD_OUT`,
   * 160 ms ease-in), then dropped; they vanished at once while the card faded. `gone` is where
   * each card that left was.
   */
  private fading: { edge: SceneEdge; t0: number }[] = [];
  private gone = new Map<string, Point>();
  private frameMs = 0;
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
  /** When cards last eased (ms). */
  private lastEase = 0;

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
        // Off screen its contents are skipped and it measures as the placeholder: keep its real
        // size (taken, the layout moved cards on and off screen and never settled).
        if (el.hasAttribute("data-off") && this.size.has(key)) continue;
        if (!el.hasAttribute("data-off")) this.measured.add(key);
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
    clearTimeout(this.zoomCss);
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

  /** Where the view is going (the view itself when it isn't easing anywhere). */
  get goal(): View {
    return this.viewTo ?? this.view;
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
    // What the layout sized each card as (a section's header is a fixed row): edges meet those edges.
    this.shownSize = new Map(sizes);
    if (!this.opts.reducedMotion) {
      const now = performance.now();
      for (const e of this.edges) {
        if (targets.has(e.from) && targets.has(e.to)) continue;
        if (this.cur.has(e.from) && this.cur.has(e.to)) this.fading.push({ edge: e, t0: now });
      }
      for (const [k, p] of this.cur) if (!targets.has(k)) this.gone.set(k, p);
    }
    // Back while its edges fade: the live edges take over.
    for (const k of targets.keys()) this.gone.delete(k);
    this.fading = this.fading.filter((f) => !(targets.has(f.edge.from) && targets.has(f.edge.to)));
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
    this.viewMovedAt = performance.now();
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

  /**
   * Zoom about a point; without one (the zoom keys and buttons), about the middle, unless the
   * canvas's top-left is on screen: then about it, so zooming in from Fit keeps the machines and
   * projects on the left in view (about the middle pushed them off).
   */
  zoomBy(f: number, sx?: number, sy?: number, animate = true) {
    const v = this.viewTo ?? this.view;
    let x0 = Infinity;
    let y0 = Infinity;
    for (const p of this.tgt.values()) (x0 = Math.min(x0, p.x)), (y0 = Math.min(y0, p.y));
    const left = x0 * v.k + v.x;
    const top = y0 * v.k + v.y;
    const ax = sx ?? (left >= 0 && left < this.w / 2 ? left : this.w / 2);
    const ay = sy ?? (top >= 0 && top < this.h / 2 ? top : this.h / 2);
    this.setView(zoomAt(v, v.k * f, ax, ay), animate);
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
    // The dots answer the pointer on open canvas only: not under a card or the canvas's own controls.
    const t = e.target as HTMLElement | null;
    this.pointer.active = !t?.closest?.("[data-card], button, [role=toolbar], [data-chrome]");
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
    // What hangs off the view's size (the minimap shows only on a wide canvas) follows.
    this.opts.onView?.(this.view);
    this.kick();
  }

  kick = () => {
    if (this.destroyed || this.raf || !this.visible || document.hidden) return;
    this.raf = requestAnimationFrame(this.frame);
  };

  // What each element's transform was last set to: unchanged, it isn't written again (the loop runs
  // every frame while anything works, and rewriting 200 cards' transforms restyled them each frame).
  private shown = new WeakMap<HTMLElement, string>();
  private setTransform(el: HTMLElement, v: string) {
    if (this.shown.get(el) === v) return;
    this.shown.set(el, v);
    el.style.transform = v;
  }

  // Moving (easing, dragged), a card is a layer of its own (translate3d), so it moves without the
  // canvas repainting under it (Safari repaints otherwise); at rest it's plain (translate), so 200
  // cards aren't 200 layers to commit every frame (Chromium). A crowd moving together (a filter
  // cleared, a group unfolded) stays plain: 200 layers made and dropped in one frame repainted
  // every card (a 70 ms frame at 206 threads). It stays plain until all of it rests: its last
  // dozen settling as layers of their own, one fewer each frame, had the page layerized again every
  // frame for a second (15–30 ms each).
  private place(el: HTMLElement, p: Point, moving = false) {
    const own = moving && !this.crowd;
    this.setTransform(el, own ? `translate3d(${p.x}px, ${p.y}px, 0)` : `translate(${p.x}px, ${p.y}px)`);
  }
  /** Cards are moving together (more than `OWN_LAYERS_MOVING` at once), until they all rest. */
  private crowd = false;

  private frame = (ms: number) => {
    this.raf = 0;
    const reduced = !!this.opts.reducedMotion;
    const t = ms / 1000;
    let moving = false;

    // View animation (ease-out cubic). A frame's timestamp is when the frame began, which can be
    // before `setView` stamped `viewT0` in a long task: a negative `u` eased backwards past zero
    // (⇧D at 15% froze the page: the dot grid never reached its spacing at a negative zoom).
    if (this.viewTo && this.viewFrom) {
      const u = Math.min(1, Math.max(0, (ms - this.viewT0) / this.viewMs));
      const e = 1 - (1 - u) ** 3;
      const a = this.viewFrom;
      const b = this.viewTo;
      this.view = { x: a.x + (b.x - a.x) * e, y: a.y + (b.y - a.y) * e, k: a.k + (b.k - a.k) * e };
      if (u >= 1) this.viewFrom = this.viewTo = null;
      else moving = true;
      this.viewMovedAt = ms;
      this.opts.onView?.(this.view);
    }
    // While the view moves (a zoom, a pan, Fit easing), nothing on the canvas repaints: any repaint
    // has the browser paint, layerize and commit the whole page again, every frame (~20 ms at 1000
    // threads). So the breathing animations hold (`data-moving`, globals.css) and cards keep their
    // `data-off` until it rests.
    const shifting = ms - this.viewMovedAt < VIEW_REST_MS;
    if (shifting !== this.container.hasAttribute("data-moving")) this.container.toggleAttribute("data-moving", shifting);
    if (shifting) moving = true;
    this.setTransform(this.world, `translate3d(${this.view.x}px, ${this.view.y}px, 0) scale(${this.view.k})`);
    // The scale for CSS too (`--zoom`): a touch screen's card tools keep a finger's size with it.
    if (this.view.k !== this.zoomShown && this.touchCss?.matches) {
      // The card whose tools show (tapped, focused, open beside the canvas) follows at once, on
      // its own (a small restyle); the rest once the zoom rests.
      for (const el of this.world.querySelectorAll<HTMLElement>("[data-card]:is([data-tapped], :focus-within, [data-selected])")) el.style.setProperty("--zoom", String(this.view.k));
      clearTimeout(this.zoomCss);
      this.zoomCss = window.setTimeout(() => {
        this.zoomShown = this.view.k;
        this.world.style.setProperty("--zoom", String(this.view.k));
        for (const el of this.world.querySelectorAll<HTMLElement>("[data-card][style*='--zoom']")) el.style.removeProperty("--zoom");
      }, ZOOM_CSS_MS);
    }

    // Cards ease to their targets: where each goes this frame, and how many move (`place`).
    const { x: vx, y: vy, k: vk } = this.view;
    const steps: [string, Point, boolean][] = [];
    let movers = 0;
    // Ease-out by time, not frames: 99 % of the way in ~250 ms at any frame rate.
    const now = performance.now();
    const dt = Math.min(34, Math.max(1, now - (this.lastEase || now - 16)));
    this.lastEase = now;
    const ease = 1 - Math.exp(-dt / 54);
    for (const [k, target] of this.tgt) {
      const c = this.cur.get(k) ?? target;
      let next = target;
      if (!reduced && (!this.drag || this.drag.key !== k)) {
        const dx = target.x - c.x;
        const dy = target.y - c.y;
        if (Math.abs(dx) > 0.3 || Math.abs(dy) > 0.3) {
          next = { x: c.x + dx * ease, y: c.y + dy * ease };
          moving = true;
        }
      }
      this.cur.set(k, next);
      const goes = next !== target || this.drag?.key === k;
      if (goes) movers++;
      steps.push([k, next, goes]);
    }
    this.crowd = movers > OWN_LAYERS_MOVING || (this.crowd && movers > 0);
    for (const [k, next, goes] of steps) {
      const el = this.els.get(k);
      if (el) {
        this.place(el, next, goes);
        // Well off screen, its animations (a busy card's border, shimmer, pulses) pause: 80 busy
        // cards at 1000 threads repainted every frame, most of them out of sight.
        const s = this.size.get(k);
        let off = this.offCards.has(el);
        if (s && this.w > 0 && this.measured.has(k)) {
          const left = next.x * vk + vx;
          const top = next.y * vk + vy;
          const outside = left + s.w * vk < -OFF_MARGIN || left > this.w + OFF_MARGIN || top + s.h * vk < -OFF_MARGIN || top > this.h + OFF_MARGIN;
          // While the view moves, a card only comes back (`data-off` skips its paint: it would show
          // blank); it goes off once the view rests.
          off = shifting ? off && outside : outside;
        }
        if (off !== this.offCards.has(el)) {
          el.toggleAttribute("data-off", off);
          if (off) this.offCards.add(el);
          else this.offCards.delete(el);
        }
      }
    }

    this.pointer.s += ((this.pointer.active && !reduced ? 1 : 0) - this.pointer.s) * 0.1;
    this.busyMix += ((this.anyBusy ? 1 : 0) - this.busyMix) * (reduced ? 1 : 0.04);
    this.ripples = this.ripples.filter((r) => t - r.t0 < 2.2);
    this.frameMs = ms;
    if (this.fading.length) {
      this.fading = reduced ? [] : this.fading.filter((f) => ms - f.t0 < EDGE_OUT_MS);
      if (!this.fading.length) this.gone.clear();
    }

    this.draw(reduced ? 0 : t, reduced);

    const live = !reduced && (this.anyBusy || this.pointer.s > 0.01 || this.ripples.length > 0 || this.busyMix > 0.01 || this.fading.length > 0);
    if (moving || live) this.kick();
  };

  // One dot on a tile the grid's spacing wide, and the pattern made of it (made again as the spacing
  // or the colour changes).
  private dotTile: { key: string; pattern: CanvasPattern; size: number } | null = null;
  private tileCanvas: HTMLCanvasElement | null = null;

  /**
   * The plain dot field (`s` apart from `x0`, `y0`) as one pattern fill, leaving out the pointer's
   * reach and the busy sweep's `bands` (drawn dot by dot). False when a pattern can't be made: draw
   * them all one by one.
   */
  private fillDots(ctx: CanvasRenderingContext2D, s: number, x0: number, y0: number, pointer: Point | null, bands: readonly [number, number][]): boolean {
    if (typeof DOMMatrix === "undefined" || typeof document === "undefined") return false;
    const size = Math.max(2, Math.round(s * this.dpr));
    const color = rgba(this.ink, Math.min(0.9, this.dotAlpha));
    const key = `${size}|${s.toFixed(2)}|${color}`;
    if (this.dotTile?.key !== key) {
      const c = (this.tileCanvas ??= document.createElement("canvas"));
      c.width = c.height = size;
      const t = c.getContext("2d");
      if (!t) return false;
      t.clearRect(0, 0, size, size);
      t.fillStyle = color;
      t.beginPath();
      t.arc(size / 2, size / 2, (0.9 * size) / s, 0, Math.PI * 2);
      t.fill();
      const pattern = ctx.createPattern(c, "repeat");
      if (!pattern) return false;
      this.dotTile = { key, pattern, size };
    }
    const { pattern } = this.dotTile;
    pattern.setTransform(new DOMMatrix().translateSelf(x0 - s / 2, y0 - s / 2).scaleSelf(s / size));
    ctx.save();
    // Two clips, so where the pointer's reach crosses a band it stays out (one even-odd path would let it back in).
    if (pointer) {
      const clip = new Path2D();
      clip.rect(0, 0, this.w, this.h);
      clip.arc(pointer.x, pointer.y, POINTER_REACH, 0, Math.PI * 2);
      ctx.clip(clip, "evenodd");
    }
    if (bands.length) {
      const clip = new Path2D();
      clip.rect(0, 0, this.w, this.h);
      const h = this.h;
      for (const [a, b] of bands) {
        // x + 0.6y between a and b, over the screen's height.
        clip.moveTo(a, 0);
        clip.lineTo(b, 0);
        clip.lineTo(b - 0.6 * h, h);
        clip.lineTo(a - 0.6 * h, h);
        clip.closePath();
      }
      ctx.clip(clip, "evenodd");
    }
    ctx.fillStyle = pattern;
    ctx.fillRect(0, 0, this.w, this.h);
    ctx.restore();
    return true;
  }

  private draw(t: number, reduced: boolean) {
    const ctx = this.ctx;
    if (!ctx) return;
    const { x: vx, y: vy, k } = this.view;
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    ctx.clearRect(0, 0, this.w, this.h);

    // Dot field in world space, coarser as you zoom out.
    let step = DOT_SPACING;
    // Never at or below zero, so the spacing loop and the rows below always end.
    const gk = Number.isFinite(k) && k > 0.01 ? k : 0.01;
    while (step * gk < 14) step *= 2;
    const s = step * gk;
    const x0 = (((vx % s) + s) % s) - s;
    const y0 = (((vy % s) + s) % s) - s;
    const r2 = (POINTER_REACH / 2) ** 2;
    const ps = this.pointer;
    const pointer = ps.s > 0.01 ? ps : null;
    const mix = reduced ? 0 : this.busyMix;
    // The sweep keeps off the cards and the space around them: between small cards far out it read
    // as a blob of blue dots behind the work. It runs through the open canvas only.
    const mask = mix > 0.01 || pointer || this.ripples.length ? this.contentBox(vx, vy, k, SWEEP_CLEAR) : null;
    const xEnd = this.w + s;
    const yEnd = this.h + s;
    const bands = mix > 0.01 ? sweepBands(t, mix, x0 + 0.6 * y0, xEnd + 0.6 * yEnd) : [];
    // Without a ripple, the plain dots are one pattern fill, and only the dots the pointer or the
    // sweep lift are drawn one by one: 6,000 arcs a frame, each its own fill, were most of a frame
    // whenever anything ran.
    const patterned = this.ripples.length === 0 && this.fillDots(ctx, s, x0, y0, pointer, bands);
    const reach = POINTER_REACH;
    const plain = new Path2D();
    // Lifted dots by how much (boost and light, in twentieths): one fill per step, not per dot.
    const lifted = new Map<number, Path2D>();
    const dot = (x: number, y: number) => {
      // Behind the work (and 56 px around it) the field stays still: no swell, ripple or sweep.
      const quiet = !!mask && x > mask.x0 && x < mask.x1 && y > mask.y0 && y < mask.y1;
      let boost = 0;
      let lit = 0;
      let px = x;
      let py = y;
      if (pointer && !quiet) {
        const dx = x - pointer.x;
        const dy = y - pointer.y;
        const d2 = dx * dx + dy * dy;
        if (d2 < r2 * 4) {
          const f = Math.exp(-d2 / (r2 * 0.5)) * pointer.s;
          boost += f;
          lit = Math.max(lit, f * 0.4);
          const d = Math.sqrt(d2) + 1;
          px += (dx / d) * f * 5;
          py += (dy / d) * f * 5;
        }
      }
      for (const r of quiet ? [] : this.ripples) {
        const age = t - r.t0;
        const d = Math.hypot(x - r.x, y - r.y);
        const rw = Math.exp(-((d - age * 520) ** 2) / 1800) * Math.exp(-age * 1.4);
        boost += rw;
        lit = Math.max(lit, rw * 0.7);
      }
      if (mix > 0.01 && !quiet) {
        // The DotField "thinking" sweep, quieter: work is happening somewhere on the canvas. It
        // fades out as it nears the work (over SWEEP_FADE), so no still frame shows a bright crest
        // cut off square at the cards' box.
        const out = mask ? Math.hypot(Math.max(mask.x0 - x, 0, x - mask.x1), Math.max(mask.y0 - y, 0, y - mask.y1)) : SWEEP_FADE;
        const near = Math.min(1, out / SWEEP_FADE);
        const wave = sweepAt(x, y, t, mix) * near * near * (3 - 2 * near);
        boost += wave * 0.3;
        lit = Math.max(lit, wave * 0.22);
      }
      if (boost < 0.002 && lit <= 0.01) {
        plain.moveTo(px + 0.9, py);
        plain.arc(px, py, 0.9, 0, Math.PI * 2);
        return;
      }
      const qb = Math.min(63, Math.round(boost * 20));
      const ql = Math.round(Math.min(1, lit) * 20);
      const key = qb * 32 + ql;
      let path = lifted.get(key);
      if (!path) lifted.set(key, (path = new Path2D()));
      const r = 0.9 + (qb / 20) * 1.2;
      path.moveTo(px + r, py);
      path.arc(px, py, r, 0, Math.PI * 2);
    };
    const spans: [number, number][] = [];
    for (let j = 0, y = y0; y <= yEnd; j++, y = y0 + j * s) {
      // This row's stretches drawn dot by dot: all of it, or (over the pattern) the sweep's bands and the pointer's reach.
      spans.length = 0;
      if (!patterned) spans.push([x0, xEnd]);
      else {
        for (const [a, b] of bands) spans.push([a - 0.6 * y, b - 0.6 * y]);
        if (pointer) {
          const dy = y - pointer.y;
          if (dy * dy < reach * reach) {
            const dx = Math.sqrt(reach * reach - dy * dy);
            spans.push([pointer.x - dx, pointer.x + dx]);
          }
        }
        if (!spans.length) continue;
        spans.sort((p, q) => p[0] - q[0]);
      }
      // Grid columns, so a dot two stretches share is drawn once.
      let next = 0;
      for (const [a, b] of spans) {
        const last = Math.floor((Math.min(b, xEnd) - x0) / s);
        for (let i = Math.max(next, Math.ceil((a - x0) / s)); i <= last; i++) dot(x0 + i * s, y);
        next = Math.max(next, last + 1);
      }
    }
    ctx.fillStyle = rgba(this.ink, Math.min(0.9, this.dotAlpha));
    ctx.fill(plain);
    for (const [key, path] of lifted) {
      const qb = key >> 5;
      const ql = (key & 31) / 20;
      const c = ql > 0.01 ? mixRgb(this.ink, this.accent, ql) : this.ink;
      ctx.fillStyle = rgba(c, Math.min(0.9, this.dotAlpha + (qb / 20) * 0.5));
      ctx.fill(path);
    }

    const scr = (p: Point) => ({ x: p.x * k + vx, y: p.y * k + vy });
    const phase = (t / PULSE_S) % 1;

    // Edges, as the layout routed them (`routeEdges`). A parent's lines are one path stroked once, so
    // a trunk its children share is one line, not a stack of translucent ones; a busy child's last
    // leg goes over it in the accent with a dot travelling along it.
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    const at = (key: string) => this.cur.get(key) ?? this.gone.get(key);
    const sized = (key: string) => this.shownSize.get(key) ?? this.size.get(key);
    const radius = 8 * k;
    // A port is a ring where a line meets a card; small, it's a tick: only where it reads as a ring.
    const ports = k >= 0.6;
    const pr = 2.75 * Math.min(1, k);
    const drawn = [...this.edges.map((edge) => ({ edge, alpha: 1 })), ...this.fading.map((f) => ({ edge: f.edge, alpha: 1 - Math.min(1, Math.max(0, (this.frameMs - f.t0) / EDGE_OUT_MS)) ** 2 }))];
    const groups = new Map<string, { path: Path2D; stroke: string; alpha: number; dashed: boolean; ends: Point[] }>();
    const busy: Point[][] = [];
    for (const { edge: e, alpha } of drawn) {
      const pts = this.edgePoints(e, at, sized, scr);
      if (!pts || alpha <= 0) continue;
      // Off screen (most of 200 threads' edges): nothing to stroke.
      let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
      for (const p of pts) (x0 = Math.min(x0, p.x)), (y0 = Math.min(y0, p.y)), (x1 = Math.max(x1, p.x)), (y1 = Math.max(y1, p.y));
      if (x1 < -8 || y1 < -8 || x0 > this.w + 8 || y0 > this.h + 8) continue;
      // A busy child's line is part of its parent's like any other; only its last leg (into the
      // child) is the accent, so a shared trunk or bus never lights up.
      if (e.busy) busy.push(pts.slice(-2));
      const id = `${e.from}|${e.dashed ? 1 : 0}|${alpha.toFixed(2)}`;
      let g = groups.get(id);
      if (!g) groups.set(id, (g = { path: new Path2D(), stroke: e.tint ? rgba(this.tint(e.tint), 0.42) : rgba(this.line, 1), alpha, dashed: e.dashed, ends: [pts[0]] }));
      tracePolyline(g.path, pts, radius);
      g.ends.push(pts[pts.length - 1]);
    }
    ctx.lineWidth = 1.25;
    for (const g of groups.values()) {
      ctx.globalAlpha = g.alpha;
      ctx.setLineDash(g.dashed ? [3, 4] : []);
      ctx.strokeStyle = g.stroke;
      ctx.stroke(g.path);
      ctx.setLineDash([]);
      if (ports)
        for (const q of g.ends) {
          ctx.beginPath();
          ctx.arc(q.x, q.y, pr, 0, Math.PI * 2);
          ctx.fillStyle = rgba(this.ground, 1);
          ctx.fill();
          ctx.stroke();
        }
    }
    ctx.globalAlpha = 1;
    for (const pts of busy) {
      ctx.strokeStyle = rgba(this.accent, 0.5);
      ctx.beginPath();
      tracePolyline(ctx, pts, radius);
      ctx.stroke();
      if (reduced) continue;
      // A dot travelling down the line with a short tail.
      const along = polylineAt(pts);
      for (let i = 0; i < 4; i++) {
        const u = phase - i * 0.035;
        if (u < 0) continue;
        const q = along(easeInOut(u));
        ctx.fillStyle = rgba(this.accent, (1 - i / 4) * 0.9);
        ctx.beginPath();
        ctx.arc(q.x, q.y, Math.max(1.2, 2.4 * Math.min(1, k)) * (1 - i * 0.18), 0, Math.PI * 2);
        ctx.fill();
      }
    }
  }

  /** The cards' extent on screen, `margin` px bigger each way. */
  private contentBox(vx: number, vy: number, k: number, margin: number) {
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const [key, p] of this.cur) {
      const s = this.shownSize.get(key) ?? this.size.get(key);
      if (!s) continue;
      x0 = Math.min(x0, p.x);
      y0 = Math.min(y0, p.y);
      x1 = Math.max(x1, p.x + s.w);
      y1 = Math.max(y1, p.y + s.h);
    }
    if (x0 === Infinity) return null;
    return { x0: x0 * k + vx - margin, y0: y0 * k + vy - margin, x1: x1 * k + vx + margin, y1: y1 * k + vy + margin };
  }

  /**
   * An edge's line on screen. Its route was planned from where the cards are going; while they ease
   * there, its first leg follows the parent and its last leg the child, so it stays attached.
   */
  private edgePoints(e: SceneEdge, at: (k: string) => Point | undefined, sized: (k: string) => Size | undefined, scr: (p: Point) => Point): Point[] | null {
    const a = at(e.from);
    const b = at(e.to);
    const sa = sized(e.from);
    const sb = sized(e.to);
    if (!a || !b || !sa || !sb) return null;
    const route = e.points;
    if (!route || route.length < 2) return [scr({ x: a.x + sa.w, y: a.y + sa.h / 2 }), scr({ x: b.x, y: b.y + sb.h / 2 })];
    const ta = this.tgt.get(e.from) ?? a;
    const tb = this.tgt.get(e.to) ?? b;
    const da = { x: a.x - ta.x, y: a.y - ta.y };
    const db = { x: b.x - tb.x, y: b.y - tb.y };
    const pts = route.map((p) => ({ ...p }));
    const n = pts.length;
    const flat = (i: number, j: number) => Math.abs(route[i].y - route[j].y) < 0.5;
    pts[0].x += da.x;
    pts[0].y += da.y;
    if (n > 2) {
      if (flat(0, 1)) pts[1].y += da.y;
      else pts[1].x += da.x;
    }
    pts[n - 1].x += db.x;
    pts[n - 1].y += db.y;
    if (n > 2) {
      if (flat(n - 1, n - 2)) pts[n - 2].y += db.y;
      else pts[n - 2].x += db.x;
    }
    return pts.map(scr);
  }
}

/** An orthogonal line through `pts`, its corners rounded to `r` (at most half of either leg). */
function tracePolyline(ctx: Pick<CanvasRenderingContext2D, "moveTo" | "lineTo" | "arcTo">, pts: readonly Point[], r: number) {
  ctx.moveTo(pts[0].x, pts[0].y);
  for (let i = 1; i < pts.length - 1; i++) {
    const o = pts[i - 1];
    const p = pts[i];
    const q = pts[i + 1];
    const rr = Math.min(r, Math.hypot(p.x - o.x, p.y - o.y) / 2, Math.hypot(q.x - p.x, q.y - p.y) / 2);
    if (rr > 0.5) ctx.arcTo(p.x, p.y, q.x, q.y, rr);
    else ctx.lineTo(p.x, p.y);
  }
  ctx.lineTo(pts[pts.length - 1].x, pts[pts.length - 1].y);
}

/** The point `u` (0–1) of the way along a polyline. */
function polylineAt(pts: readonly Point[]): (u: number) => Point {
  const legs = pts.slice(1).map((p, i) => Math.hypot(p.x - pts[i].x, p.y - pts[i].y));
  const total = legs.reduce((s, l) => s + l, 0) || 1;
  return (u) => {
    let d = Math.min(1, Math.max(0, u)) * total;
    for (let i = 0; i < legs.length; i++) {
      if (d <= legs[i] || i === legs.length - 1) {
        const f = legs[i] ? Math.min(1, d / legs[i]) : 0;
        return { x: pts[i].x + (pts[i + 1].x - pts[i].x) * f, y: pts[i].y + (pts[i + 1].y - pts[i].y) * f };
      }
      d -= legs[i];
    }
    return pts[pts.length - 1];
  };
}

const easeInOut = (u: number) => (u < 0.5 ? 2 * u * u : 1 - (-2 * u + 2) ** 2 / 2);
