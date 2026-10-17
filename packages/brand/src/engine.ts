import { accentIndex, accentRadius, globeNodes, horizonTarget, type Node } from "./geometry";
import { colors, densities, densityForSize, type Density } from "./tokens";

export type MarkState = "idle" | "thinking" | "deploying" | "success" | "error" | "sleep";

export const markStates: readonly MarkState[] = [
  "idle",
  "thinking",
  "deploying",
  "success",
  "error",
  "sleep",
];

export interface MarkEngineOptions {
  density?: Density | "auto";
  /** React to the pointer (attract/repel, glow) and clicks (ripple burst). */
  interactive?: boolean;
  /** Colour of the signature node and highlights. `null` disables it. */
  accent?: string | null;
  reducedMotion?: boolean;
}

type RGB = [number, number, number];

interface Particle {
  n: Node;
  x: number;
  y: number;
  vx: number;
  vy: number;
  r: number;
  lit: number;
}

interface Ripple {
  x: number;
  y: number;
  t0: number;
}

const clamp01 = (v: number) => (v < 0 ? 0 : v > 1 ? 1 : v);
const smooth = (v: number) => {
  const t = clamp01(v);
  return t * t * (3 - 2 * t);
};
const smoothstep = (a: number, b: number, v: number) => smooth((v - a) / (b - a));

let sampler: CanvasRenderingContext2D | null = null;

/** Resolve any CSS colour (including oklch from Tailwind v4) to RGB by painting one pixel. */
export function resolveColor(css: string): RGB {
  if (typeof document === "undefined") return [250, 250, 250];
  if (!sampler) {
    const c = document.createElement("canvas");
    c.width = c.height = 1;
    sampler = c.getContext("2d", { willReadFrequently: true });
  }
  if (!sampler) return [250, 250, 250];
  sampler.clearRect(0, 0, 1, 1);
  sampler.fillStyle = "#000";
  sampler.fillStyle = css;
  sampler.fillRect(0, 0, 1, 1);
  const d = sampler.getImageData(0, 0, 1, 1).data;
  return [d[0], d[1], d[2]];
}

const mix = (a: RGB, b: RGB, t: number) =>
  `rgb(${Math.round(a[0] + (b[0] - a[0]) * t)},${Math.round(a[1] + (b[1] - a[1]) * t)},${Math.round(
    a[2] + (b[2] - a[2]) * t,
  )})`;

/**
 * Canvas renderer for the roll mark. Framework-agnostic: React, a docs site, or a
 * marketing page can all drive the same engine.
 */
export class RollMarkEngine {
  private ctx: CanvasRenderingContext2D;
  private particles: Particle[] = [];
  private maxRow = 1;
  private accentIdx = -1;
  private accentR = 0;
  private size = 0;
  private dpr = 1;
  private density: Density = "regular";
  private ink: RGB = [250, 250, 250];
  private accentRgb: RGB = [255, 90, 31];
  private dangerRgb: RGB;
  private state: MarkState = "idle";
  private stateT0 = 0;
  private formation = 0;
  private pointer = { x: 0, y: 0, active: false, strength: 0 };
  private ripples: Ripple[] = [];
  private raf = 0;
  private running = false;
  private frameCount = 0;

  constructor(
    private canvas: HTMLCanvasElement,
    private opts: MarkEngineOptions = {},
  ) {
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("roll: 2D canvas not supported");
    this.ctx = ctx;
    this.dangerRgb = resolveColor(colors.danger);
    this.setAccent(opts.accent === undefined ? colors.accent : opts.accent);
    this.refreshColor();
  }

  setOptions(opts: MarkEngineOptions) {
    const densityChanged = opts.density !== undefined && opts.density !== this.opts.density;
    // `undefined` means "keep current"; `accent: null` explicitly turns the accent off.
    const defined = Object.fromEntries(Object.entries(opts).filter(([, v]) => v !== undefined));
    this.opts = { ...this.opts, ...defined };
    if (opts.accent !== undefined) this.setAccent(opts.accent);
    if (densityChanged && this.size) this.resize(this.size);
    this.kick();
  }

  private setAccent(accent: string | null) {
    this.opts.accent = accent;
    if (accent) this.accentRgb = resolveColor(accent);
  }

