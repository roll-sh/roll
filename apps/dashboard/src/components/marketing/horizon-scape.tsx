import { horizonBands, useReducedMotion, type MarkState } from "@roll/brand";
import { useEffect, useRef } from "react";
import { inksOf, onSchemeChange, rgba } from "./in-view";

/** Ground dots, in world units: the camera sits CAM_H above the ground and looks at the horizon. */
const CAM_H = 1;
const Z_NEAR = 1.1;
const Z_FAR = 24;
const DZ = 0.5;
const DX = 0.62;
/** World units the camera rolls forward per second. */
const SPEED = 0.34;

const smooth = (a: number, b: number, v: number) => {
  const t = Math.min(1, Math.max(0, (v - a) / (b - a)));
  return t * t * (3 - 2 * t);
};

/** Rolling hills either side of a flat valley that runs to the horizon. */
function hill(x: number, z: number) {
  const valley = smooth(1.2, 6, Math.abs(x));
  const h = Math.sin(x * 0.33 + z * 0.19) * 0.55 + Math.sin(x * 0.12 - z * 0.14 + 1.7) * 0.85 + Math.sin(x * 0.71 + z * 0.43) * 0.18;
  return (h + 0.7) * 0.42 * valley;
}

/**
 * The marketing hero's backdrop: a dotted landscape rolling towards you, hills either side of a
 * valley that runs out to Horizon, the logo's three bands set on the skyline like a rising sun.
 * The cursor lifts and lights the ground under it, a click sends a ripple across it, and the
 * shared mark state plays here too (thinking sweeps the ground, deploying raises the sun, success
 * ripples out from the horizon). Fills its positioned parent; paused offscreen; one still frame
 * under reduced motion.
 */
