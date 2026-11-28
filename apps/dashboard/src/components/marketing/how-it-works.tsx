import { HorizonMark, useReducedMotion } from "@roll/brand";
import { Cloud, Moon } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Link } from "@/components/link";
import { PRODUCTS, productBySlug, productHref } from "@/lib/marketing";
import { Beacon, T } from "./art/kit";
import { useInView } from "./in-view";
import { NOTE } from "./scene";

/**
 * One request per product: the client you already run, the protocol it speaks (each product's
 * `compat` in lib/marketing.ts), and what its engine keeps in object storage (AGENTS.md and the
 * products' own docs). `cold` is the product's measured wake-up, where its page gives one.
 */
const ROUTES: { client: string; protocol: string; product: string; keeps: string; cold?: string }[] = [
  { client: "psql", protocol: "Postgres wire", product: "postgres", keeps: "WAL and base images", cold: "0.66 s" },
  { client: "redis-cli", protocol: "RESP3", product: "redis", keeps: "every write" },
  { client: "kafka-console-producer", protocol: "Kafka protocol", product: "streams", keeps: "sealed segments" },
  { client: "aws s3 cp", protocol: "S3 API", product: "storage", keeps: "one blob per object" },
  { client: "cqlsh", protocol: "CQL", product: "nosql", keeps: "tables and a manifest", cold: "20 ms" },
  { client: "elasticsearch-py", protocol: "Elasticsearch REST", product: "search", keeps: "WAL, segments, manifests", cold: "19.8 ms" },
  { client: "amqp · mqtt · stomp", protocol: "AMQP 0-9-1", product: "queues", keeps: "durable messages" },
  { client: "git push", protocol: "git smart HTTP v2", product: "code", keeps: "changesets and blobs" },
];
/** The order requests arrive in: jumping about, so no two neighbours wake in a row. */
const ORDER = [0, 5, 2, 7, 4, 1, 6, 3];
/** An engine stays awake for this many requests after its own, then sleeps again. */
const LINGER = 3;
/** A request's phases, in ms: asked, the engine wakes, it reaches object storage, it answers, quiet. */
const PHASES = [750, 550, 750, 750, 800];
const [ASK, WAKE, STORE, ANSWER, REST] = [0, 1, 2, 3, 4];

const routed = new Set(ROUTES.map((r) => r.product));
const REST_OF = PRODUCTS.filter((p) => !routed.has(p.slug));

type Wire = { d: string };

/**
 * roll in one picture, with a request going through it. On the left are the clients you already
 * run, each speaking its own protocol; in the middle one daemon, whose engines are asleep until
 * they're asked and asleep again soon after; on the right object storage, where the data and its
 * history live, and where a branch is a second pointer into the same objects. One request at a
 * time travels the wires (measured from the rows, so they follow the layout), and the line under
 * the picture says what it's doing. On a phone it's the daemon's rows and the storage, no wires.
 * Paused offscreen; under reduced motion it's one still frame of a request answered.
 */
