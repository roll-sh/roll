import { useReducedMotion } from "@roll/brand";
import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { productBySlug } from "@/lib/marketing";
import { inksOf, onSchemeChange, rgba, useInView, type Rgb } from "./in-view";
import { NOTE } from "./scene";

/**
 * What each product's branch is, as its page says it: whether it branches from a point in its
 * history or only from now, how long it took where that's measured, and what it costs.
 */
interface Kind {
  id: string;
  name: string;
  parent: string;
  /** Branches from any point in its history (Postgres, Redis, Search); the rest branch from now. */
  pointInTime: boolean;
  /** Measured time to a ready branch, where a doc gives one. */
  ms?: number;
  note: string;
  caption: string;
  /** How it branches, in a few words, for the list. */
  how: string;
  /** The command that makes the branch (`roll …` as the CLI takes it); `at` is the moment, when it's not now. */
  cmd: (name: string, at: Date | null) => string;
}

/** A moment as each CLI's `--at` takes it: RFC 3339, or how long ago. */
const rfc3339 = (at: Date) => at.toISOString().replace(/\.\d+Z$/, "Z");
const ago = (at: Date) => `${Math.max(1, Math.round((Date.now() - at.getTime()) / 1000))}s`;

const KINDS: Kind[] = [
  {
    id: "postgres",
    name: "Postgres",
    parent: "shop",
    pointInTime: true,
    ms: 205,
    note: "own endpoint · wakes on connect",
    how: "from any point in its history",
    cmd: (name, at) => `roll db branch shop ${name}${at ? ` --at ${rfc3339(at)}` : ""}`,
    caption: "Postgres branches from now or any point in its history. 205 ms is a branch from a suspended parent, measured locally (README.md).",
  },
  {
    id: "redis",
    name: "Redis",
    parent: "cache",
    pointInTime: true,
    note: "0 B copied",
    how: "from any second it keeps",
    cmd: (name, at) => `roll redis branch cache ${name}${at ? ` --at ${ago(at)}` : ""}`,
    caption: "Redis branches from any second in its retention window, without copying the data.",
  },
  {
    id: "nosql",
    name: "NoSQL",
    parent: "app",
    pointInTime: false,
    note: "one manifest write",
    how: "one manifest write",
    cmd: (name) => `roll nosql branch app --name ${name}`,
    caption: "A NoSQL branch is one manifest write. Reset it, restore it, or mask its data for a preview.",
  },
  {
    id: "search",
    name: "Search",
    parent: "app",
    pointInTime: true,
    note: "production untouched",
    how: "now, or as it was",
    cmd: (name, at) => `roll search clusters branch app ${name}${at ? ` --at ${ago(at)}` : ""}`,
    caption: "Branch a search cluster now or as it was, to try a mapping or a reindex without touching production.",
  },
  {
    id: "storage",
    name: "Storage",
    parent: "assets",
    pointInTime: false,
    note: "0 B copied · copy-on-write",
    how: "nothing copied until it changes",
    cmd: (name) => `roll storage branch assets ${name}`,
    caption: "A bucket branch copies nothing until an object changes.",
  },
  {
    id: "streams",
    name: "Streams",
    parent: "orders",
    pointInTime: false,
    note: "shares its data",
    how: "shares its records",
    cmd: (name) => `roll stream branch orders ${name}`,
    caption: "A stream branch replays production traffic into a test, sharing its data instead of copying it.",
  },
];

const NAMES = ["dev", "test-1240", "agent-fix-login", "feature-x", "pr-482", "preview-7"];

/** History on screen, in seconds, and how much history passes per real second. */
const WINDOW = 600;
const SPEED = 10;
/** One possible write every SLOT seconds of history. */
const SLOT = 9;
const MAX = 4;
const PAD = 20;
const MAIN_Y = 58;
/** Room under the last lane for the time axis. */
const AXIS = 34;
/** Minutes of history marked on the axis. */
const MARKS = [10, 8, 6, 4, 2, 0];
const GROW_MS = 450;
/** A branch without a measured time still takes a beat to draw in. */
const BEAT_MS = 300;

