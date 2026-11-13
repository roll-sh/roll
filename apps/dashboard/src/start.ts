import { createCsrfMiddleware, createMiddleware, createStart } from "@tanstack/react-start";

/** `/dashboard/` is `/dashboard`: a permanent redirect without the trailing slash, query kept. */
const trailingSlash = createMiddleware().server(({ request, next }) => {
  const url = new URL(request.url);
  if (url.pathname.length > 1 && url.pathname.endsWith("/")) {
    return new Response(null, { status: 308, headers: { location: `${url.pathname.replace(/\/+$/, "") || "/"}${url.search}` } });
  }
  return next();
});

/** The OAuth consent screen is never framed: another site can't overlay it and steal a click. */
const noFraming = createMiddleware().server(async ({ request, next }) => {
  const result = await next();
  if (new URL(request.url).pathname.startsWith("/oauth/")) {
    try {
      result.response.headers.set("x-frame-options", "DENY");
      result.response.headers.set("content-security-policy", "frame-ancestors 'none'");
    } catch {
      // An immutable response (a redirect made elsewhere): nothing of ours to frame.
    }
  }
  return result;
});

export const startInstance = createStart(() => ({
  // Start installs CSRF protection for server functions only when there's no start.ts; keep it.
  requestMiddleware: [createCsrfMiddleware({ filter: (ctx) => ctx.handlerType === "serverFn" }), trailingSlash, noFraming],
}));
