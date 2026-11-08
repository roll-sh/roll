import { defineConfig } from "vitest/config";

/** `pnpm --filter @roll/desktop test`: the main process's logic in node, no Electron. */
export default defineConfig({
  test: { environment: "node", include: ["src/**/*.test.ts"], testTimeout: 20_000 },
});
