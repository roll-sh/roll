import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import type { SearchCluster } from "@/lib/search-api";
import { DiscoverTab } from "./discover";

const esJson = vi.hoisted(() => vi.fn());
const esRaw = vi.hoisted(() => vi.fn());
vi.mock("@/lib/search-api", async (orig) => {
  const real = await orig<typeof import("@/lib/search-api")>();
  return { ...real, esJson, esRaw };
});

const cluster = { id: "c1", name: "logs", flavor: "elasticsearch" } as unknown as SearchCluster;

afterEach(() => {
  window.location.hash = "";
  vi.clearAllMocks();
});

it("shows a view's fields, its documents over time and its rows, and a field's top values", async () => {
  const user = userEvent.setup();
  const hits = [
    { _index: "logs-web", _id: "1", _source: { "@timestamp": "2026-11-26T11:59:00Z", host: "web-1", message: "GET /cart 500" } },
    { _index: "logs-web", _id: "2", _source: { "@timestamp": "2026-11-26T11:58:00Z", host: "web-2", message: "GET / 200" } },
  ];
  esJson.mockImplementation(async (_id: string, method: string, path: string) => {
    if (path.startsWith("/_resolve/index/")) return { indices: [{ name: "logs-web" }], aliases: [], data_streams: [] };
    if (path.startsWith("/logs-web/_field_caps")) {
      return { fields: { "@timestamp": { date: { type: "date", aggregatable: true } }, host: { keyword: { type: "keyword", aggregatable: true } }, message: { text: { type: "text", aggregatable: false } } } };
    }
    if (method === "POST" && path === "/logs-web/_search") return { aggregations: { sample: { doc_count: 2, top: { buckets: [{ key: "web-1", doc_count: 1 }, { key: "web-2", doc_count: 1 }] } } } };
    throw new Error(`unexpected ${method} ${path}`);
  });
  esRaw.mockImplementation(async () => {
    const value = { took: 3, hits: { total: { value: 2, relation: "eq" }, hits }, aggregations: { over_time: { buckets: [{ key: Date.UTC(2026, 10, 26, 11, 58), doc_count: 1 }, { key: Date.UTC(2026, 10, 26, 11, 59), doc_count: 1 }] } } };
    return { value, text: JSON.stringify(value) };
  });
  window.location.hash = "#discover?view=logs-web&col=host";
  render(<DiscoverTab cluster={cluster} onOpenConsole={vi.fn()} />);

  const fields = await screen.findByRole("complementary", { name: "Fields" });
  expect(await within(fields).findByText("message")).toBeInTheDocument();
  expect(await screen.findByText("web-2")).toBeInTheDocument();
  expect(screen.getByRole("list", { name: "Documents over time" }).children).toHaveLength(2);
  // The search is the view's: KQL and the range, by the field picked as its time field.
  const [, , path, body] = esRaw.mock.calls.at(-1)!;
  expect(path).toBe("/logs-web/_search");
  expect(body.query.bool.filter[1]).toEqual({ range: { "@timestamp": { gte: "now-24h" } } });
  expect(window.location.hash).toContain("tf=%40timestamp");

  await user.click(within(fields).getByRole("button", { name: "host (keyword)" }));
  await user.click(await within(fields).findByRole("button", { name: "Filter out host: web-2" }));
  expect(screen.getByRole("textbox", { name: "Query" })).toHaveValue('not host: "web-2"');
  expect(window.location.hash).toContain("q=not+host");
});
