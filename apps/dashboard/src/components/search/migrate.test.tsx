import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import type { SearchCluster } from "@/lib/search-api";
import { MigratePanel } from "./migrate";

const migrateApi = vi.hoisted(() => ({ inspect: vi.fn(), start: vi.fn() }));
const search = vi.hoisted(() => ({ esJson: vi.fn(), request: vi.fn() }));
vi.mock("@/lib/search-migrate", async (orig) => ({ ...(await orig<typeof import("@/lib/search-migrate")>()), migrateApi }));
vi.mock("@/lib/search-api", async (orig) => {
  const real = await orig<typeof import("@/lib/search-api")>();
  return { ...real, esJson: search.esJson, searchApi: { ...real.searchApi, request: search.request } };
});

const cluster = { id: "c1", name: "shop", url: "https://shop.search.example", flavor: "elasticsearch" } as unknown as SearchCluster;
const KEY = "cmVtb3RlLWtleQ==";

afterEach(() => vi.useRealTimers());

it("inspects a remote, copies the picked index under a new name with progress, then points the old name at it", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  migrateApi.inspect.mockResolvedValue({
    remote: { host: "old.example:9200", version: "8.15.0", distribution: "elasticsearch", cluster_name: "prod" },
    indices: [
      { index: "products", docs: 100, size_bytes: 4096, aliases: [], aliases_on_roll: [] },
      { index: "logs", docs: 5, size_bytes: 512, aliases: [], aliases_on_roll: [] },
    ],
    data_streams: [],
  });
  migrateApi.start.mockResolvedValue({ indices: [{ index: "products", dest: "products-v2", task: "n1:7", created: true }] });
  const task = (completed: boolean, created: number) =>
    completed ? { completed: true, response: { total: 100, created, updated: 0, noops: 0, version_conflicts: 0, failures: [] } } : { completed: false, task: { status: { total: 100, created } } };
  search.request.mockResolvedValueOnce({ status: 200, body: JSON.stringify(task(false, 40)) }).mockResolvedValue({ status: 200, body: JSON.stringify(task(true, 100)) });
  search.esJson.mockResolvedValue({ acknowledged: true });
  const onChanged = vi.fn();
  const { container } = render(<MigratePanel cluster={cluster} existing={[]} onClose={() => {}} onChanged={onChanged} />);

  await user.type(screen.getByPlaceholderText(/elastic\.cloud/), "https://old.example:9200");
  await user.type(screen.getByLabelText("API key"), KEY);
  await user.click(screen.getByRole("button", { name: /list indices/i }));
  expect(migrateApi.inspect).toHaveBeenCalledWith("c1", expect.objectContaining({ host: "https://old.example:9200" }));

  await user.click(await screen.findByLabelText("Pick products"));
  const dest = screen.getByLabelText("Name of products's copy on roll");
  await user.clear(dest);
  await user.type(dest, "products-v2");
  await user.click(screen.getByRole("button", { name: "Migrate 1 index" }));
  expect(migrateApi.start).toHaveBeenCalledWith("c1", expect.objectContaining({ indices: [{ index: "products", dest: "products-v2" }] }));

  expect(await screen.findByText(/40 of 100 docs/)).toBeInTheDocument();
  expect(container).not.toHaveTextContent(KEY);
  await act(() => vi.advanceTimersByTimeAsync(2000));
  expect(await screen.findByText("Done")).toBeInTheDocument();
  expect(screen.getByText(/100 of 100 docs/)).toBeInTheDocument();

  expect(screen.getByLabelText("Alias name")).toHaveValue("products");
  await user.click(screen.getByRole("button", { name: "Point alias at products-v2" }));
  expect(search.esJson).toHaveBeenCalledWith("c1", "POST", "/_aliases", { actions: [{ add: { index: "products-v2", alias: "products" } }] });
  expect(await screen.findByText(/points at/)).toHaveTextContent("Alias products points at products-v2.");
});

it("maps an Algolia index for review before anything is made, then starts the copy", async () => {
  const user = userEvent.setup();
  const ALGOLIA_KEY = "algolia-admin-key-123";
  migrateApi.inspect.mockResolvedValue({
    remote: { host: "algolia app [APP1]", version: "", distribution: "algolia", cluster_name: null },
    indices: [
      { index: "products", docs: 3, size_bytes: 512, aliases: [], aliases_on_roll: [] },
      { index: "products_price_asc", docs: 3, size_bytes: 512, aliases: [], aliases_on_roll: [], replica_of: "products" },
    ],
    data_streams: [],
  });
  const plan = {
    id_field: "objectID", docs: 3, mappings: 3, default_fields: ["name^2"], synonyms: 2, rules: 1, search_template: "products-search",
    not_mapped: [{ kind: "rule", name: "filter-acme", reason: "query rules pin or exclude documents; this rule only sets its params" }], not_mapped_more: 0, notes: [],
  };
  migrateApi.start.mockResolvedValueOnce({ indices: [{ index: "products", dest: "products", plan }] }).mockResolvedValueOnce({ indices: [{ index: "products", dest: "products", task: "n1:3", created: true, plan }] });
  search.request.mockResolvedValue({ status: 200, body: JSON.stringify({ completed: false, task: { status: { total: 3, created: 1 } } }) });
  const { container } = render(<MigratePanel cluster={cluster} existing={[]} onClose={() => {}} onChanged={() => {}} />);

  await user.click(screen.getByRole("radio", { name: "Algolia" }));
  await user.type(screen.getByLabelText("Application ID"), "APP1");
  await user.type(screen.getByLabelText("API key"), ALGOLIA_KEY);
  await user.click(screen.getByRole("button", { name: /list indices/i }));
  expect(migrateApi.inspect).toHaveBeenCalledWith("c1", { kind: "algolia", app_id: "APP1", api_key: ALGOLIA_KEY });
  expect(await screen.findByText("replica of products")).toBeInTheDocument();

  await user.click(screen.getByLabelText("Pick products"));
  await user.click(screen.getByRole("button", { name: "Review mapping" }));
  expect(migrateApi.start).toHaveBeenLastCalledWith("c1", expect.objectContaining({ dry_run: true, indices: [{ index: "products", dest: "products" }] }));
  expect(await screen.findByText("filter-acme")).toBeInTheDocument();
  expect(screen.getByText(/1 thing didn't map/)).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Migrate 1 index" }));
  expect(migrateApi.start).toHaveBeenLastCalledWith("c1", expect.not.objectContaining({ dry_run: true }));
  expect(await screen.findByText(/1 of 3 docs/)).toBeInTheDocument();
  expect(container).not.toHaveTextContent(ALGOLIA_KEY);
});
