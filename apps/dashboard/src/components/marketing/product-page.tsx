import { ArrowRight, ArrowUpRight } from "lucide-react";
import { useRef, type ReactNode } from "react";
import { Link } from "@/components/link";
import { snippetsFor } from "@/lib/marketing-connect";
import { GROUPS, PRODUCTS, type Fact, type Product } from "@/lib/marketing";
import { AgentsCanvas } from "./agents-canvas";
import { HERO } from "./art/hero";
import { ART, Bento } from "./bento";
import { BranchTimeline } from "./branch-timeline";
import { InstallCommand } from "./chrome";
import { CodePanel } from "./code-panel";
import { CountUp, Reveal } from "./fx";
import { useInView } from "./in-view";
import { EYEBROW, H2, WRAP } from "./layout";
import { MergeQueue } from "./merge-queue";
import { ScaleSim } from "./scale-sim";
import { Scene, Versus } from "./scene";
import { Tour } from "./shot";
import { shotFor } from "./shots";
import { StreamFlow } from "./stream-flow";
import { Terminal } from "./terminal";

/**
 * A product's page, a section at a time. Each section has a shape of its own (the picture beside
 * the words, numbers in a strip, code beside a list, a centred screenshot, a demo on a band, a
 * column that stays while the features pass, the terminal last), and a product only gets the ones
 * it has something for. Everything they say is lib/marketing.ts's.
 */

const PRIMARY = "inline-flex shrink-0 items-center justify-center gap-1.5 rounded-lg bg-fg px-4 py-2.5 text-[14px] font-medium text-bg transition hover:opacity-85";

/** The product's own picture: its hero drawing in a frame, with what it speaks under it. On a phone, the card's smaller drawing, whose labels still read. */
function Picture({ p }: { p: Product }) {
  const ref = useRef<HTMLDivElement>(null);
  const live = useInView(ref);
  const Big = HERO[p.slug];
  const Small = ART[p.slug];
  return (
    <div ref={ref} data-live={live} data-stage className="art relative overflow-hidden rounded-2xl border border-line bg-panel">
      <div className="px-5 pt-7 pb-6 sm:px-9 sm:pt-10 sm:pb-9">
        {Big && (
          <div className="hidden sm:block">
            <Big />
          </div>
        )}
        {Small && (
          <div className={Big ? "sm:hidden" : ""}>
            <Small />
          </div>
        )}
      </div>
      <p className="flex items-center gap-2.5 border-t border-line px-5 py-3.5 font-mono text-[11px] text-muted sm:px-9">
        <p.icon className="size-3.5 shrink-0 text-subtle" strokeWidth={1.5} />
        <span className="min-w-0 truncate">{p.compat}</span>
      </p>
    </div>
  );
}

export function ProductHero({ p }: { p: Product }) {
  const group = GROUPS.find((g) => g.id === p.group)!;
  return (
    <section className="relative isolate overflow-hidden border-b border-line">
      <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] items-center gap-12 pt-12 pb-16 lg:grid-cols-[minmax(0,0.92fr)_minmax(0,1.08fr)] lg:gap-16 lg:pt-20 lg:pb-24`}>
        <div className="rise">
          <nav aria-label="Breadcrumb" className={`flex items-center gap-2 ${EYEBROW}`}>
            <Link href="/products" className="transition-colors hover:text-fg">
              Products
            </Link>
            <span aria-hidden>/</span>
            <Link href={`/products#${group.id}`} className="transition-colors hover:text-fg">
              {group.name}
            </Link>
          </nav>
          <p className="mt-8 flex items-center gap-3">
            <span className="grid size-9 place-items-center rounded-[10px] border border-line-2 bg-panel">
              <p.icon className="size-[18px]" strokeWidth={1.5} />
            </span>
            <span className="text-[17px] font-medium tracking-[-0.02em]">{p.name}</span>
          </p>
          <h1 className="mt-6 max-w-[15em] text-[2.5rem] leading-[1.02] font-semibold tracking-[-0.05em] text-balance sm:text-[3.4rem] xl:text-[3.75rem]">{p.tagline}</h1>
          <p className="mt-6 max-w-xl text-[16px] leading-relaxed text-muted sm:text-[17px]">{p.blurb}</p>
          <div className="mt-8 flex flex-col gap-3 sm:flex-row sm:items-center">
            {p.dashboard && (
              <Link href={p.dashboard} className={PRIMARY}>
                Open in dashboard <ArrowRight className="size-3.5" strokeWidth={1.5} />
              </Link>
            )}
            <InstallCommand className="sm:max-w-[21rem]" />
          </div>
          <p className="mt-8 max-w-xl text-[13px] leading-relaxed text-subtle">
            Instead of <span className="text-muted">{p.insteadOf}</span>.
          </p>
        </div>
        <div className="rise [animation-delay:120ms]">
          <Picture p={p} />
        </div>
      </div>
    </section>
  );
}

