import { useReducedMotion } from "@roll/brand";
import { useEffect, useRef, useState } from "react";
import { inksOf, onSchemeChange, rgba, useInView } from "./in-view";
import { Steps, useSteps, type Step } from "./scene";

/** The parts of the picture a step can point at. */
type Part = "produce" | "log" | "tier" | "consume";

/** What the picture shows, a part at a time, in roll streams' own words (lib/marketing.ts). */
const POINTS: (Step & { part: Part })[] = [
  { part: "produce", title: "Your Kafka clients", body: "Producers are whatever you run today: tested with Apache Kafka 4.1's tools, librdkafka and Kafka Streams." },
  { part: "log", title: "Bytes in, bytes out", body: "Each partition's log grows at its head. Produce and fetch go to disk and back without decoding a record." },
  { part: "tier", title: "No bottom", body: "A sealed segment drops into object storage, so retention is a policy, not a disk." },
  { part: "consume", title: "Groups that catch up", body: "Group billing reads behind the head. Its lag shows, in the accent, when the producers outrun it." },
];

const PARTITIONS = 6;
/** Records each producer sends per second, and each consumer can read per second. */
const PRODUCE = 3;
const CONSUME = 4.2;
/** Seconds a record flies from its producer to its partition. */
const FLIGHT = 0.5;
/** Records per segment: a full segment that scrolls off the track is sealed and tiered. */
const SEG = 8;
/** Seconds a sealed segment takes to drop into object storage. */
const DROP = 0.9;
const CONSUMERS = ["c0", "c1", "c2"];
const owner = (p: number) => Math.floor(p / 2);

const easeOut = (t: number) => 1 - (1 - t) ** 3;
const clamp = (t: number) => Math.min(1, Math.max(0, t));

interface Partition {
  offset: number;
  /** Where the track is drawn from, easing toward `offset` so appends slide in. */
  shown: number;
  /** The consumer group's position: records below it are read. */
  read: number;
  flash: number;
}
interface Flight {
  from: number;
  p: number;
  t: number;
}
interface Drop {
  p: number;
  t: number;
}

/**
 * A roll stream as a picture you can steer: producers send records to topic `orders`, each partition's
 * log grows at its head, full segments roll off the end and drop into object storage (tiering), and
 * consumer group `billing` reads behind, its lag showing when the producers outrun it. Click a
 * partition to follow it to its consumer. Canvas, paused offscreen; reduced motion draws the steady state.
 */
