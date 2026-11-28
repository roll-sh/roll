import { Link } from "@/components/link";
import { GROUPS, PRODUCTS, productHref, type GroupId } from "@/lib/marketing";
import { Bento, type BentoItem } from "./bento";
import { Reveal } from "./fx";
import { EYEBROW, WRAP } from "./layout";

/** Each family's cards, three columns wide, laid out so every row is full. */
const LAYOUT: Record<GroupId, BentoItem[]> = {
  data: ["*postgres", "redis", "search", "storage", "nosql"],
  messaging: ["streams", "queues", "tasks"],
  build: ["*apps", "previews", "agents", "*code"],
  platform: ["secrets", "access", "gateway"],
};

const number = (i: number) => String(i + 1).padStart(2, "0");

/**
 * Every product at a glance, under the hero: a column per family, a line per product with what it
 * speaks. It's the page's contents, and the fastest answer to "what does roll do".
 */
export function Directory() {
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-px overflow-hidden rounded-2xl border border-line bg-line sm:grid-cols-2 xl:grid-cols-4">
      {GROUPS.map((g, i) => (
        <div key={g.id} className="bg-bg p-5 sm:p-6">
          <a href={`#${g.id}`} className={`flex items-baseline justify-between transition-colors hover:text-fg ${EYEBROW}`}>
            <span>
              <span className="text-muted">{number(i)}</span> · {g.name}
            </span>
            <span aria-hidden>↓</span>
          </a>
          <ul className="mt-4 -mx-2">
            {PRODUCTS.filter((p) => p.group === g.id).map((p) => (
              <li key={p.slug}>
                <Link href={productHref(p.slug)} className="group flex items-start gap-3 rounded-lg px-2 py-2 transition-colors duration-150 hover:bg-panel-2">
                  <p.icon className="mt-0.5 size-4 shrink-0 text-muted transition-colors group-hover:text-fg" strokeWidth={1.5} />
                  <span className="min-w-0">
                    <span className="block text-[14px] leading-tight text-fg">{p.name}</span>
                    <span className="mt-1 block font-mono text-[11px] leading-snug text-subtle">{p.compat}</span>
                  </span>
                </Link>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </div>
  );
}

/**
 * The families, one after another: each one's number, name and line stay in a column on the left
 * while its cards pass on the right, with the page's contents under them.
 */
export function Families() {
  return (
    <>
      {GROUPS.map((g, i) => {
        const products = PRODUCTS.filter((p) => p.group === g.id);
        return (
          <section key={g.id} id={g.id} className="scroll-mt-16 border-t border-line">
            <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] gap-10 py-20 sm:py-24 xl:grid-cols-[minmax(0,3fr)_minmax(0,9fr)] xl:gap-12`}>
              <Reveal className="xl:sticky xl:top-28 xl:self-start">
                <p className={EYEBROW}>
                  <span className="text-muted">{number(i)}</span> · {products.length} products
                </p>
                <h2 className="mt-4 text-4xl leading-[1.02] font-semibold tracking-[-0.05em] sm:text-[3.4rem] xl:text-[2.75rem]">{g.name}</h2>
                <p className="mt-4 max-w-sm text-[16px] leading-relaxed text-muted">{g.lede}</p>
                <nav aria-label="Product families" className="mt-10 hidden border-t border-line pt-5 xl:block">
                  <ul className="space-y-2.5">
                    {GROUPS.map((o, k) => (
                      <li key={o.id}>
                        <a
                          href={`#${o.id}`}
                          aria-current={o.id === g.id ? "true" : undefined}
                          className={`flex items-center gap-3 font-mono text-[11px] tracking-wide uppercase transition-colors duration-200 hover:text-fg ${o.id === g.id ? "text-fg" : "text-subtle"}`}
                        >
                          <span className={`h-px transition-all duration-300 ${o.id === g.id ? "w-6 bg-fg" : "w-3 bg-line-2"}`} />
                          {number(k)} {o.name}
                        </a>
                      </li>
                    ))}
                  </ul>
                </nav>
              </Reveal>
              <Reveal delay={100}>
                <Bento items={LAYOUT[g.id]} columns={3} />
              </Reveal>
            </div>
          </section>
        );
      })}
    </>
  );
}
