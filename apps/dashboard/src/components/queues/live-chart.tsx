"use client";

import { resolveColor, useReducedMotion } from "@roll/brand";
import { useEffect, useRef, useState, type PointerEvent } from "react";
import { formatRate } from "@/lib/queues-api";

type Tone = "fg" | "muted" | "subtle";

export interface ChartSeries<P> {
  key: keyof P & string;
  label: string;
  tone: Tone;
  dashed?: boolean;
  /** Flat, faint area under the line (first series only reads well). */
  fill?: boolean;
}

const toneClass: Record<Tone, string> = { fg: "bg-fg", muted: "bg-muted", subtle: "bg-subtle" };

type RGB = [number, number, number];
const rgba = (c: RGB, a = 1) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

/** Round up to 1/2/4/6/8 × 10ⁿ so the max and its half are both readable (and whole for counts). */
function niceMax(v: number) {
  if (!(v > 0)) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 4, 6, 8, 10]) if (v <= m * p) return m * p;
  return 10 * p;
}

function clock(t: number) {
  return new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

/**
 * A calm, live line chart drawn on canvas. New samples slide in from the right with an ease-out,
 * the y-scale eases to fit, and the newest point carries a single accent "live" dot. Pauses
 * offscreen; with reduced motion it draws only the final frame whenever data changes.
 */
export function LiveChart<P extends { t: number }>({
  points,
  series,
  windowS = 300,
  height = 168,
  format = formatRate,
  live = true,
  empty = "Waiting for samples…",
  whole = false,
}: {
  points: readonly P[];
  series: ChartSeries<P>[];
  windowS?: number;
  height?: number;
  format?: (n: number) => string;
  live?: boolean;
  empty?: string;
  /** Values are counts: keep the axis max even so its half is a whole number too. */
  whole?: boolean;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const reduced = useReducedMotion();
  const [hover, setHover] = useState<number | null>(null);

  // Everything the frame loop reads lives in refs so new data never restarts the loop.
  const dataRef = useRef({ points, series, format, live, hover: null as number | null });
  const redrawRef = useRef<() => void>(() => {});
  useEffect(() => {
    dataRef.current = { points, series, format, live, hover };
    redrawRef.current();
  }, [points, series, format, live, hover]);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;

    let w = 0;
    let h = 0;
    let dpr = 1;
    let raf = 0;
    let frame = 0;
    let visible = true;
    let last = performance.now();
    const view = { end: 0, max: 0 };
    let colors: Record<"fg" | "muted" | "subtle" | "line" | "accent" | "panel", RGB>;
    let font = "10px monospace";

    const readColors = () => {
      const cs = getComputedStyle(canvas);
      const v = (name: string) => resolveColor(cs.getPropertyValue(name).trim() || cs.color);
      colors = {
        fg: v("--fg"),
        muted: v("--muted"),
        subtle: v("--subtle"),
        line: v("--line"),
        accent: v("--accent"),
        panel: v("--panel"),
      };
      font = `10px ${cs.fontFamily}`;
    };
    readColors();

    const resize = () => {
      const rect = canvas.getBoundingClientRect();
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = rect.width;
      h = rect.height;
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
      draw(performance.now(), true);
    };

    const PAD_T = 14;
    const PAD_B = 6;
    const PAD_R = 10;

    const draw = (now: number, snap = false) => {
      const { points: pts, series: ser, format: fmt, live: isLive, hover: hv } = dataRef.current;
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      if (++frame % 90 === 0) readColors();

      const lastT = pts.length ? pts[pts.length - 1].t : Date.now() / 1000;
      let peak = 0;
      for (const p of pts) {
        if (p.t < lastT - windowS) continue;
        for (const s of ser) peak = Math.max(peak, Number(p[s.key]) || 0);
      }
      const target = { end: lastT, max: whole ? Math.max(2, niceMax(peak * 1.1)) : niceMax(peak * 1.1) };
      const farOff = view.max < target.max / 3 || view.max > target.max * 3;
      if (snap || reduced || farOff || Math.abs(target.end - view.end) > windowS / 2) {
        view.end = target.end;
        view.max = target.max;
      } else {
        // Ease-out toward the target; ~250ms time constant.
        const k = 1 - Math.exp(-dt / 0.25);
        view.end += (target.end - view.end) * k;
        view.max += (target.max - view.max) * k;
      }

      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const plotW = w - PAD_R;
      const plotH = h - PAD_T - PAD_B;
      const x = (t: number) => ((t - (view.end - windowS)) / windowS) * plotW;
      const y = (v: number) => PAD_T + plotH - (Math.max(0, v) / view.max) * plotH;

      // Gridlines with their values.
      ctx.font = font;
      ctx.textBaseline = "bottom";
      ctx.lineWidth = 1;
      for (const f of [0, 0.5, 1]) {
        const gy = Math.round(y(view.max * f)) + 0.5;
        ctx.strokeStyle = rgba(colors.line);
        ctx.beginPath();
        ctx.moveTo(0, gy);
        ctx.lineTo(w, gy);
        ctx.stroke();
        if (f > 0) {
          ctx.fillStyle = rgba(colors.subtle);
          ctx.fillText(fmt(target.max * f), 0, gy - 2);
        }
      }

      if (pts.length) {
        ctx.save();
        ctx.beginPath();
        ctx.rect(0, 0, plotW + PAD_R, h);
        ctx.clip();
        ctx.lineJoin = "round";
        ctx.lineCap = "round";
        // Draw back-to-front so the first series sits on top.
        for (let si = ser.length - 1; si >= 0; si--) {
          const s = ser[si];
          const c = colors[s.tone];
          ctx.beginPath();
          let started = false;
          let firstX = 0;
          let lastX = 0;
          for (const p of pts) {
            const px = x(p.t);
            if (px < -plotW * 0.1) continue;
            const py = y(Number(p[s.key]) || 0);
            if (!started) {
              ctx.moveTo(px, py);
              firstX = px;
              started = true;
            } else ctx.lineTo(px, py);
            lastX = px;
          }
          if (!started) continue;
          if (s.fill) {
            ctx.save();
            ctx.lineTo(lastX, y(0));
            ctx.lineTo(firstX, y(0));
            ctx.closePath();
            ctx.fillStyle = rgba(c, 0.07);
            ctx.fill();
            ctx.restore();
            // Re-trace the line without the closing edges.
            ctx.beginPath();
            let first = true;
            for (const p of pts) {
              const px = x(p.t);
              if (px < -plotW * 0.1) continue;
              const py = y(Number(p[s.key]) || 0);
              if (first) {
                ctx.moveTo(px, py);
                first = false;
              } else ctx.lineTo(px, py);
            }
          }
          ctx.setLineDash(s.dashed ? [3, 3] : []);
          ctx.strokeStyle = rgba(c, s.tone === "fg" ? 0.9 : 1);
          ctx.lineWidth = 1.5;
          ctx.stroke();
          ctx.setLineDash([]);
        }

        // Hover crosshair.
        if (hv !== null && pts[hv]) {
          const hx = Math.round(x(pts[hv].t)) + 0.5;
          ctx.strokeStyle = rgba(colors.muted, 0.5);
          ctx.lineWidth = 1;
          ctx.beginPath();
          ctx.moveTo(hx, PAD_T - 4);
          ctx.lineTo(hx, h - PAD_B);
          ctx.stroke();
          for (const s of ser) {
            ctx.beginPath();
            ctx.arc(hx, y(Number(pts[hv][s.key]) || 0), 2.5, 0, Math.PI * 2);
            ctx.fillStyle = rgba(colors.panel);
            ctx.fill();
            ctx.strokeStyle = rgba(colors[s.tone]);
            ctx.lineWidth = 1.5;
            ctx.stroke();
          }
        }

        // The single accent: the live head of the first series.
        if (isLive && ser[0]) {
          const p = pts[pts.length - 1];
          const hx = x(p.t);
          const hy = y(Number(p[ser[0].key]) || 0);
          if (!reduced) {
            const phase = (now % 2400) / 2400;
            const ease = 1 - (1 - phase) ** 3;
            ctx.beginPath();
            ctx.arc(hx, hy, 2.5 + ease * 7, 0, Math.PI * 2);
            ctx.fillStyle = rgba(colors.accent, (1 - ease) * 0.28);
            ctx.fill();
          }
          ctx.beginPath();
          ctx.arc(hx, hy, 2.5, 0, Math.PI * 2);
          ctx.fillStyle = rgba(colors.accent);
          ctx.fill();
        }
        ctx.restore();
      }
    };

    const loop = (now: number) => {
      draw(now);
      raf = visible && !reduced ? requestAnimationFrame(loop) : 0;
    };
    const start = () => {
      if (!raf && visible && !reduced) {
        last = performance.now();
        raf = requestAnimationFrame(loop);
      }
    };
    redrawRef.current = () => {
      if (reduced || !visible) draw(performance.now(), true);
    };

    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    const io = new IntersectionObserver(([entry]) => {
      visible = entry.isIntersecting;
      if (visible) start();
      else {
        cancelAnimationFrame(raf);
        raf = 0;
      }
    });
    io.observe(canvas);
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onTheme = () => {
      readColors();
      draw(performance.now(), true);
    };
    mq.addEventListener("change", onTheme);
    resize();
    start();
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      io.disconnect();
      mq.removeEventListener("change", onTheme);
      redrawRef.current = () => {};
    };
  }, [reduced, windowS, whole]);

  const onMove = (e: PointerEvent<HTMLCanvasElement>) => {
    if (!points.length) return;
    const rect = e.currentTarget.getBoundingClientRect();
    const plotW = rect.width - 10;
    const end = points[points.length - 1].t;
    const t = end - windowS + ((e.clientX - rect.left) / plotW) * windowS;
    let best = 0;
    for (let i = 1; i < points.length; i++) if (Math.abs(points[i].t - t) < Math.abs(points[best].t - t)) best = i;
    setHover(best);
  };

  const shown = hover !== null && points[hover] ? points[hover] : points[points.length - 1];

  return (
    <div className="min-w-0">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 px-5 pt-4">
        {series.map((s) => (
          <span key={s.key} className="inline-flex items-center gap-1.5 text-[12px] text-muted">
            {s.dashed ? (
              <span className={`inline-block w-3 border-t border-dashed ${s.tone === "fg" ? "border-fg" : s.tone === "muted" ? "border-muted" : "border-subtle"}`} />
            ) : (
              <span className={`inline-block h-[1.5px] w-3 rounded-full ${toneClass[s.tone]}`} />
            )}
            {s.label}
            <span className="font-mono text-fg tabular-nums">{shown ? format(Number(shown[s.key]) || 0) : "—"}</span>
          </span>
        ))}
        <span className="ml-auto font-mono text-[11px] text-subtle">
          {hover !== null && shown ? clock(shown.t) : live ? "live" : ""}
        </span>
      </div>
      <div className="relative px-5 pt-2 pb-1">
        <canvas
          ref={canvasRef}
          aria-label={`Chart of ${series.map((s) => s.label).join(", ")}`}
          role="img"
          onPointerMove={onMove}
          onPointerLeave={() => setHover(null)}
          className="block w-full font-mono"
          style={{ height }}
        />
        {!points.length && (
          <p className="pointer-events-none absolute inset-0 grid place-items-center text-[12px] text-subtle">
            <span className="shimmer">{empty}</span>
          </p>
        )}
      </div>
      <div className="flex justify-between px-5 pb-3 font-mono text-[10px] text-subtle">
        <span>{windowS >= 120 ? `${Math.round(windowS / 60)} min ago` : `${windowS}s ago`}</span>
        <span>now</span>
      </div>
    </div>
  );
}
