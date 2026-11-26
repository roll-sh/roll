import { describe, expect, it } from "vitest";
import type { LogLine } from "@/lib/apps-api";
import { histogram } from "./logs";

const line = (t: number, over: Partial<LogLine> = {}): LogLine => ({ t, stream: "stdout", instance: null, deployment: null, text: "x", ...over }) as LogLine;

describe("histogram", () => {
  it("counts matches into bars over the window, errors apart", () => {
    const { bars } = histogram([line(0), line(10), line(55, { level: "error" }), line(99, { stream: "stderr" })], 0, 100, 4);
    expect(bars.map((b) => [b.lines, b.errors])).toEqual([[2, 0], [0, 0], [1, 1], [1, 1]]);
  });
});
