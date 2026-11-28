import { ArrowRight, ArrowUpRight } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { Link } from "@/components/link";
import { productBySlug, productHref } from "@/lib/marketing";
import { AgentsCanvas } from "./agents-canvas";
import { DropOutArt, GitArt } from "./art/code-tiles";
import { KeyRulesArt, PreviewTestsArt, RolldArt, TracesArt } from "./art/shipped";
import { BranchTimeline } from "./branch-timeline";
import { Reveal } from "./fx";
import { History } from "./how-it-works";
import { useInView } from "./in-view";
import { EYEBROW, H2, WRAP } from "./layout";
import { MergeQueue } from "./merge-queue";
import { Agents } from "./numbers";
import { ScaleVersus } from "./scale-versus";
import { NOTE, Scene, Versus } from "./scene";
import { StreamFlow } from "./stream-flow";

/**
 * The home page's chapters. Each has its own composition, so no two in a row read alike: a story
 * that scrolls past a pinned column, a band with the picture across the whole page, a comparison,
 * a bento, a framed canvas. One idea and one main picture each, and few words.
 */

const product = (slug: string) => productBySlug(slug)!;
const feature = (slug: string, title: string) => product(slug).features.find((f) => f.title === title)!;

const Tag = ({ n, tag }: { n: string; tag: string }) => (
  <p className={EYEBROW}>
    <span className="text-muted">{n}</span> · {tag}
  </p>
);

const More = ({ href, label, className }: { href: string; label: string; className?: string }) => (
  <Link href={href} className={`inline-flex items-center gap-1.5 text-[14px] text-fg transition hover:gap-2.5 ${className ?? ""}`}>
    {label} <ArrowRight className="size-3.5" strokeWidth={1.5} />
  </Link>
);

const STORY: { title: string; body: string; visual: ReactNode }[] = [
  {
    title: "Branch from any moment",
    body: "Now, or a few minutes ago. Click main's history and a branch forks from there, with an endpoint of its own.",
    visual: (
      <Scene>
        <BranchTimeline />
      </Scene>
    ),
  },
  {
    title: "A branch copies nothing",
    body: "It points at the objects its parent already has and keeps only what it writes. That's why a Postgres branch is ready in 205 ms.",
    visual: (
      <div className="art rounded-2xl border border-line p-6 sm:p-10">
        <History className="max-sm:hidden" />
        <History narrow className="sm:hidden" />
      </div>
    ),
  },
  {
    title: "Asleep until asked",
    body: "Nothing runs for a database nobody's querying. The first connection wakes it.",
    visual: (
      <div className="rounded-2xl border border-line p-6 sm:p-10">
        <p className={`mb-6 ${NOTE}`}>Asleep to the first answer, on one second</p>
        <Versus
          big
          rows={[
            { name: "Postgres", value: "0.66 s", share: 0.66, own: true },
            { name: "NoSQL", value: "20 ms", share: 0.02, own: true },
            { name: "Search", value: "19.8 ms", share: 0.0198, own: true },
          ]}
        />
      </div>
    ),
  },
];

/**
 * 01: a story told down the page. The headline and the story's steps stay pinned on the left
 * while each step's picture scrolls past on the right; the step in the middle of the screen is
 * the one marked. On a phone it's the steps, one under the other.
 */