export function HowItWorks() {
  const box = useRef<HTMLDivElement>(null);
  const left = useRef<(HTMLDivElement | null)[]>([]);
  const mid = useRef<(HTMLElement | null)[]>([]);
  const right = useRef<(HTMLDivElement | null)[]>([]);
  const store = useRef<HTMLDivElement>(null);
  const [wires, setWires] = useState<{ ask: Wire[]; keep: Wire[] }>({ ask: [], keep: [] });
  const inView = useInView(box);
  const reduced = useReducedMotion();
  const [tick, setTick] = useState(0);
  const [phase, setPhase] = useState(reduced ? REST : ASK);

  useEffect(() => {
    if (reduced) return setPhase(REST);
    if (!inView) return;
    const t = setTimeout(() => {
      if (phase === REST) {
        setTick((n) => n + 1);
        setPhase(ASK);
      } else setPhase(phase + 1);
    }, PHASES[phase]);
    return () => clearTimeout(t);
  }, [phase, inView, reduced]);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const measure = () => {
      const o = el.getBoundingClientRect();
      const s = store.current?.getBoundingClientRect();
      if (o.width < 900 || !s) return setWires({ ask: [], keep: [] });
      const ask: Wire[] = [];
      const keep: Wire[] = [];
      ROUTES.forEach((_, i) => {
        const a = left.current[i]?.getBoundingClientRect();
        const b = mid.current[i]?.getBoundingClientRect();
        if (!a || !b) return;
        const x1 = a.right - o.left + 6;
        const y1 = a.top + a.height / 2 - o.top;
        const x2 = b.left - o.left - 6;
        const y2 = b.top + b.height / 2 - o.top;
        const k = (x2 - x1) / 2;
        ask.push({ d: `M${x1},${y1} C${x1 + k},${y1} ${x2 - k},${y2} ${x2},${y2}` });
        const x3 = b.right - o.left + 6;
        const c = right.current[i]?.getBoundingClientRect() ?? s;
        const x4 = c.left - o.left - 6;
        const y4 = c.top + c.height / 2 - o.top;
        const k2 = (x4 - x3) / 2;
        keep.push({ d: `M${x3},${y2} C${x3 + k2},${y2} ${x4 - k2},${y4} ${x4},${y4}` });
      });
      setWires({ ask, keep });
    };
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    measure();
    return () => ro.disconnect();
  }, []);

  const at = ORDER[tick % ORDER.length];
  const route = ROUTES[at];
  /** Awake: the one being asked (once it has woken) and the few asked just before it. */
  const state = (i: number): "asleep" | "waking" | "awake" => {
    if (i === at) return phase === ASK ? "asleep" : phase === WAKE ? "waking" : "awake";
    for (let back = 1; back < LINGER; back++) {
      if (tick - back >= 0 && ORDER[(tick - back) % ORDER.length] === i) return "awake";
    }
    return "asleep";
  };
  const awake = ROUTES.filter((_, i) => state(i) !== "asleep").length;
  /** Objects written so far: one per request that reached storage. */
  const objects = 1280 + tick + (phase >= STORE ? 1 : 0);
  const moving = inView && !reduced;

  return (
    <figure>
      <div ref={box} className="relative grid grid-cols-[minmax(0,1fr)] gap-6 lg:grid-cols-[minmax(0,0.82fr)_minmax(0,1.2fr)_minmax(0,0.98fr)] lg:gap-x-24">
        <svg aria-hidden className="pointer-events-none absolute inset-0 hidden size-full overflow-visible lg:block">
          {wires.ask.map((w, i) => (
            <path key={`a${i}`} d={w.d} fill="none" strokeWidth={1} strokeDasharray={i === at ? undefined : "2 5"} className={`transition-colors duration-300 ${i === at ? "stroke-muted" : "stroke-line-2"}`} />
          ))}
          {wires.keep.map((w, i) => (
            <path
              key={`k${i}`}
              d={w.d}
              fill="none"
              strokeWidth={1}
              strokeDasharray={i === at && phase >= STORE ? undefined : "2 5"}
              className={`transition-colors duration-300 ${i === at && phase >= STORE ? "stroke-muted" : state(i) === "asleep" ? "stroke-line" : "stroke-line-2"}`}
            />
          ))}
        </svg>
        {moving && wires.ask[at] && (
          <div aria-hidden className="pointer-events-none absolute inset-0 hidden lg:block">
            {phase === ASK && <Packet key={`ask${tick}`} d={wires.ask[at].d} ms={PHASES[ASK]} />}
            {phase === STORE && <Packet key={`keep${tick}`} d={wires.keep[at].d} ms={PHASES[STORE]} />}
            {phase === ANSWER && <Packet key={`ans${tick}`} d={wires.ask[at].d} ms={PHASES[ANSWER]} back />}
          </div>
        )}

        <div className="hidden lg:block">
          <Head n="1" title="Your clients, unchanged" body="Each speaks its own protocol to roll." />
          <div className={`mt-5 ${BOX} border-transparent px-0`}>
            <p className={`${BAR} px-0 font-mono text-[10.5px] text-subtle`}>what you already run</p>
            <div className="space-y-1.5">
              {ROUTES.map((r, i) => (
                <div
                  key={r.client}
                  ref={(e) => {
                    left.current[i] = e;
                  }}
                  className={`${ROW} ${i === at && phase !== REST ? "border-line-2 bg-panel" : "border-line"}`}
                >
                  <span className={`truncate font-mono text-[12px] transition-colors duration-300 ${i === at && phase !== REST ? "text-fg" : "text-muted"}`}>{r.client}</span>
                  <span className="ml-auto shrink-0 font-mono text-[10.5px] text-subtle">{r.protocol}</span>
                </div>
              ))}
            </div>
          </div>
        </div>

        <div>
          <Head n="2" title="One daemon" body="An engine wakes when it's asked, and sleeps when it isn't." />
          <div className={`mt-5 ${BOX} border-line-2 bg-panel/60`}>
            <div className={BAR}>
              <HorizonMark size={16} interactive={false} className="text-fg" />
              <span className="text-[14px] font-semibold tracking-[-0.02em]">rolld</span>
              <span className="ml-auto font-mono text-[10.5px] text-subtle tabular-nums">
                <span className="text-fg">{awake}</span> of {ROUTES.length} awake <span className="max-sm:hidden">· the rest cost nothing</span>
              </span>
            </div>
            <div className="space-y-1.5">
              {ROUTES.map((r, i) => {
                const p = productBySlug(r.product)!;
                const s = state(i);
                return (
                  <Link
                    key={r.product}
                    href={productHref(p.slug)}
                    ref={(e: HTMLAnchorElement | null) => {
                      mid.current[i] = e;
                    }}
                    className={`${ROW} gap-2.5 hover:border-fg/30 ${s === "asleep" ? "border-dashed border-line-2 bg-transparent" : "border-line-2 bg-bg"}`}
                  >
                    <p.icon className={`size-3.5 shrink-0 transition-colors duration-300 ${s === "asleep" ? "text-subtle" : "text-fg"}`} strokeWidth={1.5} />
                    <span className={`text-[13px] transition-colors duration-300 ${s === "asleep" ? "text-muted" : "text-fg"}`}>{p.name}</span>
                    <span className="truncate font-mono text-[10.5px] text-subtle lg:hidden">{r.protocol}</span>
                    <span className="ml-auto flex shrink-0 items-center gap-1.5 font-mono text-[10.5px] tabular-nums">
                      {s === "asleep" ? (
                        <>
                          <Moon className="size-3 text-subtle" strokeWidth={1.5} />
                          <span className="text-subtle">asleep</span>
                        </>
                      ) : s === "waking" ? (
                        <>
                          <span className="size-1.5 rounded-full bg-accent" />
                          <span className="text-accent">waking</span>
                        </>
                      ) : (
                        <>
                          <span className="size-1.5 rounded-full bg-fg" />
                          <span className="text-fg">awake</span>
                          {r.cold && i === at && <span className="text-subtle max-sm:hidden">· from zero in {r.cold}</span>}
                        </>
                      )}
                    </span>
                  </Link>
                );
              })}
            </div>
            <p className={`${FOOT} flex flex-wrap items-center gap-x-1.5 gap-y-1`}>
              and
              {REST_OF.map((p) => (
                <Link key={p.slug} href={productHref(p.slug)} className="inline-flex items-center gap-1 text-muted transition-colors hover:text-fg">
                  <p.icon className="size-3" strokeWidth={1.5} />
                  {p.name}
                </Link>
              ))}
            </p>
          </div>
        </div>

        <div>
          <Head n="3" title="Object storage" body="Data and its history live here, so anything can branch." />
          <div ref={store} className={`mt-5 ${BOX} border-line-2 bg-panel/60`}>
            <div className={BAR}>
              <Cloud className="size-4 text-fg" strokeWidth={1.5} />
              <span className="truncate text-[14px] font-medium tracking-[-0.01em]">S3, or any compatible store</span>
              <span className="ml-auto shrink-0 font-mono text-[10.5px] text-subtle tabular-nums">
                <span className="text-fg">{objects.toLocaleString("en-US")}</span> objects
              </span>
            </div>
            <div className="space-y-1.5">
              {ROUTES.map((r, i) => {
                const fresh = i === at && phase >= STORE;
                return (
                  <div
                    key={r.product}
                    ref={(e) => {
                      right.current[i] = e;
                    }}
                    className={`${ROW} ${fresh ? "border-line-2 bg-bg" : "border-line"}`}
                  >
                    <span className={`truncate font-mono text-[11.5px] transition-colors duration-300 ${fresh ? "text-fg" : "text-muted"}`}>{r.keeps}</span>
                    <span aria-hidden className="ml-auto flex shrink-0 gap-[3px]">
                      {["bg-fg/10", "bg-fg/20", "bg-fg/20", "bg-fg/35", "bg-fg/35"].slice(0, 3 + ((i * 2) % 3)).map((tone, k) => (
                        <span key={k} className={`h-3.5 w-2.5 rounded-[2px] border border-line-2 ${tone}`} />
                      ))}
                      <span className={`h-3.5 w-2.5 rounded-[2px] border transition-all duration-300 ${fresh ? "border-accent bg-accent/25" : "border-dashed border-line-2"}`} />
                    </span>
                  </div>
                );
              })}
            </div>
            <p className={`${FOOT} text-balance`}>kept with their history, so there&apos;s always a moment to branch from</p>
          </div>
        </div>
      </div>

      <figcaption aria-live="off" className="mx-auto mt-10 flex w-fit max-w-full flex-wrap items-center justify-center gap-x-2.5 gap-y-1.5 rounded-2xl border border-line bg-bg px-5 py-2.5 sm:rounded-full font-mono text-[11.5px] text-subtle">
        <span className={NOTE}>request</span>
        <Said on>{route.client}</Said>
        <Arrow />
        <Said on>{route.protocol}</Said>
        <Arrow />
        <Said on={phase >= WAKE}>
          {productBySlug(route.product)!.name} wakes{route.cold ? ` in ${route.cold}` : ""}
        </Said>
        <Arrow />
        <Said on={phase >= STORE}>object storage has its {route.keeps === "every write" ? "writes" : route.keeps}</Said>
        <Arrow />
        <Said on={phase >= ANSWER}>answers</Said>
        <Arrow />
        <Said on={phase >= REST}>sleeps when it's quiet</Said>
      </figcaption>

    </figure>
  );
}

