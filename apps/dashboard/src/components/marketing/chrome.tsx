import { RollLogo } from "@roll/brand";
import { useLocation } from "@tanstack/react-router";
import { ArrowUpRight, ChevronDown } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type KeyboardEvent, type PointerEvent, type ReactNode } from "react";
import { Link } from "@/components/link";
import { api } from "@/lib/roll-api";
import { GROUPS, PRODUCTS, productHref } from "@/lib/marketing";
import { useMarkState } from "../mark-state";
import { InstallCommand } from "./install";
import { EYEBROW, WRAP } from "./layout";
import { FOOTER, PINNED } from "./nav";
import { DevelopersPanel, FOCUS, MobileSheet, ProductsPanel } from "./nav-panels";

export { InstallCommand };

type MenuId = "products" | "developers";
const MENUS: { id: MenuId; label: string }[] = [
  { id: "products", label: "Products" },
  { id: "developers", label: "Developers" },
];

/** Ms the pointer may be outside the header before its menu closes. */
const LINGER = 140;

const NAV_ITEM = `flex h-9 items-center gap-1 rounded-md px-3 text-[14px] transition-colors duration-150 ${FOCUS}`;

/**
 * The marketing pages' header, the same on every one of them: Horizon, the Products and
 * Developers menus, the two products asked for by name, and the ways in.
 *
 * - At the top of a page it's clear, so the hero's backdrop runs under it; once the page has
 *   moved it takes a hairline and a blurred backdrop.
 * - A menu opens on hover, a click or the keyboard (Enter, Space or the down arrow), as a band
 *   under the bar that changes height between menus instead of closing and opening again. Tab
 *   goes from a trigger into its menu and out the other end; Escape closes it and gives the
 *   focus back.
 * - Below `lg` the menus are one sheet under the bar.
 */
