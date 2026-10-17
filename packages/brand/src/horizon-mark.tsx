"use client";

import { useEffect, useRef, type CSSProperties } from "react";
import { resolveColor, type MarkState } from "./engine";
import { horizonBands } from "./geometry";
import { colors } from "./tokens";
import { useReducedMotion } from "./use-reduced-motion";

export interface HorizonMarkProps {
  /** At rest (`idle`) the mark is solid; any other state dissolves it into animated nodes. */
  state?: MarkState;
  size?: number;
  /** Dissolve into nodes when the cursor comes close. */
  interactive?: boolean;
  /** Distance (in multiples of size) at which the bands start dissolving into nodes. */
  reach?: number;
  accent?: string;
  className?: string;
  style?: CSSProperties;
  label?: string;
}

interface Dot {
  x: number;
  y: number;
  /** Per-dot threshold so the dissolve is staggered, not uniform. */
  seed: number;
  vx: number;
  vy: number;
  ox: number;
  oy: number;
}

/** Fewer, larger nodes when small so the bands still read as bands. */
function dotStep(px: number) {
  if (px < 40) return 8;
  if (px < 100) return 5.6;
  return 4.2;
}

type RGB = [number, number, number];
const rgb = (c: RGB, a: RGB, t: number) =>
  t > 0.02
    ? `rgb(${Math.round(c[0] + (a[0] - c[0]) * t)},${Math.round(c[1] + (a[1] - c[1]) * t)},${Math.round(c[2] + (a[2] - c[2]) * t)})`
    : `rgb(${c[0]},${c[1]},${c[2]})`;

/**
 * The roll logo (Horizon): solid bands at rest. As the cursor approaches, or while the
 * product is busy, the bands dissolve into nodes that animate the current state.
 */
