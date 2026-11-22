import { describe, expect, it } from "vitest";
import { bulkPages, pageLimit, presentSamples, SAMPLES, sample } from "./search-samples";

const NOW = Date.UTC(2026, 10, 21, 12);

describe("sample data", () => {
  it("generates the same valid _bulk pages every load, each under the body limit", () => {
    expect(pageLimit(1)).toBe(512 * 1024);
    expect(pageLimit(100)).toBe(1024 * 1024);
    const limit = 64 * 1024;
    for (const set of SAMPLES) {
      const pages = bulkPages(set, NOW, limit);
      expect(bulkPages(set, NOW, limit)).toEqual(pages);
      expect(pages.length).toBeGreaterThan(1);
      const ids: string[] = [];
      for (const p of pages) {
        expect(new TextEncoder().encode(p.body).length).toBeLessThanOrEqual(limit);
        const lines = p.body.trimEnd().split("\n").map((l) => JSON.parse(l));
        expect(lines.length).toBe(p.docs * 2);
        for (let i = 0; i < lines.length; i += 2) {
          const [op, meta] = Object.entries(lines[i])[0] as [string, { _index: string; _id: string }];
          expect(op).toBe(set.kind === "data stream" ? "create" : "index");
          expect(meta._index).toBe(set.id);
          ids.push(meta._id);
        }
      }
      expect(ids.length).toBe(set.docs);
      expect(new Set(ids).size).toBe(set.docs);
    }
    expect(sample("sample-products").documents(NOW)[0].id).toBe("p0001");
    expect(sample("sample-products").documents(NOW)[0].source).toEqual(sample("sample-products").documents(NOW + 5000)[0].source);
  });

  it("spreads log lines over the 24 hours before the load, with errors in the last hour", () => {
    const docs = sample("sample-logs").documents(NOW);
    const times = docs.map((d) => Date.parse(d.source["@timestamp"] as string));
    expect(Math.min(...times)).toBeGreaterThanOrEqual(NOW - 86_400_000);
    expect(Math.max(...times)).toBeLessThan(NOW);
    const recentErrors = docs.filter((d, i) => times[i] >= NOW - 3_600_000 && (d.source.http as { response: { status_code: number } }).response.status_code >= 500);
    expect(recentErrors.length).toBeGreaterThan(0);
  });

  it("finds loaded datasets in an index listing", () => {
    expect([...presentSamples(["sample-products", ".ds-sample-logs-2026.11.21-000001", "other"])].sort()).toEqual(["sample-logs", "sample-products"]);
  });
});
