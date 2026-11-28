import { useRouterState } from "@tanstack/react-router";
import { useEffect, useState, type CSSProperties, type ReactNode } from "react";
import { useMarkBusy } from "./mark-state";

/**
 * The dashboard's one loading system (docs/dashboard-polish.md, "Loading"):
 *
 * - A navigation keeps the page on screen; after 150 ms the header's bottom edge carries a calm
 *   accent sweep and every Horizon mark thinks ({@link NavProgress}, {@link RouteBusy}).
 * - A page's data: {@link Loaded} holds the place with a skeleton shaped like what's coming, faded
 *   in only after 150 ms (a fast load shows nothing), then the content arrives with `.arrive`
 *   (200 ms fade and 4 px rise). Content that was already there when the page opened (a cached
 *   store) just shows.
 * - Skeletons are built from {@link Skeleton} blocks in the page's own layout, or the shared shapes
 *   below for the shapes most pages share (a list panel, a grid of cards, a page heading).
 *
 * Reduced motion: skeletons don't breathe, nothing fades or sweeps; the mark shows its static
 * frame for the state.
 */

/** One placeholder block. Size it with classes (`h-3 w-24`) to the thing it stands for. */
export function Skeleton({ className = "", style }: { className?: string; style?: CSSProperties }) {
  return <span aria-hidden className={`skeleton block rounded-[5px] ${className}`} style={style} />;
}

/**
 * `children` once `ready`; until then `skeleton` in its place, faded in only if the wait passes
 * 150 ms, with the marks thinking. The wrapper is a plain `div` (`className` lays it out), so the
 * skeleton and the content take the same box.
 */
export function Loaded({ ready, skeleton, children, className = "" }: { ready: boolean; skeleton: ReactNode; children: ReactNode; className?: string }) {
  // Content there from the first render (a cached store) shows as is; content that was waited for arrives.
  const [waited] = useState(!ready);
  useMarkBusy(!ready);
  if (!ready)
    return (
      <div aria-busy="true" className={`skeleton-in ${className}`}>
        <span className="sr-only" role="status">
          Loading
        </span>
        {skeleton}
      </div>
    );
  return <div className={`${waited ? "arrive" : ""} ${className}`}>{children}</div>;
}

/** Widths that vary row to row like real names do, the same on every render (no layout jitter). */
const WIDTHS = [46, 32, 58, 38, 52, 28, 44, 36];

/** A list panel's rows (Home's "Not in a project", a product's databases): a tile, a name and a
 *  line under it, something on the right. `tile={false}` for rows without one. */
export function SkeletonList({ rows = 4, tile = true, className = "" }: { rows?: number; tile?: boolean; className?: string }) {
  return (
    <div className={`divide-y divide-line rounded-xl border border-line-2 bg-sheet inset-shadow-highlight ${className}`}>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="flex h-[46px] items-center gap-2.5 px-3">
          {tile && <Skeleton className="size-6 shrink-0 rounded-md" />}
          <span className="flex min-w-0 flex-1 flex-col gap-1.5">
            <Skeleton className="h-2.5" style={{ width: `${WIDTHS[i % WIDTHS.length]}%` }} />
            <Skeleton className="h-2 opacity-70" style={{ width: `${WIDTHS[(i + 3) % WIDTHS.length] / 2}%` }} />
          </span>
          <Skeleton className="h-2 w-12" />
        </div>
      ))}
    </div>
  );
}

/** Rows of text inside a panel that's already drawn (a tab's table, a section's list). */
export function SkeletonLines({ rows = 3 }: { rows?: number }) {
  return (
    <div className="flex flex-col gap-3 py-1">
      {Array.from({ length: rows }, (_, i) => (
        <span key={i} className="flex items-center gap-3">
          <Skeleton className="h-2.5" style={{ width: `${WIDTHS[i % WIDTHS.length]}%` }} />
          <Skeleton className="ml-auto h-2 w-14 opacity-70" />
        </span>
      ))}
    </div>
  );
}

/** A grid of cards (Home's projects): an icon, a name, a line, a footer. */
export function SkeletonCards({ count = 3, className = "" }: { count?: number; className?: string }) {
  return (
    <div className={`grid gap-2.5 sm:grid-cols-2 lg:grid-cols-3 ${className}`}>
      {Array.from({ length: count }, (_, i) => (
        <div key={i} className="rounded-xl border border-line-2 bg-sheet p-3 inset-shadow-highlight">
          <div className="flex items-center gap-2.5">
            <Skeleton className="size-8 shrink-0 rounded-md" />
            <span className="flex flex-1 flex-col gap-1.5">
              <Skeleton className="h-2.5" style={{ width: `${WIDTHS[i % WIDTHS.length]}%` }} />
              <Skeleton className="h-2 w-1/4 opacity-70" />
            </span>
          </div>
          <div className="mt-3 flex h-[19px] gap-1">
            <Skeleton className="h-full w-12" />
            <Skeleton className="h-full w-10" />
          </div>
          <div className="mt-3 flex h-[25px] items-end justify-between border-t border-line pb-1">
            <Skeleton className="h-2 w-14" />
            <Skeleton className="h-2 w-10" />
          </div>
        </div>
      ))}
    </div>
  );
}

/** A page's title and the line under it, at the size pages use (20 px heading, 12.5 px text). */
export function SkeletonHeading({ className = "" }: { className?: string }) {
  return (
    <div className={`flex flex-col gap-2 py-1 ${className}`}>
      <Skeleton className="h-4 w-40" />
      <Skeleton className="h-2.5 w-72 max-w-full opacity-70" />
    </div>
  );
}

/** A project's icon and name in the header while the projects load. */
export function SkeletonCrumb() {
  return (
    <span aria-hidden className="skeleton-in flex items-center gap-2 py-0.5 pl-0.5">
      <Skeleton className="size-5 rounded-md" />
      <Skeleton className="h-2.5 w-24" />
    </span>
  );
}

/** Pending for longer than 150 ms: what the navigation indicators show. */
function useSlowNavigation() {
  const pending = useRouterState({ select: (s) => s.status === "pending" });
  const [slow, setSlow] = useState(false);
  useEffect(() => {
    if (!pending) {
      setSlow(false);
      return;
    }
    const t = setTimeout(() => setSlow(true), 150);
    return () => clearTimeout(t);
  }, [pending]);
  return slow;
}

/** The header's bottom edge while a navigation is slow: an accent sweep along the real border. */
export function NavProgress() {
  const slow = useSlowNavigation();
  return (
    <span aria-hidden data-shown={slow || undefined} className="nav-progress pointer-events-none absolute inset-x-0 -bottom-px h-px overflow-hidden">
      <i />
    </span>
  );
}

/** Mounted once at the root: the marks think while a navigation is pending. */
export function RouteBusy() {
  const pending = useRouterState({ select: (s) => s.status === "pending" });
  useMarkBusy(pending, 150);
  return null;
}
