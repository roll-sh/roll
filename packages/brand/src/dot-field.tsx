"use client";

import { useEffect, useRef, type CSSProperties } from "react";
import { resolveColor } from "./engine";
import { colors } from "./tokens";
import { useReducedMotion } from "./use-reduced-motion";

export type DotFieldMode = "idle" | "thinking";

export interface DotFieldProps {
  /** Grid spacing in CSS px. */
  spacing?: number;
  /** Cursor influence radius in CSS px. */
  radius?: number;
  mode?: DotFieldMode;
  accent?: string;
  className?: string;
  style?: CSSProperties;
}

/**
 * A background grid of nodes that swells and glows around the cursor, ripples on click,
 * and sweeps with a scanning wave while `mode="thinking"`. Fills its positioned parent.
 */
export function DotField({
  spacing = 26,
  radius = 140,
  mode = "idle",
  accent = colors.accent,
  className,
  style,
}: DotFieldProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const modeRef = useRef(mode);
  const reducedMotion = useReducedMotion();

  useEffect(() => {
    modeRef.current = mode;
  }, [mode]);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;

    let w = 0;
    let h = 0;
    let dpr = 1;
    let raf = 0;
    let frame = 0;
    let ink = resolveColor(getComputedStyle(canvas).color);
    const acc = resolveColor(accent);
    const pointer = { x: -9999, y: -9999, s: 0, active: false };
    const ripples: { x: number; y: number; t0: number }[] = [];
    let modeMix = 0;

    const resize = () => {
      const rect = canvas.getBoundingClientRect();
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = rect.width;
      h = rect.height;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      if (reducedMotion) draw(0);
    };

    const draw = (ms: number) => {
      const t = ms / 1000;
      if (++frame % 60 === 0) ink = resolveColor(getComputedStyle(canvas).color);
      pointer.s += ((pointer.active ? 1 : 0) - pointer.s) * 0.08;
      modeMix += ((modeRef.current === "thinking" ? 1 : 0) - modeMix) * 0.05;
      for (let i = ripples.length - 1; i >= 0; i--) if (t - ripples[i].t0 > 2.5) ripples.splice(i, 1);

      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const r2 = radius * radius;
      const ox = (w % spacing) / 2;
      const oy = (h % spacing) / 2;

      for (let y = oy; y <= h; y += spacing) {
        for (let x = ox; x <= w; x += spacing) {
          let boost = 0;
          let lit = 0;
          let px = x;
          let py = y;
          if (pointer.s > 0.01) {
            const dx = x - pointer.x;
            const dy = y - pointer.y;
            const d2 = dx * dx + dy * dy;
            if (d2 < r2 * 4) {
              const f = Math.exp(-d2 / (r2 * 0.5)) * pointer.s;
              boost += f;
              lit = Math.max(lit, f * 0.5);
              const d = Math.sqrt(d2) + 1;
              px += (dx / d) * f * 6;
              py += (dy / d) * f * 6;
            }
          }
          for (const r of ripples) {
            const age = t - r.t0;
            const d = Math.hypot(x - r.x, y - r.y);
            const rw = Math.exp(-((d - age * 520) ** 2) / 1800) * Math.exp(-age * 1.4);
            boost += rw * 1.2;
            lit = Math.max(lit, rw * 0.8);
          }
          if (modeMix > 0.01) {
            const wave = Math.max(0, Math.cos((x + y * 0.6) / 160 - t * 2.4)) ** 10 * modeMix;
            boost += wave * 0.9;
            lit = Math.max(lit, wave * 0.6);
          }
          const alpha = Math.min(1, 0.14 + boost * 0.6);
          const rad = 1 + boost * 1.6;
          const c = lit > 0.01 ? [0, 1, 2].map((k) => Math.round(ink[k] + (acc[k] - ink[k]) * lit)) : ink;
          ctx.fillStyle = `rgba(${c[0]},${c[1]},${c[2]},${alpha})`;
          ctx.beginPath();
          ctx.arc(px, py, rad, 0, Math.PI * 2);
          ctx.fill();
        }
      }
      if (!reducedMotion) raf = requestAnimationFrame(draw);
    };

    const onMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      pointer.x = e.clientX - rect.left;
      pointer.y = e.clientY - rect.top;
      pointer.active = pointer.x >= 0 && pointer.y >= 0 && pointer.x <= rect.width && pointer.y <= rect.height;
    };
    const onLeave = () => (pointer.active = false);
    const onDown = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      ripples.push({ x: e.clientX - rect.left, y: e.clientY - rect.top, t0: performance.now() / 1000 });
    };

    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    resize();
    if (!reducedMotion) {
      raf = requestAnimationFrame(draw);
      window.addEventListener("pointermove", onMove, { passive: true });
      window.addEventListener("pointerdown", onDown, { passive: true });
      document.documentElement.addEventListener("pointerleave", onLeave);
    }
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerdown", onDown);
      document.documentElement.removeEventListener("pointerleave", onLeave);
    };
  }, [spacing, radius, accent, reducedMotion]);

  return (
    <canvas
      ref={canvasRef}
      aria-hidden
      className={className}
      style={{ position: "absolute", inset: 0, width: "100%", height: "100%", pointerEvents: "none", ...style }}
    />
  );
}
