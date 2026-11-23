import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import type { SearchCluster } from "@/lib/search-api";
import { ConnectorsTab } from "./connectors";

const esJson = vi.hoisted(() => vi.fn());
vi.mock("@/lib/search-api", async (orig) => ({ ...(await orig<typeof import("@/lib/search-api")>()), esJson }));

const cluster = { id: "c1", name: "shop", url: "https://shop.search.example", flavor: "elasticsearch" } as unknown as SearchCluster;
const PASSWORD = "pg-password-never-shown";

afterEach(() => vi.useRealTimers());

it("opens a Postgres connector in the console, never shows its password, and follows Sync now to its end", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  const connector = {
    id: "orders-db",
    name: "Orders",
    service_type: "postgresql",
    index_name: "content-orders",
    is_native: true,
    status: "configured",
    configuration: { host: { value: "db.example" }, password: { label: "Password", sensitive: true, value: PASSWORD } },
  };
  const job = (status: string, indexed: number) => ({ id: "j1", status, job_type: "full", connector: { id: "orders-db" }, indexed_document_count: indexed, deleted_document_count: 0, total_document_count: indexed, created_at: "2026-11-22T10:00:00Z" });
  let jobs: unknown[] = [];
  esJson.mockImplementation(async (_id: string, method: string, path: string) => {
    if (method === "GET" && path.startsWith("/_connector?")) return { results: [connector] };
    if (method === "GET" && path.startsWith("/_connector/_sync_job")) return { results: jobs };
    if (method === "POST" && path === "/_connector/_sync_job") {
      jobs = [job("in_progress", 120)];
      return { id: "j1" };
    }
    throw new Error(`unexpected ${method} ${path}`);
  });
  const onOpenConsole = vi.fn();
  const { container } = render(<ConnectorsTab cluster={cluster} onOpenConsole={onOpenConsole} />);

  const row = (await screen.findByText("Orders")).closest("tr")!;
  expect(container).not.toHaveTextContent(PASSWORD);
  await user.click(screen.getByRole("button", { name: /new connector/i }));
  expect(onOpenConsole.mock.calls[0][0]).toMatch(/PUT _connector\/[\s\S]*"service_type": "postgresql"/);

  await user.click(within(row).getByRole("button", { name: /sync now/i }));
  expect(esJson).toHaveBeenCalledWith("c1", "POST", "/_connector/_sync_job", { id: "orders-db", job_type: "full", trigger_method: "on_demand" });
  expect(await within(row).findByText("Syncing…")).toBeInTheDocument();
  expect(within(row).getByText("full · +120 −0")).toBeInTheDocument();

  jobs = [job("completed", 340)];
  connector.status = "connected";
  Object.assign(connector, { last_sync_status: "completed", last_synced: new Date().toISOString() });
  await act(() => vi.advanceTimersByTimeAsync(3000));
  expect(await within(row).findByText(/^completed/)).toBeInTheDocument();
  expect(within(row).queryByText("Syncing…")).not.toBeInTheDocument();
  expect(within(row).getByText("340")).toBeInTheDocument();
  expect(container).not.toHaveTextContent(PASSWORD);
});