/** A measured number, and under it the same thing drawn beside what it was measured against, where there's one. */
function FactCell({ f }: { f: Fact }) {
  const a = f.against;
  const most = a ? Math.max(a.ours, a.theirs) : 1;
  return (
    <div className="flex flex-col bg-bg py-9 sm:px-8 sm:py-11 sm:first:pl-0 sm:last:pr-0">
      <CountUp value={f.value} className="block font-mono text-[2.75rem] leading-none font-medium tracking-[-0.05em] tabular-nums sm:text-[3.5rem]" />
      <p className="mt-4 max-w-[18rem] text-[14px] leading-snug text-muted">{f.label}</p>
      {a && (
        <Versus
          className="mt-auto pt-7"
          rows={[
            { name: "roll", value: f.value, share: a.ours / most, own: true },
            { name: a.name, value: a.value, share: a.theirs / most },
          ]}
        />
      )}
    </div>
  );
}

export function ProductFacts({ p }: { p: Product }) {
  if (!p.facts) return null;
  return (
    <section className="border-b border-line">
      <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] divide-y divide-line sm:grid-cols-3 sm:divide-x sm:divide-y-0`}>
        {p.facts.map((f) => (
          <FactCell key={f.label} f={f} />
        ))}
      </div>
      {p.factsNote && (
        <div className="border-t border-line">
          <p className={`${WRAP} flex flex-wrap items-baseline gap-x-3 gap-y-1 py-3.5 text-[12px] text-subtle`}>
            <span className="font-mono text-[10.5px] tracking-wide uppercase">Measured</span>
            {p.factsNote}
          </p>
        </div>
      )}
    </section>
  );
}

/** What it speaks, as a short list, beside the code that connects with it. */
export function ProductConnect({ p }: { p: Product }) {
  const snippets = snippetsFor(p.slug);
  const rows = [
    ["Speaks", p.compat],
    ["Connect with", p.connect],
    ["Instead of", p.insteadOf],
  ];
  return (
    <section className={`${WRAP} py-24 sm:py-32`}>
      <div className="grid grid-cols-[minmax(0,1fr)] items-start gap-12 lg:grid-cols-[minmax(0,5fr)_minmax(0,7fr)] lg:gap-20">
        <Reveal>
          <p className={EYEBROW}>Compatibility</p>
          <h2 className={`mt-4 ${H2}`}>{p.headline}</h2>
          <dl className="mt-10 border-t border-line">
            {rows.map(([k, v]) => (
              <div key={k} className="grid grid-cols-[7.5rem_minmax(0,1fr)] gap-4 border-b border-line py-4">
                <dt className="pt-0.5 font-mono text-[10.5px] tracking-wide text-subtle uppercase">{k}</dt>
                <dd className="text-[14.5px] leading-snug text-fg">{v}</dd>
              </div>
            ))}
          </dl>
        </Reveal>
        <Reveal delay={120}>
          <CodePanel snippets={snippets} />
        </Reveal>
      </div>
    </section>
  );
}

/** The product's own dashboard page: the real render, with the tour's camera. */
export function ProductDashboard({ p }: { p: Product }) {
  const shot = shotFor(p.slug);
  if (!shot) return null;
  return (
    <section className={`${WRAP} pb-24 sm:pb-32`}>
      <Reveal className="mx-auto flex max-w-3xl flex-col items-center text-center">
        <p className={EYEBROW}>In the dashboard</p>
        <h2 className={`mt-4 ${H2}`}>{p.what}</h2>
        {p.dashboard && (
          <Link href={p.dashboard} className="group mt-6 inline-flex items-center gap-1.5 text-[14px] text-muted transition-colors hover:text-fg">
            Open it
            <ArrowUpRight className="size-3.5 transition-transform group-hover:translate-x-0.5 group-hover:-translate-y-0.5" strokeWidth={1.5} />
          </Link>
        )}
      </Reveal>
      <Reveal delay={100} className="mt-12 sm:mt-14">
        <Tour pages={[shot]} />
      </Reveal>
    </section>
  );
}

/** Each product's interactive picture, and the line that introduces it. */
const DEMOS: Record<NonNullable<Product["visual"]>, { title: string; lede: string; render: (p: Product) => ReactNode }> = {
  branch: { title: "Branch it yourself", lede: "Hover main's history and click to branch from that moment. The command under it is the one you'd run.", render: (p) => <BranchTimeline product={p.slug} /> },
  zero: { title: "Scale to zero", lede: "Send it a request, then leave it alone for a few seconds, and watch what it bills against a server that never sleeps.", render: () => <ScaleSim /> },
  stream: { title: "A topic, live", lede: "Choose how many producers to run and watch the consumers keep up. The steps under it point at each part.", render: () => <StreamFlow /> },
  queue: { title: "The merge queue", lede: "Push a change and follow it through its batch's checks onto main, or out of the queue.", render: () => <MergeQueue /> },
  agents: { title: "Threads, machines, one question", lede: "Drag the threads around, read any of them, and answer the one that's waiting.", render: () => <AgentsCanvas /> },
};

/** The product's demo, on a band of its own. */
export function ProductDemo({ p }: { p: Product }) {
  const demo = p.visual && DEMOS[p.visual];
  if (!demo) return null;
  return (
    <section className="border-y border-line bg-panel">
      <div className={`${WRAP} py-24 sm:py-28`}>
        <Reveal className="flex flex-col gap-5 lg:flex-row lg:items-end lg:justify-between lg:gap-20">
          <div>
            <p className={EYEBROW}>Try it</p>
            <h2 className={`mt-4 ${H2}`}>{demo.title}</h2>
          </div>
          <p className="max-w-md text-[16px] leading-relaxed text-muted lg:pb-1.5">{demo.lede}</p>
        </Reveal>
        <Reveal delay={120} className="mt-12">
          <Scene>{demo.render(p)}</Scene>
        </Reveal>
      </div>
    </section>
  );
}

/**
 * What you get: the heading stays in its column while the features pass, numbered, two to a row
 * and split by the grid's own hairlines. What isn't there yet is said under the heading.
 */
export function ProductFeatures({ p }: { p: Product }) {
  return (
    <section className={`${WRAP} py-24 sm:py-32`}>
      <div className="grid grid-cols-[minmax(0,1fr)] gap-12 lg:grid-cols-[minmax(0,4fr)_minmax(0,8fr)] lg:gap-20">
        <Reveal className="lg:sticky lg:top-28 lg:self-start">
          <p className={EYEBROW}>What you get</p>
          <h2 className={`mt-4 ${H2}`}>{shotFor(p.slug) ? `Everything in ${p.name}.` : p.what}</h2>
          {p.status && (
            <div className="mt-10 border-t border-line pt-5">
              <p className="font-mono text-[10.5px] tracking-wide text-subtle uppercase">Not yet</p>
              <p className="mt-2 max-w-sm text-[13.5px] leading-relaxed text-muted">{p.status}</p>
            </div>
          )}
        </Reveal>
        <Reveal delay={100}>
          <ul className="grid grid-cols-[minmax(0,1fr)] gap-px overflow-hidden rounded-2xl border border-line bg-line sm:grid-cols-2">
            {p.features.map((f, i) => (
              <li key={f.title} className="flex flex-col bg-bg p-6 sm:p-8">
                <div className="flex items-center justify-between">
                  <span className="grid size-9 place-items-center rounded-[10px] border border-line-2 bg-panel">
                    <f.icon className="size-[17px]" strokeWidth={1.5} />
                  </span>
                  <span className="font-mono text-[11px] text-subtle tabular-nums">{String(i + 1).padStart(2, "0")}</span>
                </div>
                <h3 className="mt-8 text-[18px] font-medium tracking-[-0.02em]">{f.title}</h3>
                <p className="mt-2 text-[14.5px] leading-relaxed text-muted text-pretty">{f.body}</p>
              </li>
            ))}
          </ul>
        </Reveal>
      </div>
    </section>
  );
}

/** The last word: the product's own commands in a terminal you can type in, and the way to get it. */
export function ProductStart({ p }: { p: Product }) {
  return (
    <section className="border-t border-line">
      <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] items-center gap-12 py-24 sm:py-28 lg:grid-cols-[minmax(0,5fr)_minmax(0,7fr)] lg:gap-20`}>
        <Reveal>
          <p className={EYEBROW}>Start</p>
          <h2 className={`mt-4 ${H2}`}>Try it from a terminal.</h2>
          <p className="mt-5 max-w-md text-[16px] leading-relaxed text-muted">
            One install gets you <span className="font-mono text-[0.9em] text-fg">roll</span> and every product. Or type the commands here first: Tab completes them.
          </p>
          <div className="mt-8 flex flex-col gap-3 sm:flex-row sm:items-center">
            <InstallCommand className="sm:max-w-[21rem]" />
            {p.dashboard && (
              <Link href={p.dashboard} className="group inline-flex shrink-0 items-center gap-1.5 px-1 text-[14px] text-muted transition-colors hover:text-fg">
                Open in dashboard
                <ArrowRight className="size-3.5 transition-transform group-hover:translate-x-0.5" strokeWidth={1.5} />
              </Link>
            )}
          </div>
        </Reveal>
        <Reveal delay={120}>
          <Terminal scenes={p.scenes} />
        </Reveal>
      </div>
    </section>
  );
}

/** The rest of the family as cards that fill their rows: two wide, three as one wide and two, four as they are. */
const alongside = (slugs: string[]) => slugs.map((s, i) => (slugs.length === 2 || (slugs.length === 3 && i === 0) ? `*${s}` : s));

export function ProductRelated({ p }: { p: Product }) {
  const group = GROUPS.find((g) => g.id === p.group)!;
  const related = PRODUCTS.filter((o) => o.group === p.group && o.slug !== p.slug);
  if (related.length === 0) return null;
  return (
    <section className="border-t border-line">
      <div className={`${WRAP} py-20 sm:py-24`}>
        <div className="flex items-baseline justify-between gap-6">
          <p className={EYEBROW}>More in {group.name}</p>
          <Link href="/products" className="group inline-flex items-center gap-1.5 text-[13px] text-muted transition-colors hover:text-fg">
            All products
            <ArrowRight className="size-3.5 transition-transform group-hover:translate-x-0.5" strokeWidth={1.5} />
          </Link>
        </div>
        <Bento items={alongside(related.map((o) => o.slug))} className="mt-6" />
      </div>
    </section>
  );
}