interface Branch {
  id: number;
  name: string;
  /** Where it forked, in history seconds, and the history time it was made at. */
  forkH: number;
  createdH: number;
  /** performance.now() at creation and, once it's folding away, at that. */
  created: number;
  leaving?: number;
  readyMs: number;
  forkLabel: string;
  /** The moment it forked from, or null for now. */
  at: Date | null;
}

const easeOut = (t: number) => 1 - (1 - t) ** 3;
const clamp = (t: number) => Math.min(1, Math.max(0, t));
const hash = (n: number) => {
  const x = Math.sin(n * 127.1 + 311.7) * 43758.5453;
  return x - Math.floor(x);
};
const mix = (a: Rgb, b: Rgb, t: number): Rgb => [0, 1, 2].map((k) => Math.round(a[k] + (b[k] - a[k]) * t)) as Rgb;
/** Lanes share the plot's height, so four branches fill it whatever its size. */
const laneOf = (h: number) => Math.max(34, (h - MAIN_Y - AXIS) / MAX);

const clock = (msAgo: number) => {
  const d = new Date(Date.now() - msAgo);
  const p = (n: number, l = 2) => String(n).padStart(l, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
};

/**
 * Point-in-time branching you can do yourself: main's writes arrive on the right and roll left;
 * hover to pick a moment, click (or Enter) to fork a branch from it. A branch peels off to its own
 * lane, says how fast it was ready, and takes writes of its own. At most four; the oldest folds away.
 * Products that branch only from now pin the scrubber to now.
 */
export function BranchTimeline({ product, className }: { product?: string; className?: string }) {
  const plotRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const reduced = useReducedMotion();
  const inView = useInView(plotRef);
  const [kind, setKind] = useState(() => KINDS.find((k) => k.id === product) ?? KINDS[0]);
  const [branches, setBranches] = useState<Branch[]>([]);
  /** The scrubber, as a fraction of the plot from its left (1 is now), or null when hidden. */
  const [scrub, setScrub] = useState<number | null>(null);
  const [announce, setAnnounce] = useState("");
  const [lane, setLane] = useState(52);
  const laneRef = useRef(lane);
  const laneY = (i: number) => MAIN_Y + lane * (i + 1);
  const branchesRef = useRef(branches);
  const t0 = useRef(0);
  const seq = useRef(0);
  const timers = useRef<ReturnType<typeof setTimeout>[]>([]);
  const redraw = useRef<() => void>(() => {});

  const nowH = () => (reduced ? WINDOW : WINDOW + ((performance.now() - t0.current) / 1000) * SPEED);

  useEffect(() => {
    t0.current = performance.now();
    const pending = timers.current;
    return () => pending.forEach(clearTimeout);
  }, []);

  useEffect(() => {
    branchesRef.current = branches;
    redraw.current();
  }, [branches]);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    let inks = inksOf(canvas);
    let w = 0;
    let h = 0;
    let raf = 0;
    const ys = new Map<number, number>();

    const resize = () => {
      const r = canvas.getBoundingClientRect();
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      w = r.width;
      h = r.height;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      laneRef.current = laneOf(h);
      setLane(laneRef.current);
    };
    const laneY = (i: number) => MAIN_Y + laneRef.current * (i + 1);

    const dots = (seed: number, fromH: number, toH: number, y: number, minX: number, xOf: (t: number) => number, alpha: number) => {
      const { ink } = inks;
      for (let n = Math.floor(fromH / SLOT); n <= Math.floor(toH / SLOT); n++) {
        const k = n + seed * 1000;
        if (hash(k) < 0.42) continue;
        const tH = n * SLOT + hash(k + 0.3) * SLOT * 0.6;
        if (tH < fromH || tH > toH) continue;
        const x = xOf(tH);
        if (x < minX) continue;
        const fade = 0.3 + 0.6 * clamp((x - PAD) / (w - 2 * PAD));
        ctx.fillStyle = rgba(ink, fade * alpha);
        ctx.beginPath();
        ctx.arc(x, y, 1.6 + hash(k + 0.7) * 1.6, 0, Math.PI * 2);
        ctx.fill();
      }
    };

    const draw = (ms: number) => {
      const { ink, accent } = inks;
      const now = nowH();
      const L = PAD;
      const R = w - PAD;
      const xOf = (tH: number) => R - ((now - tH) / WINDOW) * (R - L);
      ctx.clearRect(0, 0, w, h);

      // The axis: a faint rule at every other minute of history, and the lanes branches take.
      ctx.lineWidth = 1;
      for (const m of MARKS) {
        const x = Math.round(R - (m / 10) * (R - L)) + 0.5;
        ctx.strokeStyle = rgba(ink, 0.07);
        ctx.beginPath();
        ctx.moveTo(x, 14);
        ctx.lineTo(x, h - AXIS + 8);
        ctx.stroke();
      }
      ctx.setLineDash([1, 5]);
      ctx.strokeStyle = rgba(ink, 0.12);
      for (let i = 0; i < MAX; i++) {
        ctx.beginPath();
        ctx.moveTo(L, laneY(i) + 0.5);
        ctx.lineTo(R, laneY(i) + 0.5);
        ctx.stroke();
      }
      ctx.setLineDash([]);

      ctx.strokeStyle = rgba(ink, 0.4);
      ctx.beginPath();
      ctx.moveTo(L, MAIN_Y);
      ctx.lineTo(R, MAIN_Y);
      ctx.stroke();
      dots(0, now - WINDOW, now, MAIN_Y, L, xOf, 1);

      // The live head: main is taking writes right now.
      const pulse = reduced ? 0 : ((ms / 1000) % 2.2) / 2.2;
      ctx.strokeStyle = rgba(accent, 0.5 * (1 - pulse));
      ctx.beginPath();
      ctx.arc(R, MAIN_Y, 4 + 9 * easeOut(pulse), 0, Math.PI * 2);
      ctx.stroke();
      ctx.fillStyle = rgba(accent, 1);
      ctx.beginPath();
      ctx.arc(R, MAIN_Y, 3, 0, Math.PI * 2);
      ctx.fill();

      const live = branchesRef.current.filter((b) => b.leaving === undefined);
      for (const b of branchesRef.current) {
        // Lanes close up when one folds away; the one leaving stays where it was while it fades.
        const prev = ys.get(b.id) ?? laneY(Math.max(0, live.indexOf(b)));
        const target = b.leaving === undefined ? laneY(live.indexOf(b)) : prev;
        const ly = reduced ? target : prev + (target - prev) * 0.18;
        ys.set(b.id, ly);
        const alpha = b.leaving === undefined ? 1 : 1 - clamp((ms - b.leaving) / 250);
        if (alpha <= 0) continue;
        const age = ms - b.created;
        const grow = reduced ? 1 : easeOut(clamp(age / GROW_MS));
        // Accent while it's being made, settling to ink once it's been ready a moment.
        const heat = reduced ? 0 : 1 - clamp((age - b.readyMs) / 900);
        const fx = xOf(b.forkH);

        ctx.save();
        ctx.beginPath();
        ctx.rect(L, 0, fx + (R - fx) * grow - L, h);
        ctx.clip();
        ctx.strokeStyle = rgba(mix(ink, accent, heat), (0.4 + 0.4 * heat) * alpha);
        ctx.lineWidth = 1.25;
        ctx.beginPath();
        ctx.moveTo(fx, MAIN_Y);
        ctx.bezierCurveTo(fx + 20, MAIN_Y, fx + 12, ly, fx + 40, ly);
        ctx.lineTo(R, ly);
        ctx.stroke();
        ctx.restore();

        if (fx >= L) {
          ctx.strokeStyle = rgba(mix(ink, accent, heat), 0.8 * alpha);
          ctx.lineWidth = 1.25;
          ctx.beginPath();
          ctx.arc(fx, MAIN_Y, 3.5, 0, Math.PI * 2);
          ctx.stroke();
        }
        dots(b.id, b.createdH + (b.readyMs / 1000) * SPEED, now, ly, Math.max(L, fx + 40), xOf, alpha);
      }
    };

    const frame = (ms: number) => {
      draw(ms);
      raf = requestAnimationFrame(frame);
    };
    const still = () => draw(performance.now());
    redraw.current = () => {
      if (reduced || !inView) still();
    };

    resize();
    const ro = new ResizeObserver(() => {
      resize();
      redraw.current();
    });
    ro.observe(canvas);
    const off = onSchemeChange(() => {
      inks = inksOf(canvas);
      redraw.current();
    });
    if (reduced || !inView) still();
    else raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      off();
      redraw.current = () => {};
    };
    // nowH reads `reduced`, which is a dependency already.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reduced, inView]);

  const fraction = (clientX: number) => {
    const r = plotRef.current!.getBoundingClientRect();
    return clamp((clientX - r.left - PAD) / (r.width - 2 * PAD));
  };
  const pin = (f: number) => (kind.pointInTime ? f : 1);

  const branchAt = (f: number) => {
    const at = pin(f);
    const now = nowH();
    const agoMs = (1 - at) * WINDOW * 1000;
    const name = NAMES[seq.current % NAMES.length];
    const b: Branch = {
      id: ++seq.current,
      name,
      forkH: now - (1 - at) * WINDOW,
      createdH: now,
      created: performance.now(),
      readyMs: kind.ms ?? BEAT_MS,
      forkLabel: at === 1 ? "now" : clock(agoMs),
      at: at === 1 ? null : new Date(Date.now() - agoMs),
    };
    setBranches((list) => {
      const live = list.filter((x) => x.leaving === undefined);
      if (live.length < MAX) return [...list, b];
      const oldest = live[0];
      if (reduced) return [...list.filter((x) => x !== oldest), b];
      timers.current.push(setTimeout(() => setBranches((l) => l.filter((x) => x.id !== oldest.id)), 260));
      return [...list.map((x) => (x === oldest ? { ...x, leaving: performance.now() } : x)), b];
    });
    setAnnounce(`Branched ${name} from ${kind.parent}${at === 1 ? " now" : ` at ${b.forkLabel}`}.`);
  };

  // Open with two branches already made (one from the past where the product allows it), so the
  // timeline shows what it does before anyone clicks.
  const seeded = useRef(false);
  useEffect(() => {
    if (!inView || seeded.current) return;
    seeded.current = true;
    timers.current.push(setTimeout(() => branchAt(0.3), reduced ? 0 : 500));
    timers.current.push(setTimeout(() => branchAt(0.57), reduced ? 0 : 1500));
    timers.current.push(setTimeout(() => branchAt(0.81), reduced ? 0 : 2500));
    // branchAt reads the current product; seeding runs once, with the first.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inView]);

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      branchAt(scrub ?? 1);
    } else if (kind.pointInTime && (e.key === "ArrowLeft" || e.key === "ArrowRight")) {
      e.preventDefault();
      setScrub((s) => clamp((s ?? 1) + (e.key === "ArrowLeft" ? -0.05 : 0.05)));
    }
  };

  const choose = (k: Kind) => {
    setKind(k);
    setBranches([]);
    setScrub(null);
  };

  const live = branches.filter((b) => b.leaving === undefined);
  const at = (f: number) => `calc(${PAD}px + ${f} * (100% - ${2 * PAD}px))`;
  const newest = live[live.length - 1];

  return (
    <div className={`grid grid-cols-[minmax(0,1fr)] gap-px bg-line lg:grid-cols-[19rem_minmax(0,1fr)] ${className ?? ""}`}>
      <div className="bg-bg">
        <p className={`hidden px-6 pt-5 pb-3 lg:block ${NOTE}`}>What branches</p>
        <div className="flex gap-1.5 overflow-x-auto p-3 [scrollbar-width:none] lg:block lg:space-y-px lg:p-2 lg:pt-0">
          {KINDS.map((k) => {
            const p = productBySlug(k.id)!;
            const on = k.id === kind.id;
            return (
              <button
                key={k.id}
                onClick={() => choose(k)}
                aria-pressed={on}
                className={`flex shrink-0 items-center gap-2 rounded-full border px-3 py-1 text-left transition-colors duration-200 lg:w-full lg:items-start lg:gap-3 lg:rounded-lg lg:border-transparent lg:px-4 lg:py-3 ${
                  on ? "border-line-2 bg-panel-2 text-fg lg:bg-panel" : "border-line text-subtle hover:text-muted lg:hover:bg-panel/60"
                }`}
              >
                <p.icon className={`size-3.5 shrink-0 lg:mt-0.5 lg:size-4 ${on ? "text-fg" : "text-subtle"}`} strokeWidth={1.5} />
                <span className="min-w-0 lg:flex-1">
                  <span className={`block text-[12px] lg:text-[14px] lg:font-medium lg:tracking-[-0.01em] ${on ? "text-fg" : "lg:text-muted"}`}>{k.name}</span>
                  <span className={`hidden text-[12.5px] leading-snug lg:mt-0.5 lg:block ${on ? "text-muted" : "text-subtle"}`}>{k.how}</span>
                </span>
                {k.ms && <span className={`hidden font-mono text-[11px] tabular-nums lg:mt-0.5 lg:block ${on ? "text-fg" : "text-subtle"}`}>{k.ms} ms</span>}
              </button>
            );
          })}
        </div>
      </div>

      <div className="flex min-w-0 flex-col bg-bg">
        <div className="flex flex-wrap items-center gap-x-5 gap-y-1 border-b border-line px-5 py-3 font-mono text-[11px] text-subtle">
          <span className="text-fg">
            {kind.parent} <span className="text-subtle">· main</span>
          </span>
          <span className="flex items-center gap-1.5 max-sm:hidden">
            <span className="size-1.5 rounded-full bg-fg/70" /> a write
          </span>
          <span className="flex items-center gap-1.5 max-sm:hidden">
            <span className="size-2 rounded-full border border-fg/70" /> where a branch forked
          </span>
          <span className="ml-auto tabular-nums">
            <span className="text-fg">{live.length}</span> of {MAX} branches
          </span>
        </div>

        <div
          ref={plotRef}
          tabIndex={0}
          role="button"
          aria-label={
            kind.pointInTime
              ? `Branch ${kind.parent}: Enter branches at the scrubber, arrow keys move it back in time.`
              : `Branch ${kind.parent} from now: press Enter.`
          }
          onPointerMove={(e: PointerEvent) => e.pointerType === "mouse" && setScrub(pin(fraction(e.clientX)))}
          onPointerLeave={() => setScrub(null)}
          onClick={(e) => branchAt(fraction(e.clientX))}
          onKeyDown={onKey}
          onFocus={() => setScrub((s) => s ?? 1)}
          onBlur={() => setScrub(null)}
          className="relative h-[19rem] cursor-crosshair text-fg outline-none select-none focus-visible:ring-1 focus-visible:ring-accent/60 sm:h-[22rem]"
        >
          <canvas ref={canvasRef} aria-hidden className="absolute inset-0 block h-full w-full" />

          <span className="absolute font-mono text-[11px] text-muted" style={{ left: PAD, top: MAIN_Y - 26 }}>
            main <span className="text-subtle">· taking writes</span>
          </span>

          {scrub !== null && (
            <div className="pointer-events-none absolute inset-y-3" style={{ left: at(scrub) }}>
              <div className="absolute inset-y-0 w-px bg-accent/50" />
              <span
                className={`absolute top-0 rounded-md border border-line-2 bg-panel px-2 py-0.5 font-mono text-[11px] whitespace-nowrap text-fg ${
                  scrub > 0.6 ? "right-2" : "left-2"
                }`}
              >
                branch from <span className="text-accent">{scrub >= 0.999 ? "now" : clock((1 - scrub) * WINDOW * 1000)}</span>
              </span>
            </div>
          )}

          {branches.map((b) => {
            const i = b.leaving === undefined ? live.indexOf(b) : -1;
            return (
              <div
                key={b.id}
                className="pointer-events-none absolute flex max-w-[calc(100%-2.5rem)] items-center gap-2 rounded-md bg-bg px-1.5 py-0.5 font-mono text-[11px] whitespace-nowrap transition-[top,opacity] duration-250 ease-out"
                style={{ right: PAD - 6, top: laneY(Math.max(i, 0)) - 25, opacity: i < 0 ? 0 : 1 }}
              >
                <span className="text-fg">{b.name}</span>
                <Ready branch={b} kind={kind} reduced={reduced} />
                <span className="hidden truncate text-subtle md:inline">{kind.note}</span>
              </div>
            );
          })}

          {live.length < MAX && (
            <p className="pointer-events-none absolute font-mono text-[11px] whitespace-nowrap text-subtle" style={{ left: PAD, top: laneY(live.length) - 25 }}>
              {kind.pointInTime ? (
                <>
                  click main's history<span className="max-sm:hidden">: the next branch forks from there</span>
                  <span className="sm:hidden"> to fork from there</span>
                </>
              ) : (
                "click: the next branch forks from now"
              )}
            </p>
          )}

          {MARKS.map((m) => (
            <span
              key={m}
              className={`absolute bottom-2 font-mono text-[10.5px] whitespace-nowrap text-subtle ${m === 0 ? "-translate-x-full" : m === 10 ? "" : "-translate-x-1/2 max-sm:hidden"}`}
              style={{ left: at(1 - m / 10) }}
            >
              {m === 0 ? "now" : `−${m} min`}
            </span>
          ))}
        </div>

        <div className="mt-auto border-t border-line px-5 py-3.5">
          <p className="flex gap-2 overflow-x-auto font-mono text-[12px] whitespace-nowrap [scrollbar-width:none]">
            <span className="text-subtle">$</span>
            {newest ? (
              <>
                <span className="text-fg">{kind.cmd(newest.name, newest.at)}</span>
                <span className="text-subtle">
                  → branch {newest.name}
                  {kind.ms ? ` in ${kind.ms} ms` : ""}
                </span>
              </>
            ) : (
              <span className="text-subtle">{kind.cmd("dev", null)}</span>
            )}
          </p>
          <p className="mt-2 text-[12.5px] leading-relaxed text-subtle text-pretty">
            {kind.pointInTime ? "Hover main's history and click to branch from that moment. " : "Click to branch from now. "}
            {kind.caption}
          </p>
        </div>
        <p aria-live="polite" className="sr-only">
          {announce}
        </p>
      </div>
    </div>
  );
}

/** A branch's status: its measured time counting up while it's made, then ready. */
function Ready({ branch, kind, reduced }: { branch: Branch; kind: Kind; reduced: boolean }) {
  const [now, setNow] = useState(() => performance.now());
  const elapsed = now - branch.created;
  const done = reduced || elapsed >= branch.readyMs;
  useEffect(() => {
    if (done) return;
    let raf = 0;
    const tick = () => {
      setNow(performance.now());
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [done]);

  if (!done) {
    return <span className="text-accent tabular-nums">{kind.ms ? `${Math.round(elapsed)} ms` : "branching…"}</span>;
  }
  return (
    <span className="text-ok tabular-nums">
      ready{kind.ms ? ` · ${kind.ms} ms` : ""}
    </span>
  );
}
