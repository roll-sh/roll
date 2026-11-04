// `pnpm --filter dashboard start`: the production dashboard on Node. Serves the built client
// (`dist/client`, hashed assets cached for good) and hands every other request to TanStack Start's
// fetch handler (`dist/server/server.js`).
//
//   node server.mjs [-H <host>] [-p <port>]     (or HOST / PORT; default all interfaces, 3000)
import { extname } from "node:path";
import { parseArgs } from "node:util";
import { serve } from "srvx";
import { staticMiddleware } from "srvx/static";
import app from "./dist/server/server.js";

const { values } = parseArgs({
  options: { hostname: { type: "string", short: "H" }, port: { type: "string", short: "p" } },
});

const dir = new URL("./dist/client", import.meta.url).pathname;
const assets = staticMiddleware({ dir, maxAge: 31_536_000, immutable: true });
const files = staticMiddleware({ dir });
/** Types of files in public/ that srvx's table doesn't know (`curl roll.sh/install.sh | sh`). */
const TYPES = { ".sh": "application/x-sh" };

serve({
  hostname: values.hostname ?? process.env.HOST,
  port: Number(values.port ?? process.env.PORT ?? 3000),
  middleware: [
    async (request, next) => {
      const { pathname } = new URL(request.url);
      const res = await (pathname.startsWith("/assets/") ? assets : files)(request, next);
      const type = TYPES[extname(pathname)];
      if (!type || res.headers.get("content-type") !== "application/octet-stream") return res;
      const headers = new Headers(res.headers);
      headers.set("content-type", type);
      return new Response(res.body, { status: res.status, statusText: res.statusText, headers });
    },
  ],
  fetch: (request) => app.fetch(request),
});
