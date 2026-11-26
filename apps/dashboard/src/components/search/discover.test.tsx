import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import type { SearchCluster } from "@/lib/search-api";
import { DiscoverTab } from "./discover";

const esJson = vi.hoisted(() => vi.fn());
const esRaw = vi.hoisted(() => vi.fn());
const saved = vi.hoisted(() => ({ savedSearches: vi.fn(), saveSearch: vi.fn(), updateSavedSearch: vi.fn(), deleteSavedSearch: vi.fn() }));
vi.mock("@/lib/search-api", async (orig) => {
  const real = await orig<typeof import("@/lib/search-api")>();
  return { ...real, esJson, esRaw, searchApi: { ...real.searchApi, ...saved } };
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

/** A cluster with one index, `logs-web`, its fields, and `esRaw` answering `hits` for every search. */
function fakeCluster(hits: (body: Record<string, unknown>, path: string) => unknown[]) {
  esJson.mockImplementation(async (_id: string, _method: string, path: string) => {
    if (path.startsWith("/_resolve/index/")) return { indices: [{ name: "logs-web" }, { name: "logs-api" }], aliases: [], data_streams: [] };
    if (path.includes("/_field_caps")) return { fields: { "@timestamp": { date: { type: "date", aggregatable: true } }, host: { keyword: { type: "keyword", aggregatable: true } } } };
    return {};
  });
  esRaw.mockImplementation(async (_id: string, _method: string, path: string, body: Record<string, unknown>) => {
    const value = { took: 1, hits: { total: { value: 1, relation: "eq" }, hits: hits(body, path) }, aggregations: { over_time: { buckets: [] } } };
    return { value, text: JSON.stringify(value) };
  });
}

const doc = (id: string, at: number, host: string) => ({ _index: "logs-web", _id: id, _source: { "@timestamp": new Date(at).toISOString(), host }, fields: { "@timestamp": [String(at)] }, sort: [at, Number(id)] });
const T = Date.UTC(2026, 10, 26, 12, 0, 0);

it("opens a saved search, and saves the view as a new one or over the one opened", async () => {
  const user = userEvent.setup();
  fakeCluster(() => [doc("1", T, "web-1")]);
  const errors = { id: "ss_1", name: "errors", search: { view: "logs-api", time_field: "@timestamp", mode: "kql", query: "status >= 500", range: { last: "1h" }, columns: ["host"], sort: null }, made_by: "ana", created_at: 1, updated_at: 1 };
  saved.savedSearches.mockResolvedValue({ saved_searches: [errors] });
  saved.updateSavedSearch.mockImplementation(async (_c: string, id: string, change: { search: unknown }) => ({ ...errors, id, search: change.search }));
  saved.saveSearch.mockImplementation(async (_c: string, name: string, search: unknown) => ({ ...errors, id: "ss_2", name, search }));
  window.location.hash = "#discover?view=logs-web";
  render(<DiscoverTab cluster={cluster} onOpenConsole={vi.fn()} />);

  await user.click(await screen.findByRole("button", { name: "Saved searches" }));
  await user.click(await screen.findByRole("button", { name: "Open errors" }));
  expect(screen.getByRole("textbox", { name: "Query" })).toHaveValue("status >= 500");
  expect(screen.getByRole("combobox", { name: "View" })).toHaveValue("logs-api");
  await vi.waitFor(() => expect(window.location.hash).toContain("view=logs-api"));
  // Unchanged, there's nothing to save over it.
  expect(screen.getByRole("button", { name: "Save to errors" })).toBeDisabled();

  await user.clear(screen.getByRole("textbox", { name: "Query" }));
  await user.type(screen.getByRole("textbox", { name: "Query" }), "status >= 400{Enter}");
  await user.click(screen.getByRole("button", { name: "Save to errors" }));
  expect(saved.updateSavedSearch).toHaveBeenCalledWith("c1", "ss_1", { search: expect.objectContaining({ view: "logs-api", query: "status >= 400", range: { last: "1h" } }) });

  await user.type(screen.getByRole("textbox", { name: "Saved search name" }), "4xx and 5xx{Enter}");
  expect(saved.saveSearch).toHaveBeenCalledWith("c1", "4xx and 5xx", expect.objectContaining({ query: "status >= 400", columns: ["host"] }));
  expect(screen.getByRole("button", { name: "Saved searches" })).toHaveTextContent("4xx and 5xx");
});

it("shows a document among the ones just before and after it in time", async () => {
  const user = userEvent.setup();
  fakeCluster((body, path) => {
    if (path === "/logs-web/_search" && (body.query as { ids?: unknown }).ids) return [doc("5", T, "anchor-host")];
    if (body.search_after) {
      const asc = JSON.stringify(body.sort).includes('"asc"');
      return asc ? [doc("6", T + 1000, "newer-host")] : [doc("4", T - 1000, "older-host")];
    }
    return [doc("5", T, "anchor-host")];
  });
  window.location.hash = "#discover?view=logs-web&col=host";
  render(<DiscoverTab cluster={cluster} onOpenConsole={vi.fn()} />);

  await user.click(await screen.findByText("anchor-host"));
  await user.click(await screen.findByRole("button", { name: "Surrounding documents" }));
  const around = await screen.findByRole("region", { name: "Surrounding documents" });
  await within(around).findByText("older-host");
  const rows = within(around).getAllByRole("row").slice(1).map((r) => r.textContent);
  expect(rows.map((r) => r?.replace(/^.*\.\d{3}/, ""))).toEqual(["newer-host", "anchor-host", "older-host"]);
  expect(within(around).getAllByRole("row").find((r) => r.getAttribute("aria-current"))).toHaveTextContent("anchor-host");
  // Both sides search after the anchor's own sort values, over every time.
  const after = esRaw.mock.calls.map((c) => c[3]).filter((b) => b?.search_after);
  expect(after.map((b) => b.search_after)).toEqual([[T, 5], [T, 5]]);
  expect(JSON.stringify(after)).not.toContain("now-24h");

  await user.click(within(around).getByRole("button", { name: "Documents" }));
  expect(screen.queryByRole("region", { name: "Surrounding documents" })).not.toBeInTheDocument();
});