/** The three columns' boxes line up row for row: the same padding, bar, row and foot in each. */
const BOX = "rounded-2xl border p-2.5";
const BAR = "flex h-10 items-center gap-2.5 px-2 pb-1.5";
const ROW = "flex h-10 items-center gap-3 rounded-lg border px-3 transition-colors duration-300";
const FOOT = "px-2 pt-3 pb-1 text-[12px] leading-relaxed text-subtle";

function Head({ n, title, body }: { n: string; title: string; body: string }) {
  return (
    <div>
      <p className="flex items-center gap-2.5">
        <span className="grid size-5 place-items-center rounded-full border border-line-2 font-mono text-[10.5px] text-muted">{n}</span>
        <span className="text-[15px] font-medium tracking-[-0.01em] text-fg">{title}</span>
      </p>
      <p className="mt-1.5 text-[13px] text-muted lg:min-h-[2.5rem]">{body}</p>
    </div>
  );
}

function Said({ on, children }: { on: boolean; children: React.ReactNode }) {
  return <span className={`transition-colors duration-300 ${on ? "text-fg" : "text-subtle"}`}>{children}</span>;
}

const Arrow = () => (
  <span aria-hidden className="text-line-2">
    →
  </span>
);

/** The request itself: a dot that follows a wire, there or back. */
function Packet({ d, ms, back = false }: { d: string; ms: number; back?: boolean }) {
  return (
    <span
      className="absolute top-0 left-0 size-[7px] rounded-full bg-accent ring-4 ring-accent/15"
      style={{ offsetPath: `path("${d}")`, offsetRotate: "0deg", animation: `along ${ms}ms cubic-bezier(0.45, 0, 0.2, 1) ${back ? "reverse " : ""}both` }}
    />
  );
}

