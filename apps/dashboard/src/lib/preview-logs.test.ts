import { describe, expect, it } from "vitest";
import { logFilter, logLevel } from "./preview-logs";

describe("a preview's log lines", () => {
  it("say their level as roll apps reads it, and prose that mentions an error says none", () => {
    expect(logLevel('{"level":"error","msg":"card declined"}')).toBe("error");
    expect(logLevel('{"level":50,"msg":"pino"}')).toBe("error");
    expect(logLevel('{"log":{"level":"warning"}}')).toBe("warn");
    expect(logLevel('ts=1 level=warn msg="slow query"')).toBe("warn");
    expect(logLevel("2026-11-19T01:00:00Z ERROR payment failed")).toBe("error");
    expect(logLevel("[error] upstream timed out")).toBe("error");
    expect(logLevel("PANIC: out of memory")).toBe("fatal");
    expect(logLevel("listening on :3000, no error so far")).toBeNull();
    expect(logLevel("Error: boom")).toBeNull();
    expect(logLevel("{not json")).toBeNull();
  });

  it("are filtered by text case-blind or by /regex/, and a broken regex is said", () => {
    expect(["GET /checkout 500", "GET / 200"].filter(logFilter("checkout")!)).toEqual(["GET /checkout 500"]);
    expect(["GET /checkout 500", "GET / 200"].filter(logFilter("/ 5\\d\\d$/")!)).toEqual(["GET /checkout 500"]);
    expect(["Card DECLINED"].filter(logFilter("/declined/i")!)).toEqual(["Card DECLINED"]);
    expect(logFilter("/(/")).toBeNull();
    expect(["a", "b"].filter(logFilter("  ")!)).toEqual(["a", "b"]);
  });
});
