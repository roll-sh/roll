import { createCsrfMiddleware, createMiddleware, createStart } from "@tanstack/react-start";

/** `/dashboard/` is `/dashboard`: a permanent redirect without the trailing slash, query kept. */
const trailingSlash = createMiddleware().server(({ request, next }) => {
  const url = new URL(request.url);
  if (url.pathname.length > 1 && url.pathname.endsWith("/")) {
    return new Response(null, { status: 308, headers: { location: `${url.pathname.replace(/\/+$/, "") || "/"}${url.search}` } });
  }
  return next();
});

export const startInstance = createStart(() => ({
  // Start installs CSRF protection for server functions only when there's no start.ts; keep it.
  requestMiddleware: [createCsrfMiddleware({ filter: (ctx) => ctx.handlerType === "serverFn" }), trailingSlash],
}));
