import { ArrowRight, ArrowUpRight, Terminal } from "lucide-react";
import { useState, type ReactNode } from "react";
import { Link } from "@/components/link";
import { GROUPS, PRODUCTS, productBySlug, productHref, type Product } from "@/lib/marketing";
import { AgentsArt, AppsArt, CodeArt, PreviewsArt } from "./art/build";
import { NosqlArt, PostgresArt, RedisArt, SearchArt, StorageArt } from "./art/data";
import { QueuesArt, StreamsArt, TasksArt } from "./art/messaging";
import { AccessArt, GatewayArt, SecretsArt } from "./art/platform";
import { InstallCommand } from "./install";
import { EYEBROW, WRAP } from "./layout";
import { DEVELOPERS, FEATURED, transcript, type NavLink } from "./nav";

/** Each product's drawing, the bento's own (art/), for the Products menu's tile. */
const ART: Record<string, () => ReactNode> = {
  postgres: PostgresArt,
  redis: RedisArt,
  nosql: NosqlArt,
  search: SearchArt,
  storage: StorageArt,
  streams: StreamsArt,
  queues: QueuesArt,
  tasks: TasksArt,
  apps: AppsArt,
  previews: PreviewsArt,
  code: CodeArt,
  agents: AgentsArt,
  secrets: SecretsArt,
  access: AccessArt,
  gateway: GatewayArt,
};

/** The ring a keyboard's focus draws, the same on every link of the chrome. */
export const FOCUS = "outline-none focus-visible:ring-2 focus-visible:ring-accent/70";

const ROW = `group/item flex items-start gap-3 rounded-lg px-2.5 py-1.5 transition-colors duration-150 hover:bg-panel-2 ${FOCUS}`;
const TILE = "grid size-8 shrink-0 place-items-center rounded-md border border-line bg-panel text-muted transition-colors duration-150 group-hover/item:border-line-2 group-hover/item:text-fg";
/** Both menus share one grid, so the band under them only changes its height. The tile joins it from `xl`. */
const GRID = `${WRAP} grid grid-cols-[minmax(0,1fr)] gap-12 pt-7 pb-8 xl:grid-cols-[minmax(0,1fr)_22rem]`;
const ASIDE = "hidden flex-col overflow-hidden rounded-xl border border-line bg-panel xl:flex";
const FOOT = "mt-6 flex items-center justify-between gap-6 border-t border-line pt-5 text-[13px]";

function Row({ icon: Icon, title, body, href, external, onPoint }: Omit<NavLink, "icon"> & { icon: NavLink["icon"]; onPoint?: () => void }) {
  const inner = (
    <>
      <span className={TILE}>
        <Icon className="size-4" strokeWidth={1.5} />
      </span>
      <span className="min-w-0 pt-px">
        <span className="flex items-center gap-1 text-[13.5px] font-medium tracking-[-0.01em] text-fg">
          {title}
          {external && <ArrowUpRight className="size-3 text-subtle" strokeWidth={1.5} />}
        </span>
        <span className="mt-0.5 block text-[12.5px] leading-snug text-subtle text-pretty transition-colors duration-150 group-hover/item:text-muted">{body}</span>
      </span>
    </>
  );
  return external ? (
    <a href={href} target="_blank" rel="noreferrer" className={ROW}>
      {inner}
    </a>
  ) : (
    <Link href={href} className={ROW} onPointerEnter={onPoint} onFocus={onPoint}>
      {inner}
    </Link>
  );
}