export function HorizonScape({
  state = "idle",
  horizon = 0.46,
  sun = true,
  className,
}: {
  state?: MarkState;
  /** Where the skyline sits, as a fraction of the height. */
  horizon?: number;
  sun?: boolean;
  className?: string;
}) {
  const ref = useRef<HTMLCanvasElement>(null);
  const stateRef = useRef(state);
  const reduced = useReducedMotion();
  const ripplesRef = useRef<{ x: number; z: number; t0: number }[]>([]);

  useEffect(() => {
    stateRef.current = state;
    // Success ripples out from the far end of the valley, towards the viewer.
    if (state === "success") ripplesRef.current.push({ x: 0, z: Number.NaN, t0: performance.now() / 1000 });
  }, [state]);

  useEffect(() => {
    const canvas = ref.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;

    let w = 0;
    let h = 0;
    let dpr = 1;
    let raf = 0;
    let running = false;
    let inks = inksOf(canvas);
    const pointer = { x: -9999, y: -9999, s: 0, on: false };
    const ripples = ripplesRef.current;
    let busy = 0;
    let rise = 0;
    let travelled = 0;
    let last = performance.now() / 1000;

    // The sun: Horizon's three bands, filled with dots. Unit space, centred on the skyline.
    const bands = horizonBands().slice(0, 3);
    let sunDots: { x: number; y: number; band: number }[] = [];
    const buildSun = (step: number) => {
      sunDots = [];
      bands.forEach((b, band) => {
        for (let y = b.y1 + step / 2; y < b.y2; y += step) {
          const hw = b.halfWidth(y);
          for (let x = b.cx - hw + step / 2; x < b.cx + hw; x += step) sunDots.push({ x: (x - 50) / 44, y: (y - 64) / 44, band });
        }
      });
    };

    const resize = () => {
      const rect = canvas.getBoundingClientRect();
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = rect.width;
      h = rect.height;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      const sunR = Math.min(w * 0.36, h * 0.42);
      buildSun((9 / sunR) * 44);
      if (!running) draw(performance.now());
    };

    const draw = (ms: number) => {
      const t = ms / 1000;
      const dt = Math.min(0.05, t - last);
      last = t;
      const st = stateRef.current;
      if (!reduced) travelled += dt * SPEED;
      pointer.s += ((pointer.on ? 1 : 0) - pointer.s) * 0.08;
      busy += ((st === "thinking" ? 1 : 0) - busy) * 0.05;
      rise += ((st === "deploying" ? 1 : 0) - rise) * 0.04;
      const dim = st === "sleep" ? 0.45 : 1;
      for (let i = ripples.length - 1; i >= 0; i--) if (t - ripples[i].t0 > 4) ripples.splice(i, 1);

      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const hy = h * horizon;
      const f = (h - hy) * 0.92;
      const { ink, accent } = inks;
      const pr2 = 110 * 110;

      if (sun) {
        const R = Math.min(w * 0.36, h * 0.42);
        for (const d of sunDots) {
          // Deploying lifts each band a little more than the one below it.
          const lift = rise * (3 - d.band) * 0.05;
          const sx = w / 2 + d.x * R;
          const sy = hy + (d.y - lift) * R;
          let lit = 0;
          if (pointer.s > 0.01) {
            const dd = (sx - pointer.x) ** 2 + (sy - pointer.y) ** 2;
            lit = Math.exp(-dd / (pr2 * 1.4)) * pointer.s;
          }
          const sweep = busy > 0.01 ? Math.max(0, Math.cos(d.x * 2.2 - t * 2.4)) ** 12 * busy : 0;
          const a = (0.13 + rise * 0.12 + lit * 0.5 + sweep * 0.35) * dim;
          ctx.fillStyle = lit + sweep > 0.05 ? mix(ink, accent, Math.min(1, lit * 0.8 + sweep * 0.6), a) : rgba(ink, a);
          ctx.beginPath();
          ctx.arc(sx, sy, 1.15 + lit * 1.2, 0, Math.PI * 2);
          ctx.fill();
        }
      }

      // Ground, far rows first so near dots draw over far ones.
      const phase = travelled % DZ;
      for (let z = Z_FAR - phase; z > Z_NEAR; z -= DZ) {
        const wz = z + travelled;
        const fog = (1 - (z - Z_NEAR) / (Z_FAR - Z_NEAR)) ** 1.35 * smooth(Z_FAR, Z_FAR - 3, z) * smooth(Z_NEAR, Z_NEAR + 0.8, z);
        if (fog < 0.01) continue;
        const half = ((w / 2) * z) / f + DX;
        const x0 = Math.ceil(-half / DX) * DX;
        for (let x = x0; x <= half; x += DX) {
          let y = hill(x, wz);
          let lit = 0;
          for (const r of ripples) {
            const age = t - r.t0;
            const rz = Number.isNaN(r.z) ? travelled + Z_FAR * 0.9 : r.z;
            const d = Math.hypot(x - r.x, wz - rz);
            const wave = Math.exp(-((d - age * 5.5) ** 2) / 0.6) * Math.exp(-age * 0.7);
            y += wave * 0.32;
            lit = Math.max(lit, wave * 0.8);
          }
          if (busy > 0.01) {
            const sweep = Math.max(0, Math.cos(wz * 0.5 - t * 3)) ** 14 * busy;
            y += sweep * 0.12;
            lit = Math.max(lit, sweep * 0.5);
          }
          const sx = w / 2 + (x * f) / z;
          let sy = hy + ((CAM_H - y) * f) / z;
          let r = Math.max(0.5, 2.3 / Math.sqrt(z));
          if (pointer.s > 0.01) {
            const dd = (sx - pointer.x) ** 2 + (sy - pointer.y) ** 2;
            if (dd < pr2 * 4) {
              const p = Math.exp(-dd / pr2) * pointer.s;
              sy -= p * 14;
              r *= 1 + p * 0.9;
              lit = Math.max(lit, p * 0.7);
            }
          }
          if (sy < hy - 2 || sy > h + 4) continue;
          const a = Math.min(1, (0.16 + y * 0.32 + lit * 0.6) * fog * 1.25) * dim;
          ctx.fillStyle = lit > 0.04 ? mix(ink, accent, lit, a) : rgba(ink, a);
          if (r < 0.9) ctx.fillRect(sx - r, sy - r, r * 2, r * 2);
          else {
            ctx.beginPath();
            ctx.arc(sx, sy, r, 0, Math.PI * 2);
            ctx.fill();
          }
        }
      }
      if (running) raf = requestAnimationFrame(draw);
    };

    const start = () => {
      if (running || reduced) return;
      running = true;
      last = performance.now() / 1000;
      raf = requestAnimationFrame(draw);
    };
    const stop = () => {
      running = false;
      cancelAnimationFrame(raf);
    };

    const onMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      pointer.x = e.clientX - rect.left;
      pointer.y = e.clientY - rect.top;
      pointer.on = pointer.x >= 0 && pointer.y >= 0 && pointer.x <= rect.width && pointer.y <= rect.height;
    };
    const onLeave = () => (pointer.on = false);
    // A click on the ground ripples from the spot it landed on, in the ground's own perspective.
    const onDown = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      const px = e.clientX - rect.left;
      const py = e.clientY - rect.top;
      const hy = h * horizon;
      if (px < 0 || px > w || py < hy + 4 || py > h) return;
      const f = (h - hy) * 0.92;
      const z = (CAM_H * f) / (py - hy);
      ripples.push({ x: ((px - w / 2) * z) / f, z: z + travelled, t0: performance.now() / 1000 });
    };

    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    const io = new IntersectionObserver(([e]) => (e.isIntersecting ? start() : stop()));
    io.observe(canvas);
    const offScheme = onSchemeChange(() => {
      inks = inksOf(canvas);
      if (!running) draw(performance.now());
    });
    resize();
    if (!reduced) {
      window.addEventListener("pointermove", onMove, { passive: true });
      window.addEventListener("pointerdown", onDown, { passive: true });
      document.documentElement.addEventListener("pointerleave", onLeave);
    }
    return () => {
      stop();
      ro.disconnect();
      io.disconnect();
      offScheme();
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerdown", onDown);
      document.documentElement.removeEventListener("pointerleave", onLeave);
    };
  }, [horizon, sun, reduced]);

  return (
    <canvas
      ref={ref}
      aria-hidden
      className={`pointer-events-none absolute inset-0 size-full text-fg ${className ?? ""}`}
    />
  );
}

const mix = (a: number[], b: number[], t: number, alpha: number) =>
  `rgba(${Math.round(a[0] + (b[0] - a[0]) * t)},${Math.round(a[1] + (b[1] - a[1]) * t)},${Math.round(a[2] + (b[2] - a[2]) * t)},${alpha})`;
