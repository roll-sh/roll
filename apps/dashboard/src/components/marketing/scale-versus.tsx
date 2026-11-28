import { useReducedMotion } from "@roll/brand";
import { Moon, Zap } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { inksOf, onSchemeChange, rgba, useInView } from "./in-view";
import { NOTE } from "./scene";

/** Seconds of traffic on screen. */
const WINDOW = 44;
/** The most instances the service runs, and what the always-on fleet is sized for. */
const PEAK = 4;
/** Requests a second one instance takes. */
const PER_INSTANCE = 7;
/** Seconds an instance stays up after its last request. */
const LINGER = 3;
const BURST = 22;

/** The traffic both sides get: mostly quiet, a request now and then, a burst. It repeats. */
const TRAFFIC = [0, 0, 1, 0, 0, 0, 0, 0, 0, 3, 9, 18, 26, 14, 5, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 6, 11, 4, 0, 0, 0, 0, 0, 0, 0, 0];

type Bin = { req: number; inst: number };
type Sim = { bins: Bin[]; n: number; t: number; quiet: number; always: number; roll: number };

/** How many instances roll runs for a second with `req` requests, `quiet` seconds after the last one. */
const instancesFor = (req: number, quiet: number) => (req > 0 ? Math.min(PEAK, Math.ceil(req / PER_INSTANCE)) : quiet < LINGER ? 1 : 0);

function seed(): Sim {
  const s: Sim = { bins: [], n: 0, t: 0, quiet: LINGER, always: 0, roll: 0 };
  for (let i = 0; i < WINDOW; i++) push(s, TRAFFIC[i % TRAFFIC.length]);
  return s;
}

/** Closes the newest second (billing it on both sides) and opens the next with `req` requests. */
function push(s: Sim, req: number) {
  const last = s.bins[s.bins.length - 1];
  if (last) {
    s.always += PEAK;
    s.roll += last.inst;
  }
  s.quiet = req > 0 ? 0 : s.quiet + 1;
  s.bins.push({ req, inst: instancesFor(req, s.quiet) });
  if (s.bins.length > WINDOW) s.bins.shift();
  s.n++;
}

/**
 * The same traffic twice: on a fleet sized for its peak that never sleeps, and on roll, where an
 * instance runs only while something's asking. What each side bills is counted in instance-seconds
 * under it, so the two numbers pull apart as you watch. Requests you send go to both.
 */
