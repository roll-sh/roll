import { useReducedMotion } from "@roll/brand";
import { Zap } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { inksOf, onSchemeChange, rgba, useInView } from "./in-view";
import { Halftone, NOTE } from "./scene";

/** Seconds of history the chart shows, of quiet before the service scales to zero, and between scale-downs. */
const WINDOW = 40;
const IDLE = 5;
const COOL = 2;
const MAX = 4;
/** Requests per second one instance takes before another starts. */
const PER_INSTANCE = 7;
/** A burst: this many requests over this many seconds. */
const BURST = [44, 2] as const;
/** After the visitor pokes it, no traffic of its own for this long. */
const QUIET = 18;
/** How long a wake-up's steps stay lit beside the chart, in seconds each. */
const BEAT = 0.9;

/**
 * The chart's first 40 seconds, so it opens on the story rather than a blank: three bursts, each
 * followed by its quiet seconds and then zero instances.
 */
function past() {
  const bins = Array.from({ length: WINDOW + 1 }, () => ({ req: 0, inst: 0 }));
  const bursts: [number, number[]][] = [[2, [3, 9, 12, 5]], [14, [2, 4]], [23, [6, 16, 22, 9, 3]]];
  for (const [at, reqs] of bursts) {
    reqs.forEach((req, k) => (bins[at + k] = { req, inst: Math.min(MAX, Math.max(1, Math.ceil(req / PER_INSTANCE))) }));
    for (let k = at + reqs.length; k < Math.min(WINDOW - 4, at + reqs.length + IDLE); k++) bins[k] = { req: 0, inst: 1 };
  }
  return bins;
}

/** What the opening chart already billed, and how many of its seconds ran nothing. */
const BILLED = past().slice(1).reduce((n, b) => n + b.inst, 0);
const SLEPT = past().slice(1).filter((b) => b.inst === 0).length;

/** What a request to a sleeping service goes through, with roll apps' measured times (docs/apps/README.md, Measured). */
const WAKE = [
  { title: "A request arrives", note: "nothing is running" },
  { title: "The proxy holds it", note: "about 108 µs when the service is up" },
  { title: "The instance resumes", note: "13 ms from suspended · 360 ms cold, Node" },
  { title: "It answers", note: "and sleeps again after a quiet spell" },
];

interface View {
  instances: number;
  billed: number;
  /** Seconds one always-on instance would have run over the same time. */
  always: number;
  /** Seconds with nothing running. */
  idle: number;
  /** Which of WAKE's steps is lit: -1 asleep, 0 to 3 through a wake-up, 3 while serving. */
  stage: number;
}

/**
 * Scale to zero you can poke: requests arrive (yours, or a little traffic of its own while nobody's
 * pushing buttons), the service resumes from zero on the first one, adds instances under a burst and
 * goes back to zero after a few quiet seconds. The chart is the requests each second over the four
 * instances that could serve them, each a lane that's filled only while it runs; beside it, what
 * that bills against a server that never sleeps, and what one wake-up goes through. Canvas, paused
 * offscreen; reduced motion steps once a second.
 */
