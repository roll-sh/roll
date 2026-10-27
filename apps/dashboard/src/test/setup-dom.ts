/**
 * jsdom setup for component tests (`*.test.tsx`): Testing Library matchers and cleanup, the browser
 * APIs jsdom lacks, and Next's link and navigation as plain stand-ins.
 */
import "@testing-library/jest-dom/vitest";
import { cleanup, configure } from "@testing-library/react";
import { createElement, type AnchorHTMLAttributes, type ReactNode } from "react";
import { afterEach, vi } from "vitest";

// Pages load through several awaited fetches; give findBy/waitFor room on a busy machine.
configure({ asyncUtilTimeout: 5000 });

afterEach(() => {
  cleanup();
  localStorage.clear();
});

// `next/link` as a plain anchor; the tests read `href`.
vi.mock("next/link", () => ({
  default: ({ href, children, prefetch: _prefetch, ...rest }: AnchorHTMLAttributes<HTMLAnchorElement> & { href: string; prefetch?: boolean; children?: ReactNode }) =>
    createElement("a", { href, ...rest }, children),
}));

vi.mock("next/navigation", () => ({
  usePathname: () => "/dashboard",
  useRouter: () => ({ push: vi.fn(), replace: vi.fn(), refresh: vi.fn(), back: vi.fn(), prefetch: vi.fn() }),
  useSearchParams: () => new URLSearchParams(),
  useParams: () => ({}),
  notFound: () => {
    throw new Error("notFound");
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
