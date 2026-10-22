import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

/** Unit tests for the dashboard's pure logic (node, no browser): `pnpm --filter dashboard test`. */
export default defineConfig({
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  test: { environment: "node", include: ["src/**/*.test.ts"] },
});