export function ScaleSim({ className }: { className?: string }) {
  const ref = useRef<HTMLCanvasElement>(null);
  const reduced = useReducedMotion();
  const inView = useInView(ref);
  const [view, setView] = useState<View>(() => ({ instances: 0, billed: BILLED, always: WINDOW, idle: SLEPT, stage: -1 }));
  const sim = useRef({
    t: 0,
    /** Requests and the most instances running, per second of history; the last entry is now. */
    bins: past(),
    binT: 0,
    instances: 0,
    lastReq: -99,
    lastScale: 0,
    billed: BILLED,
    idle: SLEPT,
    pending: [] as number[],
    poked: -99,
    nextAmbient: 2.5,
    wokeAt: -99,
  });

  const send = (count: number, over: number) => {
    const s = sim.current;
    s.poked = s.t;
    for (let i = 0; i < count; i++) s.pending.push(s.t + (over * i) / count + Math.random() * 0.05);
  };

  useEffect(() => {
    const canvas = ref.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    let inks = inksOf(canvas);
    let font = getComputedStyle(canvas).fontFamily;
    /** The bento's halftone as a canvas fill: three dots in four on, in the ink. */
    const halftone = () => {
      const tile = document.createElement("canvas");
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      tile.width = tile.height = 6 * dpr;
      const t = tile.getContext("2d")!;
      t.scale(dpr, dpr);
      t.fillStyle = rgba(inks.ink, 0.95);
      for (const [x, y] of [[0, 0], [3, 3], [3, 0]]) t.fillRect(x, y, 1.7, 1.7);
      const p = ctx.createPattern(tile, "repeat")!;
      p.setTransform(new DOMMatrix().scale(1 / dpr));
      return p;
    };
    let tone = halftone();
    let w = 0;
    let h = 0;
    let raf = 0;
    let last = 0;
    let shownAt = 0;
    const s = sim.current;

    const resize = () => {
      const r = canvas.getBoundingClientRect();
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = r.width;
      h = r.height;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    };

    const arrive = () => {
      s.bins[WINDOW].req++;
      s.lastReq = s.t;
      if (s.instances === 0) {
        // The waking proxy holds the connection while the suspended instance resumes.
        s.instances = 1;
        s.lastScale = s.t;
        s.wokeAt = s.t;
      }
    };

    const step = (dt: number) => {
      s.t += dt;
      s.binT += dt;
      while (s.binT >= 1) {
        s.binT -= 1;
        s.bins.shift();
        s.bins.push({ req: 0, inst: s.instances });
      }
      // A little traffic of its own while nobody's pushing the buttons.
      if (!reduced && s.t >= s.nextAmbient) {
        s.nextAmbient = s.t + 6 + Math.random() * 7;
        if (s.t - s.poked > QUIET) {
          const burst = Math.random() < 0.3;
          for (let i = 0, n = burst ? 14 + Math.floor(Math.random() * 14) : 1 + Math.floor(Math.random() * 4); i < n; i++) {
            s.pending.push(s.t + i * (burst ? 0.08 : 0.15 + Math.random() * 0.5));
          }
        }
      }
      s.pending = s.pending.filter((at) => (at <= s.t ? (arrive(), false) : true));

      // Scale on the last two seconds' rate: up at once, down one instance per COOL, to zero after IDLE.
      const rate = s.bins[WINDOW].req + s.bins[WINDOW - 1].req * (1 - s.binT);
      const want = Math.min(MAX, Math.max(1, Math.ceil(rate / PER_INSTANCE)));
      if (s.instances > 0) {
        if (s.t - s.lastReq > IDLE) s.instances = 0;
        else if (want > s.instances) {
          s.instances = want;
          s.lastScale = s.t;
        } else if (want < s.instances && s.t - s.lastScale > COOL) {
          s.instances--;
          s.lastScale = s.t;
        }
      }
      s.bins[WINDOW].inst = Math.max(s.bins[WINDOW].inst, s.instances);
      s.billed += s.instances * dt;
      if (s.instances === 0) s.idle += dt;
    };

    const draw = () => {
      const { ink, accent } = inks;
      ctx.clearRect(0, 0, w, h);
      const narrow = w < 520;
      const left = narrow ? 12 : 44;
      const right = w - 12;
      const lane = narrow ? 9 : 12;
      const gap = 6;
      const lanesH = MAX * (lane + gap);
      const foot = 26;
      const base = h - foot - lanesH - 22;
      const top = 30;
      const pitch = (right - left) / WINDOW;
      const peak = Math.max(12, ...s.bins.map((b) => b.req));
      // The newest bin slides in from the right as its second fills.
      const shift = reduced ? 0 : s.binT * pitch;
      const xOf = (i: number) => left + (i - 1) * pitch + (pitch - shift);
      const laneY = (k: number) => base + 22 + k * (lane + gap);

      ctx.font = `10.5px ${font}`;
      ctx.textBaseline = "middle";
      ctx.textAlign = "left";
      ctx.fillStyle = rgba(ink, 0.45);
      ctx.fillText("requests / s", left, 14);
      ctx.fillText("instances", left, base + 11);

      // The requests' scale, and the line they stand on.
      ctx.lineWidth = 1;
      for (const f of [0.25, 0.5, 0.75, 1]) {
        const y = Math.round(base - (base - top) * f) + 0.5;
        ctx.strokeStyle = rgba(ink, 0.06);
        ctx.beginPath();
        ctx.moveTo(left, y);
        ctx.lineTo(right, y);
        ctx.stroke();
        if (!narrow && f >= 0.5) {
          ctx.fillStyle = rgba(ink, 0.32);
          ctx.textAlign = "right";
          ctx.fillText(String(Math.round(peak * f)), left - 10, y);
        }
      }
      ctx.strokeStyle = rgba(ink, 0.16);
      ctx.beginPath();
      ctx.moveTo(left, base + 0.5);
      ctx.lineTo(right, base + 0.5);
      ctx.stroke();

      // Each instance's lane: dotted while it's asleep, filled for every second it ran.
      ctx.setLineDash([1, 4]);
      for (let k = 0; k < MAX; k++) {
        const y = Math.round(laneY(k) + lane / 2) + 0.5;
        ctx.strokeStyle = rgba(ink, 0.16);
        ctx.beginPath();
        ctx.moveTo(left, y);
        ctx.lineTo(right, y);
        ctx.stroke();
        if (!narrow) {
          ctx.fillStyle = rgba(ink, k < s.instances ? 0.8 : 0.32);
          ctx.textAlign = "right";
          ctx.fillText(`#${k + 1}`, left - 10, y);
        }
      }
      ctx.setLineDash([]);

      ctx.save();
      ctx.beginPath();
      ctx.rect(left, 0, right - left, h);
      ctx.clip();
      let zeroFrom = -1;
      s.bins.forEach((b, i) => {
        const x = xOf(i);
        const live = i === WINDOW;
        if (b.req > 0) {
          const bh = Math.max(2, ((base - top) * b.req) / peak);
          const bx = Math.round(x - pitch * 0.68);
          const bw = Math.round(pitch * 0.68);
          ctx.fillStyle = live ? rgba(accent, 0.9) : rgba(ink, 0.1);
          ctx.fillRect(bx, base - bh, bw, bh);
          if (!live) {
            ctx.fillStyle = tone;
            ctx.fillRect(bx, base - bh, bw, bh);
            ctx.strokeStyle = rgba(ink, 0.4);
            ctx.strokeRect(bx + 0.5, base - bh + 0.5, bw - 1, bh);
          }
        }
        // Instances that ran during that second: compute, billed. Nothing drawn is nothing billed.
        for (let k = 0; k < b.inst; k++) {
          ctx.fillStyle = live ? rgba(accent, 0.8) : rgba(ink, k === 0 ? 0.42 : 0.3);
          ctx.fillRect(Math.round(x - pitch), laneY(k), Math.round(x) - Math.round(x - pitch), lane);
        }
        // Where it came back from zero: the wake-up, marked.
        const before = i > 0 ? s.bins[i - 1].inst : 0;
        if (b.inst > 0 && before === 0 && i > 0) {
          ctx.strokeStyle = rgba(accent, 0.7);
          ctx.beginPath();
          ctx.moveTo(Math.round(x - pitch) - 0.5, base + 4);
          ctx.lineTo(Math.round(x - pitch) - 0.5, laneY(0) + lane);
          ctx.stroke();
          if (x - pitch > left + 76) {
            ctx.fillStyle = rgba(accent, 0.95);
            ctx.textAlign = "left";
            ctx.fillText(narrow ? "13 ms" : "woke · 13 ms", x - pitch + 5, base + 11);
          }
        }
        // A quiet stretch with nothing running says so.
        if (b.inst === 0 && zeroFrom < 0) zeroFrom = i;
        if ((b.inst > 0 || i === WINDOW) && zeroFrom >= 0) {
          const to = b.inst > 0 ? i - 1 : i;
          if (to - zeroFrom >= 4) {
            ctx.fillStyle = rgba(ink, 0.34);
            ctx.textAlign = "center";
            ctx.fillText(to - zeroFrom >= 7 && !narrow ? "asleep · nothing billed" : "asleep", (xOf(zeroFrom) - pitch + xOf(to)) / 2, laneY(2) + lane / 2);
          }
          zeroFrom = -1;
        }
      });
      ctx.restore();

      ctx.fillStyle = rgba(ink, 0.4);
      ctx.textAlign = "left";
      ctx.fillText(`−${WINDOW} s`, left, h - 10);
      ctx.textAlign = "right";
      ctx.fillText("now", right, h - 10);
    };

    const publish = (now: number) => {
      if (now - shownAt < 100) return;
      shownAt = now;
      const since = s.t - s.wokeAt;
      const stage = s.instances === 0 ? -1 : since < BEAT * 3 ? Math.floor(since / BEAT) : 3;
      const always = WINDOW + s.t;
      setView((v) =>
        v.instances === s.instances && Math.abs(v.billed - s.billed) < 0.05 && Math.abs(v.always - always) < 0.1 && v.stage === stage
          ? v
          : { instances: s.instances, billed: s.billed, always, idle: s.idle, stage },
      );
    };

    const frame = (now: number) => {
      const dt = last ? Math.min(0.05, (now - last) / 1000) : 0;
      last = now;
      step(dt);
      draw();
      publish(now);
      raf = requestAnimationFrame(frame);
    };

    resize();
    const ro = new ResizeObserver(() => {
      resize();
      draw();
    });
    ro.observe(canvas);
    const off = onSchemeChange(() => {
      inks = inksOf(canvas);
      font = getComputedStyle(canvas).fontFamily;
      tone = halftone();
      draw();
    });
    let tick: ReturnType<typeof setInterval> | undefined;
    if (reduced && inView) {
      // No smooth motion: the chart moves a whole second at a time.
      draw();
      tick = setInterval(() => {
        for (let i = 0; i < 10; i++) step(0.1);
        draw();
        publish(performance.now() + 1000);
      }, 1000);
    } else if (inView && !reduced) raf = requestAnimationFrame(frame);
    else draw();
    return () => {
      cancelAnimationFrame(raf);
      clearInterval(tick);
      ro.disconnect();
      off();
    };
  }, [reduced, inView]);

  const running = view.instances > 0;
  const share = Math.min(1, view.billed / view.always);
  const asleep = Math.round((view.idle / view.always) * 100);
  return (
    <div className={`grid grid-cols-[minmax(0,1fr)] gap-px bg-line lg:grid-cols-[minmax(0,1fr)_22rem] ${className ?? ""}`}>
      <div className="flex min-w-0 flex-col bg-bg">
        <div className="flex flex-wrap items-center gap-x-5 gap-y-2 border-b border-line px-5 py-3 font-mono text-[11px]">
          <span className="text-fg">
            web <span className="text-subtle">· production</span>
          </span>
          <span className={`flex items-center gap-1.5 ${running ? "text-accent" : "text-subtle"}`}>
            <span className={`size-1.5 rounded-full ${running ? "bg-accent" : "bg-subtle"}`} />
            {running ? `${view.instances} of ${MAX} running` : "0 running · asleep"}
          </span>
          <div className="ml-auto flex gap-2 font-sans">
            <button onClick={() => send(1, 0)} className="rounded-lg border border-line-2 px-3 py-1.5 text-[12px] text-fg transition hover:bg-panel-2 active:scale-[0.98]">
              Send a request
            </button>
            <button
              onClick={() => send(...BURST)}
              className="inline-flex items-center gap-1.5 rounded-lg bg-fg px-3 py-1.5 text-[12px] font-medium text-bg transition hover:opacity-85 active:scale-[0.98]"
            >
              <Zap className="size-3.5" strokeWidth={1.5} />
              Send a burst
            </button>
          </div>
        </div>
        <div className="relative min-h-72 flex-1 sm:min-h-[23rem]">
          <canvas
            ref={ref}
            aria-label="Requests per second over the last 40 seconds, and which of four instances ran in each"
            className="absolute inset-0 block size-full font-mono text-fg"
          />
        </div>
      </div>

      <div className="flex flex-col bg-bg">
        <div className="p-5 sm:p-6">
          <p className={NOTE}>Compute billed</p>
          <div aria-live="off" className="mt-4 space-y-3.5">
            <Meter name="this service" seconds={view.billed} share={share} own />
            <Meter name="one server, always on" seconds={view.always} share={1} />
          </div>
          <p className="mt-4 text-[12.5px] leading-relaxed text-muted text-pretty">
            Asleep for <span className="text-fg tabular-nums">{asleep}%</span> of that time. Only a running instance is metered, so the gaps cost
            nothing.
          </p>
        </div>
        <div className="flex-1 border-t border-line p-5 sm:p-6">
          <p className={NOTE}>One wake-up</p>
          <ol className="mt-4 space-y-3">
            {WAKE.map((step, i) => {
              const on = view.stage === i;
              const done = view.stage > i;
              return (
                <li key={step.title} className="flex gap-3">
                  <span
                    className={`mt-[5px] size-2 shrink-0 rounded-full border transition-colors duration-300 ${
                      on ? "border-accent bg-accent" : done ? "border-fg/70 bg-fg/70" : "border-line-2"
                    }`}
                  />
                  <span>
                    <span className={`block text-[13.5px] transition-colors duration-300 ${on || done ? "text-fg" : "text-muted"}`}>{step.title}</span>
                    <span className="block font-mono text-[11px] text-subtle">{step.note}</span>
                  </span>
                </li>
              );
            })}
          </ol>
        </div>
      </div>
    </div>
  );
}

/** A meter of compute-seconds: its bar is its share of the always-on server's. */
function Meter({ name, seconds, share, own = false }: { name: string; seconds: number; share: number; own?: boolean }) {
  return (
    <div>
      <div className="flex items-baseline justify-between gap-3 font-mono text-[11px]">
        <span className={own ? "text-fg" : "text-subtle"}>{name}</span>
        <span className={`tabular-nums ${own ? "text-fg" : "text-muted"}`}>{seconds.toFixed(1)} s</span>
      </div>
      <div className="mt-1.5 h-3 overflow-hidden rounded-[3px] border border-line">
        <div className={`h-full ${own ? "bg-fg/10 text-fg" : "text-muted"}`} style={{ width: `${Math.max(1, share * 100)}%` }}>
          <Halftone level={own ? 75 : 25} />
        </div>
      </div>
    </div>
  );
}
