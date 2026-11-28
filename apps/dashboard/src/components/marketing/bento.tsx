import { ArrowUpRight, Check } from "lucide-react";
import { useRef, type ReactNode } from "react";
import { Link } from "@/components/link";
import { productBySlug, productHref, type Product } from "@/lib/marketing";
import { AgentsArt, AppsArt, CodeArt, PreviewsArt } from "./art/build";
import { NosqlArt, PostgresArt, RedisArt, SearchArt, StorageArt } from "./art/data";
import { QueuesArt, StreamsArt, TasksArt } from "./art/messaging";
import { AccessArt, GatewayArt, SecretsArt } from "./art/platform";
import { Spotlight } from "./fx";
import { useInView } from "./in-view";

/** Each product's drawing. A product without one still gets its card. */
export const ART: Record<string, () => ReactNode> = {
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

/** A product's slug, with `*` in front for a card two columns wide: `["*postgres", "redis", "storage"]`. */
export type BentoItem = string;

/** Every product, as the home page lays them out: a wide card in each row, alternating sides. */
export const ALL: BentoItem[] = [
  ...["*postgres", "redis", "storage"],
  ...["search", "nosql", "*streams"],
  ...["*apps", "queues", "tasks"],
  ...["previews", "agents", "*code"],
  ...["*secrets", "access", "gateway"],
];

/**
 * Products as a bento grid: four columns on a wide screen (or three), two below. A card is the product's
 * icon and name, one sentence (what it is, bright; what it replaces, muted) and its drawing. A
 * wide card has the sentence on the left with what it speaks and three of its features, and the
 * drawing on the right. On a phone the narrow cards sit two to a row without their drawings.
 * Everything a card says comes from lib/marketing.ts.
 */
export function Bento({ items = ALL, className, columns = 4 }: { items?: BentoItem[]; className?: string; /** Columns on a wide screen: three where the grid shares its row with something else. */ columns?: 3 | 4 }) {
  return (
    <div className={`grid grid-cols-2 gap-3 sm:gap-4 ${columns === 3 ? "xl:grid-cols-3" : "xl:grid-cols-4"} ${className ?? ""}`}>
      {items.map((item) => {
        const wide = item.startsWith("*");
        const p = productBySlug(wide ? item.slice(1) : item);
        return p ? <Card key={p.slug} p={p} wide={wide} /> : null;
      })}
    </div>
  );
}

function Card({ p, wide }: { p: Product; wide: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const live = useInView(ref);
  const Drawing = ART[p.slug];
  return (
    <div ref={ref} data-live={live} className={`art ${wide ? "col-span-2" : ""}`}>
      <Spotlight className="h-full rounded-2xl border border-line bg-panel transition-colors duration-200 hover:border-line-2">
        <Link
          href={productHref(p.slug)}
          className={`group/card flex h-full flex-col overflow-hidden rounded-[inherit] xl:min-h-[24.5rem] ${wide ? "sm:min-h-[21rem] sm:flex-row" : ""}`}
        >
          <div className={`flex flex-col p-4 sm:p-6 xl:p-7 ${wide ? "p-5 sm:w-[44%] sm:shrink-0" : ""}`}>
            <div className="flex items-center gap-2.5">
              <p.icon className="size-[18px] shrink-0 text-fg" strokeWidth={1.5} />
              <h3 className="text-[15px] font-medium tracking-[-0.02em] text-fg sm:text-[17px]">{p.name}</h3>
              <ArrowUpRight
                className="ml-auto size-4 text-subtle opacity-0 transition duration-200 group-hover/card:translate-x-0.5 group-hover/card:-translate-y-0.5 group-hover/card:text-fg group-hover/card:opacity-100"
                strokeWidth={1.5}
              />
            </div>
            <p className={`mt-3 leading-snug text-muted text-pretty sm:mt-4 sm:text-[15px] ${wide ? "text-[15px]" : "text-[13px]"}`}>
              <span className="text-fg">{p.what}</span> <span className={wide ? "" : "hidden sm:inline"}>Instead of {p.insteadOf}.</span>
            </p>
            {wide && (
              <div className="mt-auto hidden pt-8 sm:block">
                <p className="font-mono text-[11px] text-subtle">{p.compat}</p>
                <ul className="mt-3 space-y-1.5">
                  {p.features.slice(0, 3).map((f) => (
                    <li key={f.title} className="flex items-center gap-2 text-[13.5px] text-fg">
                      <Check className="size-3.5 shrink-0 text-muted" strokeWidth={1.5} />
                      {f.title}
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
          {Drawing && (
            <div
              className={
                wide
                  ? "flex min-w-0 flex-1 items-center pb-2 sm:py-6 [mask-image:linear-gradient(to_right,black_93%,transparent)]"
                  : "mt-auto hidden sm:block [mask-image:linear-gradient(to_right,black_93%,transparent)]"
              }
            >
              <div className={`w-full transition-opacity duration-300 ${wide ? "" : "ml-auto max-w-[26rem]"}`}>
                <Drawing />
              </div>
            </div>
          )}
        </Link>
      </Spotlight>
    </div>
  );
}