export function ScaleVersus({ className }: { className?: string }) {
  const root = useRef<HTMLDivElement>(null);
  const left = useRef<HTMLCanvasElement>(null);
  const right = useRef<HTMLCanvasElement>(null);
  const sim = useRef<Sim>(seed());
  const redraw = useRef<() => void>(() => {});
  const inView = useInView(root);
  const reduced = useReducedMotion();
  const [view, setView] = useState(() => ({ running: sim.current.bins[WINDOW - 1].inst, always: sim.current.always, roll: sim.current.roll }));

  const publish = () => {
    const s = sim.current;
    setView({ running: s.bins[s.bins.length - 1].inst, always: s.always, roll: s.roll });
  };

  const send = (count: number) => {
    const s = sim.current;
    const now = s.bins[s.bins.length - 1];
    now.req += count;
    s.quiet = 0;
    now.inst = instancesFor(now.req, 0);
    publish();
    redraw.current();
  };

  useEffect(() => {
    const canvases = [left.current, right.current];
    if (!canvases[0] || !canvases[1]) return;
    let inks = inksOf(canvases[0]);
    const font = getComputedStyle(canvases[0]).fontFamily;

    const paint = (canvas: HTMLCanvasElement, always: boolean) => {
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      const dpr = Math.min(2, window.devicePixelRatio || 1);
      const r = canvas.getBoundingClientRect();
      const w = Math.round(r.width);
      const h = Math.round(r.height);
      if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
        canvas.width = w * dpr;
        canvas.height = h * dpr;
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      const s = sim.current;
      const { ink, accent } = inks;
      const pad = 20;
      const lane = 11;
      const gap = 5;
      const lanesTop = h - 30 - PEAK * (lane + gap);
      const base = lanesTop - 30;
      const top = 30;
      const pitch = (w - pad * 2) / (WINDOW - 1);
      const shift = reduced ? 0 : s.t * pitch;
      const xOf = (i: number) => pad + (i - 1) * pitch + (pitch - shift);
      const peak = Math.max(BURST + 4, ...s.bins.map((b) => b.req));

      ctx.font = `10.5px ${font}`;
      ctx.textBaseline = "middle";
      ctx.fillStyle = rgba(ink, 0.42);
      ctx.textAlign = "left";
      ctx.fillText("requests / s", pad, 14);
      ctx.fillText("instances", pad, lanesTop - 12);
      ctx.fillText(`−${WINDOW} s`, pad, h - 11);
      ctx.textAlign = "right";
      ctx.fillText("now", w - pad, h - 11);

      ctx.strokeStyle = rgba(ink, 0.12);
      ctx.beginPath();
      ctx.moveTo(pad, base + 0.5);
      ctx.lineTo(w - pad, base + 0.5);
      ctx.stroke();

      ctx.save();
      ctx.beginPath();
      ctx.rect(pad, 0, w - pad * 2, h);
      ctx.clip();
      // Each lane is drawn as whole runs, not a rectangle a second: translucent fills that overlap would show seams.
      for (let k = 0; k < PEAK; k++) {
        const y = lanesTop + k * (lane + gap);
        ctx.fillStyle = rgba(ink, 0.05);
        ctx.fillRect(pad, y, w - pad * 2, lane);
        ctx.fillStyle = rgba(ink, always ? 0.26 : 0.5);
        let from = -1;
        for (let i = 0; i <= s.bins.length; i++) {
          const on = i < s.bins.length && (always || s.bins[i].inst > k);
          if (on && from < 0) from = i;
          if (!on && from >= 0) {
            ctx.fillRect(Math.round(xOf(from)), y, Math.round(xOf(i)) - Math.round(xOf(from)), lane);
            from = -1;
          }
        }
      }
      let labelled = pad + 64;
      s.bins.forEach((b, i) => {
        const x = Math.round(xOf(i));
        const bw = Math.max(2, Math.round(pitch) - 2);
        if (b.req > 0) {
          const bh = Math.max(3, (b.req / peak) * (base - top));
          ctx.fillStyle = rgba(ink, 0.55);
          ctx.fillRect(x, base - bh, bw, bh);
        }
        const before = s.bins[i - 1];
        if (!always && b.inst > 0 && before && before.inst === 0) {
          ctx.fillStyle = rgba(accent, 1);
          ctx.fillRect(x, lanesTop - 4, 2, PEAK * (lane + gap) + 2);
          // A wake's label only where there's room for it: clear of "instances" and of the one before.
          if (x > labelled) {
            ctx.textAlign = "left";
            ctx.fillText(w < 520 ? "13 ms" : "woke · 13 ms", x + 6, lanesTop - 12);
            labelled = x + (w < 520 ? 52 : 96);
          }
        }
      });
      ctx.restore();
    };

    const draw = () => {
      paint(canvases[0]!, true);
      paint(canvases[1]!, false);
    };
    redraw.current = draw;
    draw();

    const ro = new ResizeObserver(draw);
    canvases.forEach((c) => ro.observe(c!));
    const off = onSchemeChange(() => {
      inks = inksOf(canvases[0]!);
      draw();
    });

    let raf = 0;
    let before = 0;
    const frame = (now: number) => {
      const s = sim.current;
      const dt = before ? Math.min(0.1, (now - before) / 1000) : 0;
      before = now;
      s.t += dt;
      if (s.t >= 1) {
        s.t -= 1;
        push(s, TRAFFIC[s.n % TRAFFIC.length]);
        publish();
      }
      draw();
      raf = requestAnimationFrame(frame);
    };
    if (inView && !reduced) raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      off?.();
    };
  }, [inView, reduced]);

  const share = Math.round((view.roll / view.always) * 100);

  return (
    <div ref={root} className={`overflow-hidden rounded-2xl border border-line bg-line ${className ?? ""}`}>
      <div className="grid grid-cols-[minmax(0,1fr)] gap-px lg:grid-cols-2">
        <Side
          title="A server that never sleeps"
          sub={`Sized for the busiest second: ${PEAK} instances, all day.`}
          status={`${PEAK} of ${PEAK} running`}
          billed={view.always}
        >
          <canvas ref={left} aria-label="The traffic on four instances that are always running" className="absolute inset-0 block size-full font-mono text-fg" />
        </Side>
        <Side
          own
          title="The same traffic, on roll"
          sub="An instance runs while something's asking, and no longer."
          status={view.running ? `${view.running} of ${PEAK} running` : "asleep · nothing billed"}
          asleep={view.running === 0}
          billed={view.roll}
        >
          <canvas ref={right} aria-label="The same traffic on roll: instances wake for requests and sleep between them" className="absolute inset-0 block size-full font-mono text-fg" />
        </Side>
      </div>
      <div className="mt-px flex flex-wrap items-center gap-x-6 gap-y-3 bg-bg px-5 py-4 sm:px-6">
        <p className="text-[14px] text-muted">
          So far roll has billed <span className="font-mono text-fg tabular-nums">{share}%</span> of what the always-on fleet did. Only running instances are metered.
        </p>
        <div className="ml-auto flex gap-2">
          <button onClick={() => send(1)} className="rounded-lg border border-line-2 px-3 py-1.5 text-[12px] text-fg transition hover:bg-panel-2 active:scale-[0.98]">
            Send a request
          </button>
          <button onClick={() => send(BURST)} className="inline-flex items-center gap-1.5 rounded-lg bg-fg px-3 py-1.5 text-[12px] font-medium text-bg transition hover:opacity-85 active:scale-[0.98]">
            <Zap className="size-3.5" strokeWidth={1.5} />
            Send a burst
          </button>
        </div>
      </div>
    </div>
  );
}

function Side({ title, sub, status, billed, own = false, asleep = false, children }: { title: string; sub: string; status: string; billed: number; own?: boolean; asleep?: boolean; children: React.ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col bg-bg">
      <div className="flex items-start gap-4 px-5 pt-5 sm:px-6 sm:pt-6">
        <div className="min-w-0">
          <p className={`text-[17px] font-medium tracking-[-0.02em] ${own ? "text-fg" : "text-muted"}`}>{title}</p>
          <p className="mt-1 text-[13px] text-subtle">{sub}</p>
          <p className={`mt-3 flex items-center gap-1.5 font-mono text-[11px] ${own && !asleep ? "text-accent" : "text-subtle"}`}>
            {asleep ? <Moon className="size-3" strokeWidth={1.5} /> : <span className={`size-1.5 rounded-full ${own ? "bg-accent" : "bg-subtle"}`} />}
            {status}
          </p>
        </div>
        <div className="ml-auto shrink-0 text-right">
          <p className={`font-mono text-[2.5rem] leading-none font-medium tracking-[-0.05em] tabular-nums sm:text-[3.25rem] ${own ? "text-fg" : "text-muted"}`}>{billed.toLocaleString("en-US")}</p>
          <p className={`mt-2 ${NOTE}`}>instance-seconds billed</p>
        </div>
      </div>
      <div className="relative mt-2 min-h-64 flex-1 sm:min-h-[19rem]">{children}</div>
    </div>
  );
}