const GAP = 4.7;

/**
 * What object storage holds, drawn wide: history as a row of objects, the newest on the right;
 * main, which points at all of it; and a branch, which points at the same objects up to where it
 * forked and holds only what it wrote since. `narrow` is the same drawing at a phone's width:
 * half the objects, so its labels stay readable.
 */
export function History({ fresh = false, tick = 0, narrow = false, className }: { fresh?: boolean; tick?: number; narrow?: boolean; className?: string }) {
  const w = narrow ? 320 : 640;
  const blocks = narrow ? 13 : 26;
  const shared = narrow ? 8 : 16;
  const pitch = (w + GAP) / blocks;
  const fork = shared * pitch - GAP;
  const tone = narrow ? "how-n" : "how-w";
  const writes = narrow ? [30, 86, 142, 232, 286] : [40, 110, 180, 250, 320, 460, 530, 600];
  const own = narrow ? [262, 296] : [500, 560];
  return (
    <svg viewBox={`0 0 ${w} ${narrow ? 164 : 150}`} fill="none" aria-hidden className={`block h-auto w-full overflow-visible ${className ?? ""}`}>
      <Tones id={tone} />
      <T x={0} y={10} tone="faint" size={9}>
        objects, oldest first
      </T>
      <T x={w} y={10} tone="faint" anchor="end" size={9}>
        now
      </T>
      {Array.from({ length: blocks }, (_, i) => {
        const newest = i === blocks - 1;
        const kept = i < shared;
        return (
          <g key={newest ? `n${tick}` : i} className={newest && fresh ? "rise" : undefined}>
            <rect x={i * pitch} y={20} width={pitch - GAP} height={24} rx={3} fill={`url(#${tone}${i < shared / 2 ? 25 : kept ? 50 : 75})`} />
            <rect x={i * pitch} y={20} width={pitch - GAP} height={24} rx={3} className={newest && fresh ? "stroke-accent" : kept ? "stroke-line-2" : "stroke-subtle"} />
          </g>
        );
      })}
      {fresh && <Beacon cx={w - (pitch - GAP) / 2} cy={32} r={2.4} />}

      <path d={`M0,54 V60 H${fork} V54`} className="stroke-subtle" />
      <T x={fork / 2} y={74} tone="muted" anchor="middle" size={9}>
        {narrow ? "shared by both, not copied" : "shared by main and pr-1240, not copied"}
      </T>

      <path d={`M0,96 H${w}`} className="stroke-muted" />
      {writes.map((x) => (
        <circle key={x} cx={x} cy={96} r={2.6} className="fill-fg" />
      ))}
      <circle cx={w - 3} cy={96} r={3.4} className="fill-fg" />
      <T x={0} y={88} tone="fg" size={10}>
        main
      </T>

      <circle cx={fork} cy={96} r={3.6} className="fill-bg stroke-fg" />
      <path d={`M${fork},60 V92`} className="stroke-subtle" strokeDasharray="2 3" />
      <path d={`M${fork + 3.5},96 c${narrow ? "20,0 16,38 36,38" : "30,0 26,38 56,38"} H${w}`} className="stroke-subtle" />
      {own.map((x) => (
        <circle key={x} cx={x} cy={134} r={2.6} className="fill-muted" />
      ))}
      <T x={fork + (narrow ? 44 : 66)} y={124} tone="fg" size={10}>
        pr-1240
      </T>
      <T x={w} y={148} tone="faint" anchor="end" size={9}>
        only its own writes are new
      </T>
      <T x={0} y={narrow ? 162 : 148} tone="faint" size={9}>
        branch from now, or from any moment kept
      </T>
    </svg>
  );
}

/** The drawing's halftones (the bento's `Dither`, at this drawing's size). */
function Tones({ id }: { id: string }) {
  return (
    <defs>
      {([25, 50, 75] as const).map((level) => (
        <pattern key={level} id={`${id}${level}`} width={6} height={6} patternUnits="userSpaceOnUse">
          {(level === 25 ? [[0, 0]] : level === 50 ? [[0, 0], [3, 3]] : [[0, 0], [3, 3], [3, 0]]).map(([x, y]) => (
            <rect key={`${x}${y}`} x={x} y={y} width={1.7} height={1.7} className="fill-fg" />
          ))}
        </pattern>
      ))}
    </defs>
  );
}
