"use client";

import { useEffect, useRef, type CSSProperties } from "react";
import { RollMarkEngine, type MarkState } from "./engine";
import type { Density } from "./tokens";
import { useReducedMotion } from "./use-reduced-motion";

export interface RollMarkProps {
  state?: MarkState;
  /** Rendered size in CSS px (square). */
  size?: number;
  density?: Density | "auto";
  /** Dots react to the cursor and ripple on click. */
  interactive?: boolean;
  /** How far outside the mark (in multiples of its size) the cursor still attracts it. */
  reach?: number;
  accent?: string | null;
  className?: string;
  style?: CSSProperties;
  label?: string;
}

export function RollMark({
  state = "idle",
  size = 64,
  density = "auto",
  interactive = true,
  reach = 0.6,
  accent,
  className,
  style,
  label = "roll",
}: RollMarkProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const engineRef = useRef<RollMarkEngine | null>(null);
  const reducedMotion = useReducedMotion();

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const engine = new RollMarkEngine(canvas, { density, interactive, accent, reducedMotion });
    engineRef.current = engine;
    engine.resize(size);
    engine.setState(state);

    // Only animate while on screen.
    const io = new IntersectionObserver(([entry]) => (entry.isIntersecting ? engine.start() : engine.stop()));
    io.observe(canvas);
    return () => {
      io.disconnect();
      engine.destroy();
      engineRef.current = null;
    };
    // The engine is created once; later prop changes are applied by the effects below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => engineRef.current?.setState(state), [state]);
  useEffect(() => engineRef.current?.resize(size), [size]);
  useEffect(
    () => engineRef.current?.setOptions({ density, interactive, accent, reducedMotion }),
    [density, interactive, accent, reducedMotion],
  );

  useEffect(() => {
    if (!interactive) return;
    const canvas = canvasRef.current;
    if (!canvas) return;
    const onMove = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      const x = e.clientX - rect.left;
      const y = e.clientY - rect.top;
      const pad = rect.width * reach;
      const inside = x > -pad && y > -pad && x < rect.width + pad && y < rect.height + pad;
      if (inside) engineRef.current?.setPointer(x, y);
      else engineRef.current?.clearPointer();
    };
    const onLeave = () => engineRef.current?.clearPointer();
    const onDown = (e: PointerEvent) => {
      const rect = canvas.getBoundingClientRect();
      engineRef.current?.burst(e.clientX - rect.left, e.clientY - rect.top);
    };
    window.addEventListener("pointermove", onMove, { passive: true });
    document.documentElement.addEventListener("pointerleave", onLeave);
    canvas.addEventListener("pointerdown", onDown);
    return () => {
      window.removeEventListener("pointermove", onMove);
      document.documentElement.removeEventListener("pointerleave", onLeave);
      canvas.removeEventListener("pointerdown", onDown);
    };
  }, [interactive, reach]);

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
