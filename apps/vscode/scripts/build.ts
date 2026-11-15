// Bundles the extension into dist/extension.cjs (CommonJS, as VS Code loads extensions).
// Run: pnpm --filter roll-secrets build
import { build } from "esbuild";

await build({
  absWorkingDir: new URL("..", import.meta.url).pathname,
  entryPoints: ["src/extension.ts"],
  outfile: "dist/extension.cjs",
  bundle: true,
  platform: "node",
  format: "cjs",
  target: "node20",
  sourcemap: "linked",
  logLevel: "info",
  external: ["vscode"],
});