  refreshColor() {
    this.ink = resolveColor(getComputedStyle(this.canvas).color);
  }

  resize(cssSize: number) {
    this.size = cssSize;
    this.dpr = Math.min(window.devicePixelRatio || 1, 3);
    this.canvas.width = Math.round(cssSize * this.dpr);
    this.canvas.height = Math.round(cssSize * this.dpr);
    const d = this.opts.density && this.opts.density !== "auto" ? this.opts.density : densityForSize(cssSize);
    if (d !== this.density || this.particles.length === 0) {
      this.density = d;
      const nodes = globeNodes(densities[d]);
      this.maxRow = Math.max(1, ...nodes.map((n) => n.row));
      this.accentIdx = accentIndex(nodes);
      this.accentR = accentRadius(nodes[this.accentIdx], densities[d]);
      this.particles = nodes.map((n) => ({ n, x: n.x, y: n.y, vx: 0, vy: 0, r: n.r, lit: 0 }));
    }
    this.kick();
  }

  setState(state: MarkState) {
    if (state === this.state) return;
    this.state = state;
    this.stateT0 = performance.now() / 1000;
    if (state === "error" && !this.opts.reducedMotion) {
      for (const p of this.particles) {
        p.vx += (Math.random() - 0.5) * 0.09;
        p.vy += (Math.random() - 0.5) * 0.05;
      }
    }
    this.kick();
  }

  /** Pointer position in CSS px relative to the canvas' top-left corner. */
  setPointer(px: number, py: number) {
    const s = this.unitScale();
    this.pointer.x = (px - this.size / 2) / s;
    this.pointer.y = (py - this.size / 2) / s;
    this.pointer.active = true;
    this.kick();
  }

  clearPointer() {
    this.pointer.active = false;
  }

  /** Click ripple + impulse, in CSS px relative to the canvas. */
  burst(px: number, py: number) {
    const s = this.unitScale();
    const x = (px - this.size / 2) / s;
    const y = (py - this.size / 2) / s;
    this.ripples.push({ x, y, t0: performance.now() / 1000 });
    if (!this.opts.reducedMotion) {
      for (const p of this.particles) {
        const dx = p.x - x;
        const dy = p.y - y;
        const d = Math.hypot(dx, dy) + 0.15;
        p.vx += (dx / d) * (0.05 / d);
        p.vy += (dy / d) * (0.05 / d);
      }
    }
    this.kick();
  }

  start() {
    this.running = true;
    this.kick();
  }

  stop() {
    this.running = false;
    cancelAnimationFrame(this.raf);
    this.raf = 0;
  }

  destroy() {
    this.stop();
  }

  private kick() {
    if (!this.running || this.raf) return;
    this.raf = requestAnimationFrame(this.frame);
  }

  private unitScale() {
    return (this.size / 2) * 0.84;
  }