export function HorizonMark({
  state = "idle",
  size = 160,
  interactive = true,
  reach = 1.1,
  accent = colors.accent,
  className,
  style,
  label = "roll",
}: HorizonMarkProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const reducedMotion = useReducedMotion();
  const stateRef = useRef({ state, t0: 0 });

  useEffect(() => {
    stateRef.current = { state, t0: performance.now() / 1000 };
  }, [state]);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;

    const dpr = Math.min(window.devicePixelRatio || 1, 3);
    canvas.width = Math.round(size * dpr);
    canvas.height = Math.round(size * dpr);
    const bands = horizonBands();
    const paths = bands.map((b) => new Path2D(b.d));
    const shape = new Path2D();
    for (const path of paths) shape.addPath(path);
    // Small sizes: one row of larger nodes per band, over a faint solid ghost of the mark,
    // so the dissolved logo still reads as the Horizon.
    const small = size < 56;
    const ghost = small ? 0.2 : 0;
    const step = dotStep(size);
    const dotR = step * (size < 56 ? 0.45 : 0.37);

    // Fill every band with a node grid; deterministic seeds keep renders stable.
    const dots: Dot[] = [];
    let s = 7;
    const rand = () => (s = (s * 16807) % 2147483647) / 2147483647;
    for (const b of bands) {
      const rows = size < 56 ? 1 : Math.max(1, Math.round((b.y2 - b.y1) / step));
      const rowH = (b.y2 - b.y1) / rows;
      for (let j = 0; j < rows; j++) {
        const y = b.y1 + rowH * (j + 0.5);
        const hw = b.halfWidth(y) - step * 0.4;
        for (let x = b.cx - Math.floor(hw / step) * step; x <= b.cx + hw; x += step) {
          dots.push({ x, y, seed: rand(), vx: 0, vy: 0, ox: 0, oy: 0 });
        }
      }
    }

    let ink = resolveColor(getComputedStyle(canvas).color);
    const acc = resolveColor(accent);
    const danger = resolveColor(colors.danger);
    const pointer = { x: -999, y: -999, target: 0 };
    let dissolve = 0;
    let raf = 0;
    let frame = 0;
    const k = (size / 100) * dpr;

    const draw = (ms: number) => {
      const now = ms / 1000;
      const { state: st, t0 } = stateRef.current;
      const age = now - t0;
      const busy = st !== "idle" && st !== "sleep";
      if (++frame % 30 === 0) ink = resolveColor(getComputedStyle(canvas).color);
      const target = Math.max(interactive ? pointer.target : 0, busy ? 1 : 0);
      dissolve = reducedMotion ? target : dissolve + (target - dissolve) * 0.08;
      const p = dissolve;

      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      const base = st === "sleep" ? 0.45 : 1;

      // Solid bands fade out as the nodes take over.
      const solid = Math.max(ghost * p, 1 - Math.min(1, p * 1.6)) * base;
      if (solid > 0.002) {
        ctx.setTransform(k, 0, 0, k, 0, 0);
        ctx.globalAlpha = solid;
        ctx.fillStyle = rgb(ink, ink, 0);
        ctx.fill(shape);
        ctx.setTransform(1, 0, 0, 1, 0, 0);
      }

      if (p > 0.01) {
        const t = reducedMotion ? 0 : now;
        for (const d of dots) {
          const show = Math.max(0, Math.min(1, (p - d.seed * 0.45) / 0.4));
          if (show <= 0) continue;
          let boost = 0;
          let lit = 0;
          let tint = acc;

          if (st === "thinking") {
            const w = Math.max(0, Math.cos(d.x * 0.09 - t * 5)) ** 6;
            boost += w * 0.7;
            lit = w;
          } else if (st === "deploying") {
            const w = Math.max(0, Math.cos(d.y * 0.16 + t * 7)) ** 4;
            boost += w * 0.6;
            lit = w * 0.8;
          } else if (st === "success") {
            const dist = Math.hypot(d.x - 50, d.y - 50) / 50;
            const w = Math.exp(-((dist - (age % 2.4) * 1.4) ** 2) / 0.02);
            boost += w * 0.8;
            lit = w;
          } else if (st === "error") {
            tint = danger;
            lit = 0.35 + 0.55 * Math.exp(-age * 1.5);
            if (!reducedMotion && age < 0.5) {
              d.vx += (d.seed - 0.5) * 0.5;
              d.vy += (((d.seed * 7) % 1) - 0.5) * 0.3;
            }
          }

          // Pointer: nodes scatter and glow around the cursor.
          const dx = d.x + d.ox - pointer.x;
          const dy = d.y + d.oy - pointer.y;
          const dist2 = dx * dx + dy * dy;
          const f = interactive && !reducedMotion ? Math.exp(-dist2 / 260) * pointer.target : 0;
          const dist = Math.sqrt(dist2) + 0.5;
          d.vx = (d.vx + (dx / dist) * f * 0.9 - d.ox * 0.08) * 0.82;
          d.vy = (d.vy + (dy / dist) * f * 0.9 - d.oy * 0.08) * 0.82;
          d.ox += d.vx;
          d.oy += d.vy;
          boost += f * 0.6;
          if (st !== "error") lit = Math.max(lit, Math.min(1, f * 1.4));

          ctx.globalAlpha = Math.min(1, show * 1.2) * base;
          ctx.fillStyle = rgb(ink, tint, Math.min(1, lit));
          ctx.beginPath();
          ctx.arc((d.x + d.ox) * k, (d.y + d.oy) * k, dotR * show * (1 + boost) * k, 0, Math.PI * 2);
          ctx.fill();
        }
      }
      ctx.globalAlpha = 1;
      if (!reducedMotion) raf = requestAnimationFrame(draw);
    };

    const onMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      pointer.x = ((e.clientX - rect.left) / rect.width) * 100;
      pointer.y = ((e.clientY - rect.top) / rect.height) * 100;
      const dist = Math.hypot(pointer.x - 50, pointer.y - 50) / 100;
      pointer.target = Math.max(0, Math.min(1, 1 - (dist - 0.35) / reach));
    };
    const onLeave = () => (pointer.target = 0);

    raf = requestAnimationFrame(draw);
    if (interactive && !reducedMotion) {
      window.addEventListener("pointermove", onMove, { passive: true });
      document.documentElement.addEventListener("pointerleave", onLeave);
    }
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("pointermove", onMove);
      document.documentElement.removeEventListener("pointerleave", onLeave);
    };
    // With reduced motion the canvas draws a single frame, so re-run when the state changes.
  }, [size, reach, accent, interactive, reducedMotion, reducedMotion ? state : null]);

  return (
    <canvas
      ref={canvasRef}
      role="img"
      aria-label={label}
      data-state={state}
      className={className}
      style={{ width: size, height: size, display: "block", ...style }}
    />
  );
}
