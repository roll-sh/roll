/**
 * A stand-in for `codeApi` in component tests. Every method is a `vi.fn` that rejects until a test
 * gives it an answer, so an unexpected call shows up as an error rather than a hang. Use with:
 *
 *   vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig()), ...(await import("@/test/api")).apiModule }));
 */
import { vi, type Mock } from "vitest";
import type { CodeApi } from "@/lib/code-api";

type Fns = { [K in keyof CodeApi]: CodeApi[K] extends (...args: infer A) => infer R ? Mock<(...args: A) => R> : CodeApi[K] };

const fns = new Map<string, Mock>();
const fixed: Partial<Record<keyof CodeApi, unknown>> = {
  mcpUrl: () => "http://roll.test/v1/code/mcp",
  eventsUrl: (repo: string) => `http://roll.test/v1/code/repos/${repo}/events`,
  authHeaders: () => ({}),
};

export const api = new Proxy({} as Fns, {
  get(_, key) {
    if (typeof key !== "string") return undefined;
    if (key in fixed) return fixed[key as keyof CodeApi];
    let fn = fns.get(key);
    if (!fn) {
      fn = vi.fn(async () => {
        throw new Error(`codeApi.${key} isn't mocked in this test`);
      });
      fns.set(key, fn);
    }
    return fn;
  },
});

/** Forget every answer and call (call from `beforeEach`). */
export function resetApi() {
  fns.clear();
}

/** What `vi.mock("@/lib/code-api")` spreads over the real module: the fake client, no event stream. */
export const apiModule = { codeApi: api, subscribe: () => () => {} };
