import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

/**
 * `pnpm --filter dashboard test`: the pure logic in node (`*.test.ts`) and React components in
 * jsdom with Testing Library (`*.test.tsx`).
 */
export default defineConfig({
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  test: {
    projects: [
      { extends: true, test: { name: "node", environment: "node", include: ["src/**/*.test.ts"] } },
      {
        extends: true,
        test: { name: "dom", environment: "jsdom", include: ["src/**/*.test.tsx"], setupFiles: ["./src/test/setup-dom.ts"] },
      },
    ],
  },
});
