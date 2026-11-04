/**
 * jsdom setup for component tests (`*.test.tsx`): Testing Library matchers and cleanup, the browser
 * APIs jsdom lacks, and the router's link and navigation hooks as plain stand-ins, so components
 * render without a router around them.
 */
import "@testing-library/jest-dom/vitest";
import * as webStreams from "node:stream/web";
import { cleanup, configure } from "@testing-library/react";
import { createElement, type AnchorHTMLAttributes, type ReactNode } from "react";
import { afterEach, vi } from "vitest";
import { stringifySearch } from "@/lib/search";

// vmThreads' jsdom context lacks some of Node's web-stream globals; TanStack Start's server-function
// client builds a `ByteLengthQueuingStrategy` as it loads (the route tree imports it).
for (const [name, value] of Object.entries(webStreams)) (globalThis as Record<string, unknown>)[name] ??= value;

// Pages load through several awaited fetches; give findBy/waitFor room on a busy machine.
configure({ asyncUtilTimeout: 5000 });

afterEach(() => {
  cleanup();
  localStorage.clear();
});

type LinkProps = AnchorHTMLAttributes<HTMLAnchorElement> & { to?: string; search?: Record<string, unknown>; children?: ReactNode } & Record<string, unknown>;

// The router's `Link` as a plain anchor (the tests read `href`); everything else is the real router.
vi.mock("@tanstack/react-router", async (original) => ({
  ...(await original<typeof import("@tanstack/react-router")>()),
  Link: ({ href, to, search, children, preload: _p, replace: _r, resetScroll: _s, activeProps: _a, ...rest }: LinkProps) =>
    createElement("a", { href: href ?? `${to && to !== "." ? to : ""}${search ? stringifySearch(search) || "?" : ""}`, ...rest }, children),
  useNavigate: () => vi.fn(),
  useRouter: () => ({ invalidate: vi.fn() }),
  useLocation: <T,>(opts?: { select?: (l: { pathname: string; search: object; hash: string }) => T }) => {
    const l = { pathname: "/dashboard", search: {}, hash: "" };
    return opts?.select ? opts.select(l) : l;
  },
}));

// The marks draw on canvas; jsdom has none (they skip drawing when there's no context).
HTMLCanvasElement.prototype.getContext = (() => null) as unknown as HTMLCanvasElement["getContext"];

if (!window.matchMedia) {
  window.matchMedia = (query: string) =>
    ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }) as MediaQueryList;
}

class NoopObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return [];
  }
}
globalThis.IntersectionObserver ??= NoopObserver as unknown as typeof IntersectionObserver;
globalThis.ResizeObserver ??= NoopObserver as unknown as typeof ResizeObserver;
Element.prototype.scrollIntoView ??= function () {};