export function DataChapter() {
  const steps = useRef<(HTMLDivElement | null)[]>([]);
  const [at, setAt] = useState(0);
  useEffect(() => {
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) if (e.isIntersecting) setAt(steps.current.indexOf(e.target as HTMLDivElement));
      },
      { rootMargin: "-45% 0px -45% 0px" },
    );
    steps.current.forEach((el) => el && io.observe(el));
    return () => io.disconnect();
  }, []);

  return (
    <section id="data" className="scroll-mt-14 border-t border-line">
      <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] gap-x-20 gap-y-14 py-24 sm:py-32 lg:grid-cols-[minmax(0,21rem)_minmax(0,1fr)]`}>
        <div className="lg:sticky lg:top-28 lg:self-start">
          <Tag n="01" tag="Data" />
          <h2 className={`mt-4 ${H2}`}>Databases that branch and sleep.</h2>
          <p className="mt-5 text-[16px] leading-relaxed text-muted text-pretty">Postgres, Redis, a Cassandra-compatible store, search and S3 buckets, each in its own protocol.</p>
          <ol className="mt-10 hidden border-l border-line lg:block">
            {STORY.map((s, i) => (
              <li key={s.title}>
                <button
                  onClick={() => steps.current[i]?.scrollIntoView({ block: "center", behavior: "smooth" })}
                  aria-current={i === at ? "step" : undefined}
                  className={`-ml-px flex w-full items-baseline gap-3 border-l py-2.5 pl-5 text-left text-[15px] transition-colors duration-300 ${
                    i === at ? "border-fg text-fg" : "border-transparent text-subtle hover:text-muted"
                  }`}
                >
                  <span className="font-mono text-[10.5px] tabular-nums">{String(i + 1).padStart(2, "0")}</span>
                  {s.title}
                </button>
              </li>
            ))}
          </ol>
          <More href={productHref("postgres")} label="Branching in Postgres" className="mt-10" />
        </div>

        <div className="space-y-20 lg:space-y-32 lg:py-10">
          {STORY.map((s, i) => (
            <div
              key={s.title}
              ref={(el) => {
                steps.current[i] = el;
              }}
            >
              <p className="font-mono text-[11px] text-subtle tabular-nums">{String(i + 1).padStart(2, "0")}</p>
              <h3 className="mt-2 text-2xl font-semibold tracking-[-0.03em] text-fg">{s.title}</h3>
              <p className="mt-2 max-w-xl text-[15px] leading-relaxed text-muted text-pretty">{s.body}</p>
              <div className="mt-7">{s.visual}</div>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}

const MESSAGING = ["streams", "queues", "tasks"];

/** 02: a band across the page. Few words in the middle, the stream running edge to edge under them. */
export function MessagingChapter() {
  return (
    <section id="messaging" className="scroll-mt-14 border-t border-line bg-panel">
      <Reveal className={`${WRAP} pt-24 text-center sm:pt-32`}>
        <Tag n="02" tag="Messaging" />
        <h2 className={`mx-auto mt-4 max-w-3xl ${H2}`}>Streams with no bottom.</h2>
        <p className="mx-auto mt-5 max-w-xl text-[16.5px] leading-relaxed text-muted text-pretty">
          Kafka-compatible topics whose sealed segments drop into object storage, so retention is a policy, not a disk.
        </p>
      </Reveal>
      <StreamFlow bleed className="mt-12" />
      <div className={`${WRAP} pt-16 pb-24 sm:pb-32`}>
        <div className="mx-auto grid max-w-5xl grid-cols-[minmax(0,1fr)] gap-x-12 gap-y-8 sm:grid-cols-3">
          {MESSAGING.map((slug) => {
            const p = product(slug);
            return (
              <Link key={slug} href={productHref(slug)} className="group">
                <p className="flex items-center gap-2 text-[15px] font-medium text-fg">
                  <p.icon className="size-4 text-muted" strokeWidth={1.5} />
                  {p.name}
                  <ArrowUpRight className="size-3.5 text-subtle transition group-hover:translate-x-0.5 group-hover:-translate-y-0.5 group-hover:text-fg" strokeWidth={1.5} />
                </p>
                <p className="mt-2 text-[14px] leading-relaxed text-muted text-pretty">{p.what}</p>
              </Link>
            );
          })}
        </div>
      </div>
    </section>
  );
}

/** 03: a comparison. The headline over two halves that get the same traffic and bill differently. */
export function HostingChapter() {
  return (
    <section id="run" className="scroll-mt-14 border-t border-line">
      <div className={`${WRAP} py-24 sm:py-32`}>
        <Reveal className="flex flex-wrap items-end justify-between gap-x-10 gap-y-6">
          <div>
            <Tag n="03" tag="Hosting" />
            <h2 className={`mt-4 max-w-3xl ${H2}`}>Nothing runs when nothing's asked.</h2>
          </div>
          <More href={productHref("apps")} label="roll apps" className="pb-2" />
        </Reveal>
        <Reveal delay={100} className="mt-12">
          <ScaleVersus />
        </Reveal>
      </div>
    </section>
  );
}

/** A small tile of the code bento: a drawing, and the one thing it says. */
function Tile({ title, body, children }: { title: string; body: string; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const live = useInView(ref);
  return (
    <div ref={ref} data-live={live} className="art flex flex-col rounded-2xl border border-line bg-panel p-6 sm:p-7">
      <div className="mx-auto w-full max-w-[26rem] flex-1 content-center py-4">{children}</div>
      <h3 className="mt-6 text-[16px] font-medium tracking-[-0.015em] text-fg">{title}</h3>
      <p className="mt-1.5 text-[13.5px] leading-relaxed text-muted text-pretty">{body}</p>
    </div>
  );
}

/** 04: a bento. The headline sits inside the big tile, over the merge queue; three small tiles each draw one more thing. */
export function CodeChapter() {
  return (
    <section id="code" className="scroll-mt-14 border-t border-line">
      <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] gap-3 py-24 sm:gap-4 sm:py-32 lg:grid-cols-3`}>
        <Reveal className="overflow-hidden rounded-2xl border border-line lg:col-span-3">
          <div className="flex flex-wrap items-end justify-between gap-x-10 gap-y-5 p-6 sm:p-10">
            <div>
              <Tag n="04" tag="roll code" />
              <h2 className={`mt-4 ${H2}`}>Keep the code rolling.</h2>
              <p className="mt-4 max-w-lg text-[16px] leading-relaxed text-muted text-pretty">Agents push all day. A merge queue keeps main green.</p>
            </div>
            <More href={productHref("code")} label="roll code" className="pb-1" />
          </div>
          <MergeQueue className="border-t border-line" />
        </Reveal>
        <Tile title="Merges happen on the server" body={feature("code", "Lands in O(change)").body}>
          <Agents />
        </Tile>
        <Tile title="A failure drops out" body={feature("code", "A merge queue").body}>
          <DropOutArt />
        </Tile>
        <Tile title="git, derived" body={feature("code", "git, derived").body}>
          <GitArt />
        </Tile>
      </div>
    </section>
  );
}

