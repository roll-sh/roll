import { useReducedMotion } from "@roll/brand";
import { useRef, type ReactNode } from "react";
import { Link } from "@/components/link";
import { productBySlug, productHref } from "@/lib/marketing";
import { flow } from "./art/kit";
import { CountUp } from "./fx";
import { useInView, useSeen } from "./in-view";
import { Halftone, Versus } from "./scene";

const EASE = "cubic-bezier(0.23, 1, 0.32, 1)";

/**
 * A duration drawn to scale: a span from zero to the measured time on an axis of `of` (in the
 * same unit), between what starts the clock and what stops it.
 */
export function Span({ value, of, unit, from, to, marks, big = false }: { value: number; of: number; unit: string; from: string; to: string; marks: number[]; big?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const seen = useSeen(ref);
  const reduced = useReducedMotion();
  const shown = seen || reduced;
  const share = (value / of) * 100;
  return (
    <div ref={ref} className={`font-mono text-subtle ${big ? "text-[12px]" : "text-[10.5px]"}`}>
      <div className="relative h-5">
        <span className="absolute left-0 text-muted">{from}</span>
        <span className="absolute -translate-x-1/2 whitespace-nowrap text-fg transition-opacity duration-500" style={{ left: `max(${share}%, 5.5rem)`, opacity: shown ? 1 : 0, transitionDelay: "700ms" }}>
          {to}
        </span>
      </div>
      <div className={`relative border border-line ${big ? "h-10 rounded-md" : "h-4 rounded-[3px]"}`}>
        <div className="h-full bg-fg/10 text-fg" style={{ width: shown ? `${share}%` : "0%", transition: reduced ? "none" : `width 1100ms ${EASE} 150ms` }}>
          <Halftone level={75} />
        </div>
        <span className="absolute -top-1 -bottom-1 w-px bg-fg" style={{ left: shown ? `${share}%` : "0%", transition: reduced ? "none" : `left 1100ms ${EASE} 150ms` }} />
      </div>
      <div className="relative mt-1.5 h-4">
        {marks.map((m) => (
          <span key={m} className={`absolute whitespace-nowrap tabular-nums ${m === 0 ? "" : m === of ? "-translate-x-full" : "-translate-x-1/2"}`} style={{ left: `${(m / of) * 100}%` }}>
            {m}
            {m === of ? ` ${unit}` : ""}
          </span>
        ))}
      </div>
    </div>
  );
}

const AGENTS = Array.from({ length: 64 }, (_, i) => ({ x: 6 + (i % 16) * 9.5, y: 8 + Math.floor(i / 16) * 16 }));

/** 64 agents pushing at once, and the one main their merges land on. */
export function Agents() {
  const ref = useRef<HTMLDivElement>(null);
  const live = useInView(ref);
  return (
    <div ref={ref} data-live={live} className="art">
      <svg viewBox="0 0 320 72" fill="none" aria-hidden className="block h-auto w-full overflow-visible">
        {AGENTS.map((a, i) => (
          <circle key={i} cx={a.x} cy={a.y} r={2.1} className={i % 9 === 4 ? "fill-fg" : "fill-subtle"} />
        ))}
        {[8, 24, 40, 56].map((y, i) => (
          <path key={y} d={`M156,${y} C186,${y} 186,32 214,32`} className="stroke-subtle" strokeDasharray="3 4" {...flow(1.1 + i * 0.2, 7)} />
        ))}
        <path d="M214,32 H320" className="stroke-fg" />
        <path d="M214,32 H320" className="stroke-fg" strokeWidth={5} strokeDasharray="1.5 12" strokeLinecap="round" {...flow(0.9, 13.5, { scaled: true })} />
        <text x={6} y={71} fontSize={8.5} className="fill-subtle font-mono">
          64 agents
        </text>
        <text x={320} y={50} fontSize={8.5} textAnchor="end" className="fill-muted font-mono">
          main
        </text>
        <text x={320} y={71} fontSize={8.5} textAnchor="end" className="fill-subtle font-mono">
          40,000 merges in one run
        </text>
      </svg>
    </div>
  );
}

/**
 * The measurements that have something to stand beside, each from its product's `facts` in
 * lib/marketing.ts (its `factsNote` says where it was measured), drawn to scale.
 */
const NUMBERS: { product: string; value: string; label: string; visual: ReactNode }[] = [
  {
    product: "postgres",
    value: "0.66 s",
    label: "From nothing running to the first row.",
    visual: <Span big value={0.66} of={1} unit="s" from="connect" to="first row" marks={[0, 0.25, 0.5, 0.75, 1]} />,
  },
  {
    product: "search",
    value: "23 MiB",
    label: "Memory when idle.",
    visual: (
      <Versus
        big
        rows={[
          { name: "roll search", value: "23 MiB", share: 23 / 2395, own: true },
          { name: "Elasticsearch", value: "2,395 MiB", share: 1 },
        ]}
      />
    ),
  },
  {
    product: "nosql",
    value: "84k",
    label: "Lightweight transactions a second.",
    visual: (
      <Versus
        big
        rows={[
          { name: "roll nosql", value: "84k ops/s", share: 1, own: true },
          { name: "ScyllaDB", value: "10k ops/s", share: 10 / 84 },
        ]}
      />
    ),
  },
  {
    product: "queues",
    value: "180k",
    label: "Messages a second, on quorum queues.",
    visual: (
      <Versus
        big
        rows={[
          { name: "roll queues", value: "180k msg/s", share: 1, own: true },
          { name: "RabbitMQ 4", value: "71k msg/s", share: 71 / 180 },
        ]}
      />
    ),
  },
];

/** The numbers as rows: the measurement on the left, and across the rest of the row what it looks like beside the alternative. */
export function Numbers() {
  return (
    <div className="border-b border-line">
      {NUMBERS.map((n) => {
        const p = productBySlug(n.product)!;
        return (
          <Link
            key={n.product}
            href={productHref(p.slug)}
            className="group grid grid-cols-[minmax(0,1fr)] items-center gap-x-20 gap-y-8 border-t border-line py-10 sm:py-12 lg:grid-cols-[minmax(0,19rem)_minmax(0,1fr)]"
          >
            <div>
              <p className="flex items-center gap-2 font-mono text-[11px] text-subtle transition-colors group-hover:text-fg">
                <p.icon className="size-3.5" strokeWidth={1.5} />
                {p.name}
              </p>
              <CountUp value={n.value} className="mt-4 block font-mono text-6xl leading-none font-medium tracking-[-0.05em] tabular-nums sm:text-7xl" />
              <p className="mt-4 text-[15px] text-muted">{n.label}</p>
            </div>
            <div>
              {n.visual}
              <p className="mt-4 text-[11.5px] leading-snug text-subtle">{p.factsNote}</p>
            </div>
          </Link>
        );
      })}
    </div>
  );
}
