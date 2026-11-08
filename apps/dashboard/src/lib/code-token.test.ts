import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/** A window with localStorage and events, enough for code-token. */
function fakeWindow() {
  const store = new Map<string, string>();
  const target = new EventTarget();
  return Object.assign(target, {
    localStorage: {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
      removeItem: (k: string) => void store.delete(k),
    },
  });
}

beforeEach(() => vi.stubGlobal("window", fakeWindow()));
afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
});

describe("code token", () => {
  it("stores, trims and clears the token, telling subscribers", async () => {
    const t = await import("./code-token");
    const changed = vi.fn();
    const off = t.subscribeToken(changed);
    expect(t.getToken()).toBeUndefined();
    t.setToken("  rlc_abc ");
    expect(t.getToken()).toBe("rlc_abc");
    t.setToken("");
    expect(t.getToken()).toBeUndefined();
    t.setToken("rlc_x");
    t.setToken(null);
    expect(t.getToken()).toBeUndefined();
    expect(changed).toHaveBeenCalledTimes(4);
    off();
    t.setToken("rlc_y");
    expect(changed).toHaveBeenCalledTimes(4);
  });

  it("follows other tabs through storage events for its key only", async () => {
    const t = await import("./code-token");
    const changed = vi.fn();
    t.subscribeToken(changed);
    const storage = (key: string | null) => Object.assign(new Event("storage"), { key });
    window.dispatchEvent(storage("roll.code.token"));
    window.dispatchEvent(storage("something.else"));
    window.dispatchEvent(storage(null));
    expect(changed).toHaveBeenCalledTimes(2);
  });

  it("sends the token to the products' APIs only while it's the operator's", async () => {
    const t = await import("./code-token");
    t.setToken("rlc_owner");
    expect(t.operatorHeaders()).toEqual({});
    t.setToken("operator-token", true);
    expect(t.operatorHeaders()).toEqual({ authorization: "Bearer operator-token" });
    t.markOperator(false);
    expect(t.operatorHeaders()).toEqual({});
    t.setToken("operator-token", true);
    t.setToken(null);
    expect(t.operatorHeaders()).toEqual({});
  });
});
