// Run the Worker locally in workerd (Miniflare) for roll's CDN tests:
//
//   PORT=8787 ROLL_ORIGIN=http://127.0.0.1:59000 ROLL_CDN_ORIGIN_SECRET=… node test/serve.mjs
//
// Every `ROLL_*` environment variable becomes a binding. The Worker is bundled in memory from
// src/ (nothing is written to the repository, so it runs from a read-only mount), the cache is
// in memory, and `ready <url>` is printed once it listens. Stops on SIGTERM or SIGINT.
import { build } from "esbuild";
import { Miniflare } from "miniflare";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const out = await build({
  entryPoints: [`${root}src/index.ts`],
  bundle: true,
  format: "esm",
  platform: "neutral",
  target: "es2022",
  write: false,
  logLevel: "warning",
});

const bindings = Object.fromEntries(Object.entries(process.env).filter(([k]) => k.startsWith("ROLL_")));
const mf = new Miniflare({
  modules: true,
  script: out.outputFiles[0].text,
  compatibilityDate: "2026-07-01",
  host: "127.0.0.1",
  port: Number(process.env.PORT || 8787),
  bindings,
  cache: true,
});
const url = await mf.ready;
console.log(`ready ${url.href.replace(/\/$/, "")}`);
console.error(`cdn worker: started in ${process.uptime().toFixed(1)}s`);

let stopping = false;
const stop = async () => {
  if (stopping) return;
  stopping = true;
  await mf.dispose().catch(() => {});
  process.exit(0);
};
process.on("SIGTERM", stop);
process.on("SIGINT", stop);
