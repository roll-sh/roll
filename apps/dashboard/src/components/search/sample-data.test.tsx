import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import type { SearchCluster } from "@/lib/search-api";
import { SampleData } from "./sample-data";

const api = vi.hoisted(() => ({ createKey: vi.fn(), request: vi.fn() }));
const esRaw = vi.hoisted(() => vi.fn());
vi.mock("@/lib/search-api", async (orig) => ({ ...(await orig<typeof import("@/lib/search-api")>()), searchApi: api, esRaw }));

const cluster = { id: "c1", name: "shop", url: "https://shop.search.example", flavor: "elasticsearch", max_body_mb: 100 } as unknown as SearchCluster;
const KEY = "c2VjcmV0LWtleS1vbmNl";

it("makes a data key in place, shows it once and fills it into the copied code, never storing it", async () => {
  const user = userEvent.setup();
  esRaw.mockResolvedValue({ value: { took: 1, hits: { total: { value: 0 }, hits: [] } }, text: "" });
  api.createKey.mockResolvedValue({ id: "k1", name: "first queries", api_key: KEY, url: cluster.url, expires_at: 1_800_000_000 });
  render(<SampleData cluster={cluster} present={new Set(["sample-products"])} onChanged={() => {}} onOpenConsole={() => {}} onManageKeys={() => {}} />);

  await user.click(await screen.findByRole("button", { name: /make a key/i }));
  expect(api.createKey).toHaveBeenCalledWith("c1", "first queries", "data", 30 * 86_400);
  const once = await screen.findByRole("group", { name: "New key" });
  expect(once).toHaveTextContent("shown once");
  await user.click(screen.getByRole("button", { name: "Reveal" }));
  expect(once).toHaveTextContent(KEY);

  await user.click(screen.getByRole("button", { name: "curl" }));
  expect(await navigator.clipboard.readText()).toContain(`Authorization: ApiKey ${KEY}`);

  await user.click(screen.getByRole("button", { name: /saved it/i }));
  await waitFor(() => expect(screen.queryByText(KEY)).not.toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: "python" }));
  expect(await navigator.clipboard.readText()).toContain(`api_key=${JSON.stringify(KEY)}`);

  for (const store of [localStorage, sessionStorage])
    for (let i = 0; i < store.length; i++) expect(store.getItem(store.key(i) ?? "")).not.toContain(KEY);
});
