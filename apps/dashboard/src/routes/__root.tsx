import { createRootRoute, HeadContent, isNotFound, Outlet, Scripts } from "@tanstack/react-router";
import type { ReactNode } from "react";
import geistMono from "@fontsource-variable/geist-mono/files/geist-mono-latin-wght-normal.woff2?url";
import geistSans from "@fontsource-variable/geist/files/geist-latin-wght-normal.woff2?url";
import { MarkStateProvider } from "@/components/mark-state";
import { NotFound } from "@/components/not-found";
import { titled } from "@/lib/head";
import { anySearch } from "@/lib/search";
import css from "../globals.css?url";

export const Route = createRootRoute({
  validateSearch: anySearch,
  head: ({ matches }) => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      // A page's own title replaces this one; a 404 has none of its own.
      { title: matches.some((m) => isNotFound(m.error)) ? titled("Not found") : "roll" },
      { name: "description", content: "Serverless infrastructure for everything." },
    ],
    links: [
      { rel: "preload", href: geistSans, as: "font", type: "font/woff2", crossOrigin: "anonymous" },
      { rel: "preload", href: geistMono, as: "font", type: "font/woff2", crossOrigin: "anonymous" },
      { rel: "stylesheet", href: css },
      { rel: "icon", href: "/icon.svg", sizes: "any", type: "image/svg+xml" },
    ],
  }),
  shellComponent: Document,
  component: Outlet,
  notFoundComponent: NotFound,
});

function Document({ children }: { children: ReactNode }) {
  return (
    <html lang="en" className="h-full antialiased">
      <head>
        <HeadContent />
      </head>
      <body className="min-h-full flex flex-col font-sans">
        <MarkStateProvider>{children}</MarkStateProvider>
        <Scripts />
      </body>
    </html>
  );
}
