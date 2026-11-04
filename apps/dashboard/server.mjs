// `pnpm --filter dashboard start`: the production dashboard on Node. Serves the built client
// (`dist/client`, hashed assets cached for good) and hands every other request to TanStack Start's
// fetch handler (`dist/server/server.js`).
//
//   node server.mjs [-H <host>] [-p <port>]     (or HOST / PORT; default all interfaces, 3000)
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

serve({
  hostname: values.hostname ?? process.env.HOST,
  port: Number(values.port ?? process.env.PORT ?? 3000),
  middleware: [(request, next) => (new URL(request.url).pathname.startsWith("/assets/") ? assets : files)(request, next)],
  fetch: (request) => app.fetch(request),
});