/** 05: a framed canvas on a band, with its few words beside it. */
export function AgentsChapter() {
  return (
    <section id="agents" className="scroll-mt-14 border-t border-line bg-panel">
      <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] items-center gap-x-14 gap-y-12 py-24 sm:py-32 xl:grid-cols-[minmax(0,1fr)_15rem]`}>
        <Reveal className="xl:order-2">
          <Tag n="05" tag="Agents" />
          <h2 className="mt-4 text-4xl leading-[1.05] font-semibold tracking-[-0.045em] text-balance xl:text-[2.5rem]">Every agent you run, on one canvas.</h2>
          <p className="mt-5 max-w-md text-[16px] leading-relaxed text-muted text-pretty">Claude Code, Codex and ACP agents on your own machines. Drag the threads, and answer the one that's waiting.</p>
          <More href={productHref("agents")} label="roll agents" className="mt-6" />
        </Reveal>
        <Reveal delay={100} className="overflow-hidden rounded-2xl border border-line-2 bg-line">
          <AgentsCanvas />
        </Reveal>
      </div>
    </section>
  );
}

/** Recently landed, from the repo's history, each with a drawing of what it is. */
const SHIPPED: { date: string; product: string; title: string; body: string; art: () => ReactNode }[] = [
  {
    date: "Nov 28",
    product: "previews",
    title: "Your own browser tests on previews",
    body: "Run your Playwright or Cypress tests against a preview; snapshots go to its visual review.",
    art: PreviewTestsArt,
  },
  { date: "Nov 27", product: "apps", title: "rolld ships with the CLI", body: "Checksummed Linux and macOS builds. install.sh --with-rolld runs the daemon yourself.", art: RolldArt },
  { date: "Nov 27", product: "search", title: "OpenTelemetry traces in Search", body: "Spans land in traces-*.otel-*, and their events as log lines.", art: TracesArt },
  { date: "Nov 27", product: "secrets", title: "Key-scoped rules", body: "A grant that reaches the keys it names, and nothing else in the environment.", art: KeyRulesArt },
];

/** What shipped lately: one row of cards, each a drawing, when, which product and what it is. Below `xl` the row scrolls sideways. */
export function Shipped() {
  return (
    <div className="-mx-5 flex snap-x snap-mandatory gap-3 overflow-x-auto px-5 [scrollbar-width:none] sm:-mx-8 sm:gap-4 sm:px-8 xl:mx-0 xl:grid xl:grid-cols-4 xl:overflow-visible xl:px-0">
      {SHIPPED.map((s) => (
        <ShippedCard key={s.title} {...s} />
      ))}
    </div>
  );
}

function ShippedCard({ date, product: slug, title, body, art: Drawing }: (typeof SHIPPED)[number]) {
  const ref = useRef<HTMLDivElement>(null);
  const live = useInView(ref);
  const p = product(slug);
  return (
    <div ref={ref} data-live={live} className="art w-[19rem] shrink-0 snap-start xl:w-auto">
      <Link href={productHref(slug)} className="group flex h-full flex-col overflow-hidden rounded-2xl border border-line bg-panel transition-colors duration-200 hover:border-line-2">
        <div className="border-b border-line bg-bg px-5 py-6 sm:px-6">
          <Drawing />
        </div>
        <div className="flex flex-1 flex-col p-5 sm:p-6">
          <p className="flex items-center gap-2 font-mono text-[11px] text-subtle">
            <p.icon className="size-3.5 text-muted" strokeWidth={1.5} />
            <span className="text-muted">{p.name}</span>
            <span>· {date}</span>
            <ArrowUpRight className="ml-auto size-4 text-subtle transition group-hover:translate-x-0.5 group-hover:-translate-y-0.5 group-hover:text-fg" strokeWidth={1.5} />
          </p>
          <h3 className="mt-4 text-[16px] font-medium tracking-[-0.015em] text-fg">{title}</h3>
          <p className="mt-1.5 text-[13.5px] leading-relaxed text-muted text-pretty">{body}</p>
        </div>
      </Link>
    </div>
  );
}
