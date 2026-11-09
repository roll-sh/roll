import { availableParallelism } from "node:os";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

/**
 * `pnpm --filter dashboard test`: the pure logic in node (`*.test.ts`) and React components in
 * jsdom with Testing Library (`*.test.tsx`).
 *
 * vmThreads gives every file a fresh context (no state leaks between files) without setting up a
 * worker and jsdom per file: 18 s instead of 39 s for the suite on 4 cores. More than 12 workers
 * only contend on a big machine. Component tests may take 15 s before they count as hung: on a
 * shared, loaded machine a whole-surface render can pass 5 s.
 */
export default defineConfig({
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  test: {
    pool: "vmThreads",
    maxWorkers: Math.min(12, availableParallelism()),
    projects: [
      { extends: true, test: { name: "node", environment: "node", include: ["src/**/*.test.ts"] } },
      {
        extends: true,
        test: { name: "dom", environment: "jsdom", include: ["src/**/*.test.tsx"], setupFiles: ["./src/test/setup-dom.ts"], testTimeout: 15_000 },
      },
    ],
  },
});