  private frame = (nowMs: number) => {
    this.raf = 0;
    const reduced = !!this.opts.reducedMotion;
    const now = nowMs / 1000;
    const t = reduced ? 0 : now;
    const st = reduced ? 10 : now - this.stateT0;
    const state = this.state;
    const interactive = this.opts.interactive !== false && !reduced;
    const accentOn = !!this.opts.accent;

    if (++this.frameCount % 30 === 0) this.refreshColor();

    const ps = this.pointer;
    ps.strength += ((ps.active && interactive ? 1 : 0) - ps.strength) * 0.12;

    const formationTarget = state === "sleep" || (state === "deploying" && st > 0.15) ? 1 : 0;
    this.formation = reduced
      ? formationTarget
      : this.formation + (formationTarget - this.formation) * (state === "deploying" ? 0.045 : 0.06);
    const F = this.formation;
    const ramp = smooth(st * 2.5);

    this.ripples = this.ripples.filter((r) => now - r.t0 < 2);

    const ctx = this.ctx;
    const scale = this.unitScale() * this.dpr;
    const cx = (this.size / 2) * this.dpr;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
    ctx.globalAlpha = state === "sleep" ? 0.55 + 0.45 * (1 - F) : 1;

    const inkCss = mix(this.ink, this.ink, 0);

    this.particles.forEach((p, i) => {
      const n = p.n;
      const rowFrac = n.row / this.maxRow;
      const fi = smooth((F - (1 - rowFrac) * 0.4) / 0.6);
      const h = horizonTarget(n);
      let tx = n.x + (h.x - n.x) * fi;
      let ty = n.y + (h.y - n.y) * fi;
      let s = 1 + (h.scale - 1) * fi;
      let lit = 0;
      let tint = this.accentRgb;
      let base = n.r;
      const isAccent = accentOn && i === this.accentIdx && (state === "idle" || state === "success");

      switch (state) {
        case "idle":
          s *= 0.88 + 0.14 * (0.5 + 0.5 * Math.sin(t * 1.6 - (n.x + n.y) * 2.4));
          if (isAccent) {
            base = this.accentR;
            lit = 1;
          }
          break;
        case "thinking": {
          const lon = Math.asin(Math.max(-1, Math.min(1, n.x / Math.sqrt(Math.max(1e-3, 1 - n.y * n.y)))));
          const w = 0.5 + 0.5 * Math.cos(lon * 3 - t * 5);
          s *= 1 + (0.3 + 0.95 * w - 1) * ramp;
          lit = smoothstep(0.82, 1, w) * 0.85 * ramp;
          ty += Math.sin(t * 3 + n.x * 2) * 0.012 * ramp;
          break;
        }
        case "deploying": {
          const w = Math.max(0, Math.cos(ty * 6 + st * 8));
          const inDome = n.y < 0;
          s *= 1 + w * (inDome ? 0.5 : 0.2) * smooth(st - 0.4);
          lit = inDome ? w * 0.55 * smooth(st - 0.4) : 0;
          break;
        }
        case "success": {
          // Repeats every 2.4s so a resting success state keeps signalling; real flows last ~1.8s.
          const ring = Math.exp(-((n.d - (st % 2.4) * 1.8) ** 2) / 0.025);
          s *= (0.92 + 0.08 * Math.sin(t * 1.6 - (n.x + n.y) * 2.4)) * (1 + ring * 0.9);
          lit = ring;
          if (isAccent) {
            base = this.accentR;
            s *= 1 + 0.2 * smooth(st * 2);
            lit = Math.max(lit, smooth(st * 2));
          }
          break;
        }
        case "error":
          s *= 0.85;
          tint = this.dangerRgb;
          lit = 0.25 + 0.6 * Math.exp(-st * 1.5);
          break;
        case "sleep":
          s *= 0.55 + 0.08 * Math.sin(t * 0.8 + n.x * 2);
          break;
      }

      if (ps.strength > 0.01) {
        const dx = n.x - ps.x;
        const dy = n.y - ps.y;
        const d2 = dx * dx + dy * dy;
        const f = Math.exp(-d2 / 0.09) * ps.strength;
        if (f > 0.002) {
          const d = Math.sqrt(d2) + 1e-3;
          tx += (dx / d) * f * 0.16;
          ty += (dy / d) * f * 0.16;
          s *= 1 + f * 0.8;
          if (state !== "error") lit = Math.max(lit, f * 0.45);
        }
      }

      for (const r of this.ripples) {
        const age = now - r.t0;
        const d = Math.hypot(n.x - r.x, n.y - r.y);
        const rw = Math.exp(-((d - age * 2.2) ** 2) / 0.02) * Math.exp(-age * 1.5);
        s *= 1 + rw * 0.8;
        if (state !== "error") lit = Math.max(lit, rw * 0.6);
      }

      if (reduced) {
        p.x = tx;
        p.y = ty;
        p.r = base * s;
        p.lit = lit;
      } else {
        p.vx = (p.vx + (tx - p.x) * 0.12) * 0.78;
        p.vy = (p.vy + (ty - p.y) * 0.12) * 0.78;
        p.x += p.vx;
        p.y += p.vy;
        p.r += (base * s - p.r) * 0.2;
        p.lit += (lit - p.lit) * 0.2;
      }

      if (p.r <= 0.001) return;
      ctx.fillStyle = p.lit > 0.01 ? mix(this.ink, tint, Math.min(1, p.lit)) : inkCss;
      ctx.beginPath();
      ctx.arc(cx + p.x * scale, cx + p.y * scale, p.r * scale, 0, Math.PI * 2);
      ctx.fill();
    });

    // Idle breathes continuously; reduced motion renders a single frame per change.
    if (!reduced) this.kick();
  };
}