export function StreamFlow({ className, bleed = false }: { className?: string; bleed?: boolean }) {
  const ref = useRef<HTMLCanvasElement>(null);
  const reduced = useReducedMotion();
  const inView = useInView(ref);
  const steps = useSteps(POINTS.length);
  const part = useRef<Part>(POINTS[0].part);
  const [read, setRead] = useState({ lag: 0, chunks: 14, sent: 0 });
  const [producers, setProducers] = useState(3);
  const producersRef = useRef(producers);
  const selected = useRef<number | null>(null);
  const redraw = useRef<() => void>(() => {});
  const sim = useRef({
    parts: Array.from({ length: PARTITIONS }, (_, i): Partition => {
      const offset = 1180 + i * 37;
      return { offset, shown: offset, read: offset - 2, flash: 0 };
    }),
    flights: [] as Flight[],
    drops: [] as Drop[],
    chunks: 14,
    sent: 0,
    nextSend: 0,
    t: 0,
  });

  useEffect(() => {
    producersRef.current = producers;
    redraw.current();
  }, [producers]);

  useEffect(() => {
    part.current = POINTS[steps.at].part;
    redraw.current();
  }, [steps.at]);

  useEffect(() => {
    const canvas = ref.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    let inks = inksOf(canvas);
    let font = getComputedStyle(canvas).fontFamily;
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

    /** Where everything sits for the current size; narrow canvases drop the side labels. */
    const layout = () => {
      const narrow = w < 520;
      const x0 = narrow ? w * 0.14 : w * 0.2;
      const x1 = narrow ? w * 0.7 : w * 0.72;
      const top = 40;
      const bandY = h * 0.76;
      const rh = (bandY - top - 22) / PARTITIONS;
      const cell = narrow ? 8 : 10;
      return { narrow, x0, x1, top, rh, bandY, cell, cap: Math.floor((x1 - x0) / cell), px: narrow ? w * 0.05 : w * 0.07, cx: narrow ? w * 0.86 : w * 0.88 };
    };
    const rowY = (L: ReturnType<typeof layout>, p: number) => L.top + L.rh * (p + 0.5);
    const prodY = (L: ReturnType<typeof layout>, i: number, n: number) => L.top + ((L.bandY - L.top - 22) * (i + 0.5)) / n;
    const consY = (L: ReturnType<typeof layout>, c: number) => (rowY(L, c * 2) + rowY(L, c * 2 + 1)) / 2;

    const step = (dt: number) => {
      const L = layout();
      s.t += dt;
      const n = producersRef.current;
      // Producers send on a jittered clock; each record picks a partition (no key: round-robin-ish).
      s.nextSend -= dt * n * PRODUCE;
      while (s.nextSend <= 0) {
        s.nextSend += 0.6 + Math.random() * 0.8;
        s.flights.push({ from: Math.floor(Math.random() * n), p: Math.floor(Math.random() * PARTITIONS), t: s.t });
        s.sent++;
      }
      s.flights = s.flights.filter((f) => {
        if (s.t - f.t < FLIGHT) return true;
        const part = s.parts[f.p];
        part.offset++;
        part.flash = 1;
        // The oldest whole segment has left the track: seal it and tier it.
        if ((part.offset - L.cap) % SEG === 0) s.drops.push({ p: f.p, t: s.t });
        return false;
      });
      s.drops = s.drops.filter((d) => {
        if (s.t - d.t < DROP) return true;
        s.chunks++;
        return false;
      });
      // Each consumer splits its reading between its two partitions, the one further behind first.
      CONSUMERS.forEach((_, c) => {
        let budget = CONSUME * dt;
        for (const p of [c * 2, c * 2 + 1].sort((a, b) => s.parts[b].offset - s.parts[b].read - (s.parts[a].offset - s.parts[a].read))) {
          const part = s.parts[p];
          const take = Math.min(budget, part.offset - part.read);
          part.read += take;
          budget -= take;
        }
      });
      for (const part of s.parts) {
        part.shown += (part.offset - part.shown) * Math.min(1, dt * 10);
        part.flash = Math.max(0, part.flash - dt * 3);
      }
    };

    const draw = () => {
      const L = layout();
      const { ink, accent } = inks;
      const sel = selected.current;
      const n = producersRef.current;
      ctx.clearRect(0, 0, w, h);
      ctx.font = `10.5px ${font}`;
      ctx.textBaseline = "middle";
      // The part the current step points at stays bright; the rest steps back.
      const lit = (p: Part) => (part.current === p ? 1 : 0.6);

      // What each column is.
      ctx.textAlign = "left";
      ctx.fillStyle = rgba(ink, 0.5 * lit("produce") + 0.1);
      if (!L.narrow) ctx.fillText("producers", L.px - 22, 16);
      ctx.fillStyle = rgba(ink, 0.5 * lit("log") + 0.1);
      ctx.fillText(L.narrow ? "partitions" : "partitions · oldest to newest", L.x0 - 4, 16);
      ctx.fillStyle = rgba(ink, 0.5 * lit("consume") + 0.1);
      ctx.textAlign = "right";
      ctx.fillText(L.narrow ? "billing" : "consumer group billing", w - 12, 16);
      ctx.textAlign = "left";

      // Producers.
      ctx.globalAlpha = lit("produce");
      for (let i = 0; i < n; i++) {
        const y = prodY(L, i, n);
        ctx.fillStyle = rgba(ink, 0.85);
        ctx.beginPath();
        ctx.arc(L.px, y, 4, 0, Math.PI * 2);
        ctx.fill();
        ctx.fillStyle = rgba(ink, 0.4);
        ctx.textAlign = "center";
        ctx.fillText(`app-${i + 1}`, L.px, y + 15);
        ctx.textAlign = "left";
      }

      // Partitions: the track, its records (newest at the head, on the right) and its offset.
      ctx.globalAlpha = Math.max(lit("log"), lit("consume") === 1 ? 0.8 : 0);
      const size = L.cell - 3;
      for (let p = 0; p < PARTITIONS; p++) {
        const part = s.parts[p];
        const y = rowY(L, p);
        const dim = sel !== null && sel !== p ? 0.35 : 1;
        ctx.fillStyle = rgba(ink, 0.06 * dim + (sel === p ? 0.05 : 0));
        ctx.beginPath();
        ctx.roundRect(L.x0 - 4, y - L.rh * 0.36, L.x1 - L.x0 + 8, L.rh * 0.72, 5);
        ctx.fill();
        const frac = part.offset - part.shown;
        for (let k = 0; k < L.cap + 1; k++) {
          const o = part.offset - 1 - k;
          const x = L.x1 - (k + frac + 1) * L.cell;
          if (x < L.x0 - 1) break;
          const unread = o >= part.read;
          const fresh = k === 0 ? part.flash : 0;
          ctx.fillStyle = unread ? rgba(accent, (0.5 + 0.5 * fresh) * dim) : rgba(ink, (o % SEG === SEG - 1 ? 0.62 : 0.4) * dim);
          ctx.beginPath();
          ctx.roundRect(x, y - size / 2, size, size, 2);
          ctx.fill();
        }
        ctx.fillStyle = rgba(ink, 0.45 * dim);
        if (!L.narrow) {
          ctx.textAlign = "right";
          ctx.fillText(`p${p}`, L.x0 - 12, y);
        }
        ctx.textAlign = "left";
        ctx.fillStyle = rgba(ink, 0.75 * dim);
        ctx.fillText(part.offset.toLocaleString("en-US"), L.x1 + 10, y);
      }

      // Records in flight, producer to partition head.
      ctx.globalAlpha = Math.max(lit("produce"), lit("log"));
      for (const f of s.flights) {
        const p = easeOut(clamp((s.t - f.t) / FLIGHT));
        const y0 = prodY(L, Math.min(f.from, n - 1), n);
        const y1 = rowY(L, f.p);
        const x = L.px + (L.x1 - L.cell - L.px) * p;
        const y = y0 + (y1 - y0) * p;
        ctx.fillStyle = rgba(ink, (0.35 + 0.55 * p) * (sel !== null && sel !== f.p ? 0.35 : 1));
        ctx.beginPath();
        ctx.roundRect(x - size / 2, y - size / 2, size, size, 2);
        ctx.fill();
      }

      // Object storage: sealed segments land here as chunks and drift left.
      ctx.globalAlpha = lit("tier");
      const by = L.bandY;
      const bh = h - by - 14;
      ctx.strokeStyle = rgba(ink, 0.2);
      ctx.setLineDash([3, 4]);
      ctx.beginPath();
      ctx.roundRect(L.x0 - 4, by, L.x1 - L.x0 + 8, bh, 6);
      ctx.stroke();
      ctx.setLineDash([]);
      const cw = SEG * 2.2;
      const pitch = cw + 6;
      const room = Math.floor((L.x1 - L.x0 - 12) / pitch);
      for (let k = 0; k < room; k++) {
        const x = L.x1 - 8 - (k + 1) * pitch;
        ctx.fillStyle = rgba(ink, 0.3 * (1 - k / room) + 0.08);
        ctx.beginPath();
        ctx.roundRect(x, by + bh / 2 - 5, cw, 10, 2);
        ctx.fill();
      }
      for (const d of s.drops) {
        const p = easeOut(clamp((s.t - d.t) / DROP));
        const y0 = rowY(L, d.p);
        const x = L.x0 + (L.x1 - 8 - pitch - L.x0) * p;
        ctx.fillStyle = rgba(accent, 0.8 - 0.4 * p);
        ctx.beginPath();
        ctx.roundRect(x, y0 + (by + bh / 2 - 5 - y0) * p, cw, 10, 2);
        ctx.fill();
      }
      ctx.fillStyle = rgba(ink, 0.45);
      ctx.textAlign = "left";
      ctx.fillText("object storage", L.x0 + 6, by + 12);
      ctx.textAlign = "right";
      ctx.fillText(L.narrow ? `${s.chunks} tiered` : `${s.chunks} segments tiered`, L.x1 + 2, by + 12);

      // Consumer group billing: assignment lines from each partition's head, and the lag.
      ctx.globalAlpha = lit("consume");
      for (let c = 0; c < CONSUMERS.length; c++) {
        const y = consY(L, c);
        const mine = sel !== null && owner(sel) === c;
        const dim = sel !== null && !mine ? 0.35 : 1;
        const lag = Math.round(s.parts[c * 2].offset - s.parts[c * 2].read + s.parts[c * 2 + 1].offset - s.parts[c * 2 + 1].read);
        for (const p of [c * 2, c * 2 + 1]) {
          ctx.strokeStyle = rgba(mine && sel === p ? accent : ink, (mine && sel === p ? 0.8 : 0.16) * dim);
          ctx.beginPath();
          ctx.moveTo(L.x1 + (L.narrow ? 36 : 48), rowY(L, p));
          ctx.lineTo(L.cx - 8, y);
          ctx.stroke();
        }
        ctx.fillStyle = rgba(lag > 6 ? accent : ink, 0.9 * dim);
        ctx.beginPath();
        ctx.arc(L.cx, y, 4.5, 0, Math.PI * 2);
        ctx.fill();
        ctx.textAlign = "left";
        ctx.fillStyle = rgba(ink, 0.8 * dim);
        ctx.fillText(CONSUMERS[c], L.cx + 10, y - 7);
        ctx.fillStyle = rgba(lag > 6 ? accent : ink, 0.55 * dim);
        ctx.fillText(`lag ${lag}`, L.cx + 10, y + 7);
      }
      ctx.globalAlpha = 1;
    };

    /** The header's counters, a few times a second. */
    const publish = (now: number) => {
      if (now - shownAt < 250) return;
      shownAt = now;
      const lag = Math.round(s.parts.reduce((sum, p) => sum + p.offset - p.read, 0));
      setRead((r) => (r.lag === lag && r.chunks === s.chunks && r.sent === s.sent ? r : { lag, chunks: s.chunks, sent: s.sent }));
    };

    const frame = (now: number) => {
      const dt = last ? Math.min(0.05, (now - last) / 1000) : 0;
      last = now;
      step(dt);
      draw();
      publish(now);
      raf = requestAnimationFrame(frame);
    };
    redraw.current = () => (reduced || !inView) && draw();

    resize();
    const ro = new ResizeObserver(() => {
      resize();
      redraw.current();
    });
    ro.observe(canvas);
    const off = onSchemeChange(() => {
      inks = inksOf(canvas);
      font = getComputedStyle(canvas).fontFamily;
      redraw.current();
    });
    const onDown = (e: PointerEvent) => {
      const L = layout();
      const r = canvas.getBoundingClientRect();
      const p = Math.floor((e.clientY - r.top - L.top) / L.rh);
      selected.current = p >= 0 && p < PARTITIONS && selected.current !== p ? p : null;
      redraw.current();
    };
    canvas.addEventListener("pointerdown", onDown);
    if (reduced || !inView) draw();
    else raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      off();
      canvas.removeEventListener("pointerdown", onDown);
    };
  }, [reduced, inView]);

  const counters = (
    <>
      <span className="text-fg">
        topic orders <span className="text-subtle">· 6 partitions</span>
      </span>
      <span className="text-subtle max-sm:hidden">
        sent <span className="text-fg tabular-nums">{read.sent.toLocaleString("en-US")}</span>
      </span>
      <span className="text-subtle">
        lag <span className={`tabular-nums ${read.lag > 12 ? "text-accent" : "text-fg"}`}>{read.lag}</span>
      </span>
      <span className="text-subtle max-md:hidden">
        tiered <span className="text-fg tabular-nums">{read.chunks}</span> segments
      </span>
      <span className={`flex items-center gap-2 text-subtle ${bleed ? "" : "ml-auto"}`}>
        producers
        <span className="flex rounded-md border border-line p-0.5">
          {[1, 2, 3, 4, 5, 6].map((k) => (
            <button
              key={k}
              onClick={() => setProducers(k)}
              aria-pressed={k === producers}
              className={`w-5 rounded-[5px] py-0.5 tabular-nums transition ${k === producers ? "bg-panel-2 text-fg" : "hover:text-fg"}`}
            >
              {k}
            </button>
          ))}
        </span>
      </span>
    </>
  );
  const label = "Producers writing to six partitions, sealed segments tiering to object storage, and a consumer group reading behind";

  // Across the page: the picture runs edge to edge, with its counters in one strip under it.
  if (bleed) {
    return (
      <div className={className}>
        <canvas ref={ref} aria-label={label} className="block h-[24rem] w-full cursor-pointer font-mono text-fg sm:h-[30rem]" />
        <div className="mt-8 flex justify-center px-4">
          <div className="flex flex-wrap items-center justify-center gap-x-5 gap-y-2 rounded-2xl border border-line-2 bg-bg px-5 py-2 font-mono text-[11px] sm:rounded-full">{counters}</div>
        </div>
      </div>
    );
  }

  return (
    <div className={`bg-bg ${className ?? ""}`}>
      <div className="flex flex-wrap items-center gap-x-5 gap-y-2 border-b border-line px-5 py-3 font-mono text-[11px]">{counters}</div>
      <canvas ref={ref} aria-label={label} className="block h-80 w-full cursor-pointer font-mono text-fg sm:h-[26rem]" />
      <Steps steps={POINTS} state={steps} running={inView} row className="border-t border-line" />
    </div>
  );
}
