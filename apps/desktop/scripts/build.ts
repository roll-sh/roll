// Bundles the main process and the preload into dist/ (CommonJS, as Electron loads them).
// Run: pnpm --filter @roll/desktop build
import { build, type BuildOptions } from "esbuild";

const root = new URL("..", import.meta.url).pathname;
const common: BuildOptions = {
  absWorkingDir: root,
  bundle: true,
  platform: "node",
  format: "cjs",
  target: "node22",
  sourcemap: "linked",
  logLevel: "info",
  external: ["electron"],
};

await Promise.all([
  // electron-updater loads lazily and only when auto-update is on; keep it a real dependency.
  build({ ...common, entryPoints: ["src/main/main.ts"], outfile: "dist/main.cjs", external: ["electron", "electron-updater"] }),
  // Sandboxed preloads may only require "electron" (and a few builtins): bundle everything else.
  build({ ...common, entryPoints: ["src/preload/preload.ts"], outfile: "dist/preload.cjs" }),
]);
