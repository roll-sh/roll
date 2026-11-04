import { createRouter } from "@tanstack/react-router";
import { NotFound } from "./components/not-found";
import { parseSearch, stringifySearch } from "./lib/search";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({
    routeTree,
    parseSearch,
    stringifySearch,
    defaultNotFoundComponent: NotFound,
    defaultPreload: "intent",
    scrollRestoration: true,
  });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof getRouter>;
  }
}
