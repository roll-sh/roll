#!/usr/bin/env node
// `pnpm typecheck`: every workspace package's own `typecheck` script, all packages at once, with
// `tsc` resolving to TypeScript 7 (the native compiler, the root devDependency) instead of the
// package's TypeScript 5.9. The packages keep 5.9 because Next's build uses its compiler API; both
// report the same diagnostics on this repo (checked with injected errors when this was added).
// Each package's scripts stay the one place that says what it checks.
import { spawn } from "node:child_process";
import { readdirSync, readFileSync, existsSync } from "node:fs";
import { delimiter, join } from "node:path";

const root = new URL("..", import.meta.url).pathname;
const packages = ["apps", "packages"]
  .flatMap((dir) => readdirSync(join(root, dir)).map((name) => join(root, dir, name)))
  .filter((dir) => existsSync(join(dir, "package.json")))
  .map((dir) => ({ dir, script: JSON.parse(readFileSync(join(dir, "package.json"), "utf8")).scripts?.typecheck }))
  .filter((p) => p.script);

function run({ dir, script }) {
  return new Promise((resolve) => {
    const PATH = [join(root, "node_modules/.bin"), join(dir, "node_modules/.bin"), process.env.PATH].join(delimiter);
    const child = spawn("sh", ["-c", script], { cwd: dir, env: { ...process.env, PATH }, stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (out += d));
    child.on("close", (code) => resolve({ dir: dir.slice(root.length), code, out }));
  });
}

const results = await Promise.all(packages.map(run));
let failed = 0;
for (const r of results) {
  if (r.code === 0) continue;
  failed++;
  process.stdout.write(`\n${r.dir}: typecheck failed\n${r.out}`);
}
console.log(failed ? `\n${failed} of ${results.length} packages failed` : `typecheck: ${results.length} packages clean`);
process.exit(failed ? 1 : 0);