/** Every product by family, and a tile that draws the one being pointed at. */
export function ProductsPanel({ live }: { live: boolean }) {
  const [pointed, setPointed] = useState<string | null>(null);
  const shown = productBySlug(pointed ?? FEATURED.slug)!;
  return (
    <div className={GRID}>
      <div className="flex min-w-0 flex-col">
        <div className="grid grid-cols-4 gap-x-4 xl:gap-x-6">
          {GROUPS.map((g) => (
            <div key={g.id} className="min-w-0">
              <p className={`px-2.5 ${EYEBROW}`}>{g.name}</p>
              <ul className="mt-3">
                {PRODUCTS.filter((p) => p.group === g.id).map((p) => (
                  <li key={p.slug}>
                    <Row icon={p.icon} title={p.name} body={p.compat} href={productHref(p.slug)} onPoint={() => setPointed(p.slug)} />
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
        <div className={`mt-auto ${FOOT}`}>
          <Link href="/products" className={`group/all inline-flex items-center gap-1.5 rounded-sm text-fg ${FOCUS}`}>
            All products <ArrowRight className="size-3.5 transition-transform duration-200 group-hover/all:translate-x-0.5" strokeWidth={1.5} />
          </Link>
          <span className="font-mono text-[11px] text-subtle">{PRODUCTS.length} products · one daemon</span>
        </div>
      </div>
      <Featured p={shown} fresh={shown.slug === FEATURED.slug && FEATURED.fresh} live={live} />
    </div>
  );
}

function Featured({ p, fresh, live }: { p: Product; fresh: boolean; live: boolean }) {
  const Drawing = ART[p.slug];
  return (
    <Link
      href={productHref(p.slug)}
      tabIndex={-1}
      aria-hidden
      className={`group/tile transition-colors duration-200 hover:border-line-2 ${ASIDE}`}
    >
      <div data-live={live} className="art flex min-h-[13.5rem] flex-1 items-center overflow-hidden border-b border-line px-3 [mask-image:linear-gradient(to_right,black_90%,transparent)]">
        <div key={p.slug} className="nav-swap w-full">
          {Drawing ? <Drawing /> : null}
        </div>
      </div>
      <div key={`${p.slug}-words`} className="nav-swap flex min-h-[6.5rem] flex-col p-4">
        <p className="flex items-center gap-2 text-[13.5px] font-medium tracking-[-0.01em] text-fg">
          <p.icon className="size-4 text-muted" strokeWidth={1.5} />
          {p.name}
          {fresh && <span className="rounded-full bg-accent/15 px-1.5 py-px text-[10.5px] font-medium text-accent">New</span>}
          <ArrowUpRight className="ml-auto size-3.5 text-subtle transition duration-200 group-hover/tile:translate-x-0.5 group-hover/tile:-translate-y-0.5 group-hover/tile:text-fg" strokeWidth={1.5} />
        </p>
        <p className="mt-1.5 text-[12.5px] leading-snug text-muted text-pretty">
          {p.what} <span className="text-subtle">Instead of {p.insteadOf}.</span>
        </p>
      </div>
    </Link>
  );
}

/** Where to start, what to read, what to wire in, and the CLI in a terminal. */
export function DevelopersPanel() {
  return (
    <div className={GRID}>
      <div className="flex min-w-0 flex-col">
        <div className="grid grid-cols-4 gap-x-4 xl:gap-x-6">
          {DEVELOPERS.map((col) => (
            <div key={col.name} className="min-w-0">
              <p className={`px-2.5 ${EYEBROW}`}>{col.name}</p>
              <ul className="mt-3">
                {col.links.map((l) => (
                  <li key={l.title}>
                    <Row {...l} />
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
        <div className={`mt-auto ${FOOT}`}>
          <InstallCommand className="max-w-[26rem]" />
          <span className="font-mono text-[11px] whitespace-nowrap text-subtle">macOS · Linux · Windows</span>
        </div>
      </div>
      <Cli />
    </div>
  );
}

/** The CLI as you'd meet it: a few commands, each with what it answers. */
const SESSION = [...transcript("postgres").slice(0, 1), ...transcript("redis")];

function Cli() {
  return (
    <div className={ASIDE}>
      <p className="flex items-center gap-2 border-b border-line px-4 py-2.5 font-mono text-[11px] text-subtle">
        <Terminal className="size-3.5" strokeWidth={1.5} />
        roll CLI
      </p>
      <div className="flex-1 space-y-2 px-4 py-4 font-mono text-[12px] leading-relaxed whitespace-pre">
        {SESSION.map(({ cmd, out }) => (
          <div key={cmd}>
            <p className="truncate text-fg">
              <span className="text-subtle select-none">$ </span>
              {cmd}
            </p>
            <p className="truncate text-muted">{out}</p>
          </div>
        ))}
      </div>
      <p className="flex items-center gap-2 border-t border-line px-4 py-2.5 font-mono text-[11px] text-subtle">
        <span className="size-1.5 rounded-full bg-accent" />
        one CLI for every product
      </p>
    </div>
  );
}

/**
 * The phone's menu, a sheet under the bar: every product by family, the developers' links, and
 * the two ways in pinned at the bottom.
 */
export function MobileSheet({ open, id, onNavigate }: { open: boolean; id: string; onNavigate: () => void }) {
  const item = `flex items-center gap-2.5 rounded-lg py-2.5 text-[15px] text-fg ${FOCUS}`;
  return (
    <div
      id={id}
      inert={!open}
      onClick={(e) => (e.target as HTMLElement).closest("a") && onNavigate()}
      className={`fixed inset-x-0 top-16 bottom-0 z-40 flex flex-col bg-bg transition-[opacity,transform,visibility] duration-200 ease-out-soft motion-reduce:transform-none motion-reduce:transition-none lg:hidden ${
        open ? "visible translate-y-0 opacity-100" : "invisible -translate-y-2 opacity-0"
      }`}
    >
      <nav aria-label="Site" className="flex-1 overflow-y-auto overscroll-contain px-5 pt-3 pb-8 sm:px-8">
        {GROUPS.map((g) => (
          <section key={g.id} className="border-b border-line py-4">
            <p className={EYEBROW}>{g.name}</p>
            <ul className="mt-1.5 grid grid-cols-2 gap-x-4">
              {PRODUCTS.filter((p) => p.group === g.id).map((p) => (
                <li key={p.slug}>
                  <Link href={productHref(p.slug)} className={item}>
                    <p.icon className="size-4 shrink-0 text-muted" strokeWidth={1.5} />
                    {p.name}
                  </Link>
                </li>
              ))}
            </ul>
          </section>
        ))}
        <section className="py-4">
          <p className={EYEBROW}>Developers</p>
          <ul className="mt-1.5 grid grid-cols-2 gap-x-4">
            <li>
              <Link href="/products" className={item}>
                All products
              </Link>
            </li>
            {DEVELOPERS.flatMap((c) => c.links)
              .filter((l) => l.href !== "/signin")
              .map((l) => (
                <li key={l.title}>
                  {l.external ? (
                    <a href={l.href} target="_blank" rel="noreferrer" className={item}>
                      {l.title}
                      <ArrowUpRight className="size-3.5 text-subtle" strokeWidth={1.5} />
                    </a>
                  ) : (
                    <Link href={l.href} className={item}>
                      {l.title}
                    </Link>
                  )}
                </li>
              ))}
          </ul>
        </section>
        <InstallCommand className="mt-2" />
      </nav>
      <div className="grid grid-cols-2 gap-3 border-t border-line px-5 py-4 sm:px-8">
        <Link href="/signin" className={`rounded-lg border border-line-2 py-2.5 text-center text-[14px] font-medium text-fg ${FOCUS}`}>
          Sign in
        </Link>
        <Link href="/dashboard" className={`rounded-lg bg-fg py-2.5 text-center text-[14px] font-medium text-bg ${FOCUS}`}>
          Start rolling
        </Link>
      </div>
    </div>
  );
}