export function MarketingHeader() {
  const { state } = useMarkState();
  const pathname = useLocation({ select: (l) => l.pathname });
  const [open, setOpen] = useState<MenuId | null>(null);
  const [sheet, setSheet] = useState(false);
  const [scrolled, setScrolled] = useState(false);
  const [heights, setHeights] = useState<Record<MenuId, number>>({ products: 0, developers: 0 });
  /** How the open menu was opened: one opened by hover stays open through the click that follows. */
  const via = useRef<"hover" | "press">("press");
  const linger = useRef<ReturnType<typeof setTimeout>>(undefined);
  const bar = useRef<HTMLElement>(null);
  const panels = useRef<Record<MenuId, HTMLDivElement | null>>({ products: null, developers: null });

  const stay = () => clearTimeout(linger.current);
  const close = useCallback(() => {
    clearTimeout(linger.current);
    setOpen(null);
  }, []);
  const leave = () => {
    clearTimeout(linger.current);
    linger.current = setTimeout(() => setOpen(null), LINGER);
  };

  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 8);
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  // A new page closes whatever was open.
  useEffect(() => {
    close();
    setSheet(false);
  }, [pathname, close]);

  // The band is as tall as the open menu, measured: the menus' heights differ, and change with the width.
  useEffect(() => {
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => {
      setHeights({ products: panels.current.products?.offsetHeight ?? 0, developers: panels.current.developers?.offsetHeight ?? 0 });
    });
    for (const el of Object.values(panels.current)) if (el) ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    if (!open && !sheet) return;
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const inside = bar.current?.contains(document.activeElement);
      if (open && inside) bar.current?.querySelector<HTMLElement>(`[data-nav-trigger="${open}"]`)?.focus();
      if (sheet) bar.current?.querySelector<HTMLElement>("[data-nav-toggle]")?.focus();
      close();
      setSheet(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, sheet, close]);

  // The sheet covers the page: the page under it doesn't scroll, and a window made wide closes it.
  useEffect(() => {
    if (!sheet) return;
    const root = document.documentElement;
    const was = root.style.overflow;
    root.style.overflow = "hidden";
    const wide = window.matchMedia("(min-width: 64rem)");
    const onWide = () => wide.matches && setSheet(false);
    wide.addEventListener("change", onWide);
    return () => {
      root.style.overflow = was;
      wide.removeEventListener("change", onWide);
    };
  }, [sheet]);

  const links = (id: MenuId) => Array.from(panels.current[id]?.querySelectorAll<HTMLElement>("a[href]:not([tabindex='-1']), button") ?? []);

  const onTrigger = (id: MenuId) => ({
    onPointerEnter: (e: PointerEvent) => {
      if (e.pointerType !== "mouse") return;
      stay();
      if (open !== id) via.current = "hover";
      setOpen(id);
    },
    onClick: () => {
      stay();
      if (open === id && via.current === "press") return setOpen(null);
      via.current = "press";
      setOpen(id);
    },
    onKeyDown: (e: KeyboardEvent) => {
      const into = e.key === "ArrowDown" || (e.key === "Tab" && !e.shiftKey && open === id);
      if (!into) return;
      e.preventDefault();
      via.current = "press";
      setOpen(id);
      // The menu is hidden (and so can't take the focus) until it has rendered open.
      requestAnimationFrame(() => links(id)[0]?.focus());
    },
  });

  /** Tab runs off either end of a menu back into the bar: before it to its trigger, after it to what follows. */
  const onPanelKey = (id: MenuId) => (e: KeyboardEvent) => {
    if (e.key !== "Tab") return;
    const all = links(id);
    const trigger = bar.current?.querySelector<HTMLElement>(`[data-nav-trigger="${id}"]`);
    if (e.shiftKey && document.activeElement === all[0]) {
      e.preventDefault();
      trigger?.focus();
    } else if (!e.shiftKey && document.activeElement === all.at(-1)) {
      e.preventDefault();
      const items = Array.from(bar.current?.querySelectorAll<HTMLElement>("[data-nav-item]") ?? []);
      items[items.indexOf(trigger!) + 1]?.focus();
      close();
    }
  };

  const solid = open !== null || sheet;
  const surface = solid ? `bg-bg ${sheet ? "border-line" : "border-transparent"}` : scrolled ? "border-line bg-bg/80 backdrop-blur-xl" : "border-transparent bg-transparent";

  return (
    <>
      <header
        ref={bar}
        onPointerEnter={stay}
        onPointerLeave={(e) => e.pointerType === "mouse" && leave()}
        onBlur={(e) => !e.currentTarget.contains(e.relatedTarget) && close()}
        className={`sticky top-0 z-50 border-b transition-[background-color,border-color] duration-200 ${surface}`}
      >
        <div className={`${WRAP} flex h-16 items-center`}>
          <Link href="/" aria-label="roll home" className={`-ml-1 shrink-0 rounded-md p-1 ${FOCUS}`} onPointerEnter={leave}>
            <RollLogo state={state} size={20} />
          </Link>

          <nav aria-label="Site" className="ml-8 hidden items-center lg:flex">
            {MENUS.map((m) => {
              const on = open === m.id;
              const here = m.id === "products" && pathname.startsWith("/products");
              return (
                <button
                  key={m.id}
                  type="button"
                  data-nav-item
                  data-nav-trigger={m.id}
                  aria-expanded={on}
                  aria-controls={`nav-${m.id}`}
                  className={`${NAV_ITEM} ${on || here ? "text-fg" : "text-muted hover:text-fg"}`}
                  {...onTrigger(m.id)}
                >
                  {m.label}
                  <ChevronDown className={`size-3.5 text-subtle transition-transform duration-200 motion-reduce:transition-none ${on ? "rotate-180" : ""}`} strokeWidth={1.5} />
                </button>
              );
            })}
            {PINNED.map((p) => (
              <Link
                key={p.slug}
                href={productHref(p.slug)}
                data-nav-item
                onPointerEnter={leave}
                className={`${NAV_ITEM} ${pathname === productHref(p.slug) ? "text-fg" : "text-muted hover:text-fg"}`}
              >
                {p.name}
              </Link>
            ))}
          </nav>

          <div className="ml-auto flex items-center gap-1.5" onPointerEnter={leave}>
            <Link href="/signin" data-nav-item className={`${NAV_ITEM} hidden text-muted hover:text-fg lg:flex`}>
              Sign in
            </Link>
            <Link
              href="/dashboard"
              data-nav-item
              className={`flex h-9 items-center rounded-lg bg-fg px-3.5 text-[14px] font-medium text-bg transition duration-150 hover:opacity-85 active:scale-[0.98] ${FOCUS} ring-offset-2 ring-offset-bg`}
            >
              Start rolling
            </Link>
            <button
              type="button"
              data-nav-toggle
              aria-expanded={sheet}
              aria-controls="nav-sheet"
              aria-label={sheet ? "Close the menu" : "Open the menu"}
              onClick={() => setSheet((v) => !v)}
              className={`-mr-2 grid size-10 place-items-center rounded-md text-fg lg:hidden ${FOCUS}`}
            >
              <span aria-hidden className="relative block h-3 w-[18px]">
                <span className={`absolute inset-x-0 top-0 h-px bg-current transition-transform duration-200 motion-reduce:transition-none ${sheet ? "translate-y-[5.5px] rotate-45" : ""}`} />
                <span className={`absolute inset-x-0 bottom-0 h-px bg-current transition-transform duration-200 motion-reduce:transition-none ${sheet ? "-translate-y-[5.5px] -rotate-45" : ""}`} />
              </span>
            </button>
          </div>
        </div>

        {/* The band the menus open in: one surface with the bar, as tall as the menu that's open. */}
        <div
          className={`absolute inset-x-0 top-full hidden overflow-hidden border-b bg-bg transition-[height,border-color] duration-[220ms] ease-out-soft motion-reduce:transition-none lg:block ${
            open ? "border-line" : "border-transparent"
          }`}
          style={{ height: open ? heights[open] : 0 }}
        >
          {MENUS.map((m) => (
            <div
              key={m.id}
              id={`nav-${m.id}`}
              ref={(el) => {
                panels.current[m.id] = el;
              }}
              onKeyDown={onPanelKey(m.id)}
              onClick={(e) => (e.target as HTMLElement).closest("a") && close()}
              className={`absolute inset-x-0 top-0 transition-[opacity,transform,visibility] duration-200 ease-out-soft motion-reduce:transform-none motion-reduce:transition-none ${
                open === m.id ? "visible translate-y-0 opacity-100" : "invisible -translate-y-1.5 opacity-0"
              }`}
            >
              {m.id === "products" ? <ProductsPanel live={open === "products"} /> : <DevelopersPanel />}
            </div>
          ))}
        </div>
      </header>

      {/* The page steps back while a menu is open; a click on it closes the menu. */}
      <div
        aria-hidden
        onClick={close}
        className={`fixed inset-0 z-40 hidden bg-bg/60 transition-opacity duration-200 motion-reduce:transition-none lg:block ${open ? "opacity-100" : "pointer-events-none opacity-0"}`}
      />
      <MobileSheet open={sheet} id="nav-sheet" onNavigate={() => setSheet(false)} />
    </>
  );
}

function FooterColumn({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div>
      <p className={EYEBROW}>{title}</p>
      <ul className="mt-4 space-y-2.5 text-[13.5px]">{children}</ul>
    </div>
  );
}

const FOOT_LINK = `rounded-sm text-muted transition-colors duration-150 hover:text-fg ${FOCUS}`;

/** What's live: the commit the API's node runs and when that node started (`GET /v1/info`). */
function Deployed() {
  const [info, setInfo] = useState<{ commit: string; since?: string } | null>(null);
  useEffect(() => {
    let live = true;
    api
      .info()
      .then((i) => live && i.commit && setInfo({ commit: i.commit, since: i.started_at }))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  if (!info) return null;
  const since = info.since ? new Date(info.since) : null;
  return (
    <span className="font-mono text-[11px] tracking-wide">
      live{" "}
      <a href={`https://github.com/roll-sh/roll/commit/${info.commit}`} target="_blank" rel="noreferrer" className={FOOT_LINK}>
        {info.commit.slice(0, 7)}
      </a>
      {since && !Number.isNaN(since.getTime()) && <> · since {since.toISOString().slice(0, 16).replace("T", " ")} UTC</>}
    </span>
  );
}

/** The marketing pages' footer: Horizon and the install line, every product by family, and the rest of the site. */
export function MarketingFooter() {
  const { state } = useMarkState();
  return (
    <footer className="border-t border-line">
      <div className={`${WRAP} grid grid-cols-[minmax(0,1fr)] gap-x-8 gap-y-14 py-16 sm:py-20 xl:grid-cols-[minmax(0,26rem)_minmax(0,1fr)] xl:gap-x-20`}>
        <div>
          <Link href="/" aria-label="roll home" className={`-ml-1 inline-block rounded-md p-1 ${FOCUS}`}>
            <RollLogo state={state} size={20} />
          </Link>
          <p className="mt-4 max-w-sm text-[13.5px] leading-relaxed text-muted text-pretty">
            Serverless infrastructure for everything. One daemon, every primitive, nothing running when nothing's asked.
          </p>
          <InstallCommand className="mt-6 max-w-[26rem]" />
        </div>
        <div className="grid grid-cols-2 gap-x-6 gap-y-12 sm:grid-cols-3 lg:grid-cols-6">
          {GROUPS.map((g) => (
            <FooterColumn key={g.id} title={g.name}>
              {PRODUCTS.filter((p) => p.group === g.id).map((p) => (
                <li key={p.slug}>
                  <Link href={productHref(p.slug)} className={FOOT_LINK}>
                    {p.name}
                  </Link>
                </li>
              ))}
            </FooterColumn>
          ))}
          {FOOTER.map((col) => (
            <FooterColumn key={col.name} title={col.name}>
              {col.links.map((l) => (
                <li key={l.title}>
                  {l.external ? (
                    <a href={l.href} target="_blank" rel="noreferrer" className={`inline-flex items-center gap-1 ${FOOT_LINK}`}>
                      {l.title}
                      <ArrowUpRight className="size-3 text-subtle" strokeWidth={1.5} />
                    </a>
                  ) : (
                    <Link href={l.href} className={FOOT_LINK}>
                      {l.title}
                    </Link>
                  )}
                </li>
              ))}
            </FooterColumn>
          ))}
        </div>
      </div>
      <div className="border-t border-line">
        <div className={`${WRAP} flex flex-wrap items-center justify-between gap-x-6 gap-y-2 py-6 text-[12.5px] text-subtle`}>
          <span>© {new Date().getFullYear()} roll</span>
          <Deployed />
          <span className="font-mono text-[11px] tracking-wide">keep the code rolling</span>
        </div>
      </div>
    </footer>
  );
}
