import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { Catalog, Rule } from "@/lib/access-api";
import { Builder } from "./builder";

vi.mock("@/lib/workspace-resources", () => ({
  loadInventory: async () => ({ views: new Map(), kinds: new Set() }),
  resourcesByProduct: () => ({ postgres: [{ id: "prj_billing", name: "billing" }, { id: "prj_logs", name: "logs" }], search: [{ id: "cl_1", name: "events" }] }),
}));
vi.mock("@/lib/access-api", async (orig) => ({
  ...(await orig<typeof import("@/lib/access-api")>()),
  accessApi: { searchIndices: async () => [{ index: "logs-2025" }, { index: ".internal" }], encryptionKeys: async () => [{ name: "webhooks" }] },
}));
vi.mock("@/lib/workspace-store", () => ({ useWorkspace: () => ({ projects: [{ id: "prj_shop", name: "shop" }] }) }));

/**
 * The permission builder: ticking a product's resources by name narrows that product's rule to
 * them, and the summary says what the token can do with their names.
 */

const catalog: Catalog = { products: ["postgres", "redis", "search", "secrets"], actions: [], presets: [], routes: [], operator_only: [], api_url: "" };

function Harness({ onRules, start = [{ product: "postgres", resources: "*", actions: { read: "allow" } }] }: { onRules: (r: Rule[]) => void; start?: Rule[] }) {
  const [rules, setRules] = useState<Rule[]>(start);
  return (
    <Builder
      catalog={catalog}
      rules={rules}
      onChange={(r) => {
        setRules(r);
        onRules(r);
      }}
    />
  );
}

describe("the builder", () => {
  it("narrows a product to the resources ticked by name", async () => {
    const seen: Rule[][] = [];
    render(<Harness onRules={(r) => seen.push(r)} />);
    const rows = screen.getAllByRole("button", { name: /all/ });
    await userEvent.click(rows[0]);
    await userEvent.click(await screen.findByRole("checkbox", { name: /billing/ }));
    await waitFor(() => expect(seen.at(-1)).toEqual([{ product: "postgres", resources: [{ id: "prj_billing" }], actions: { read: "allow" } }]));
    expect(screen.getByText(/Postgres · read · billing/)).toBeTruthy();
    expect(screen.getByText("Anything else is refused.")).toBeTruthy();
  });

  it("narrows search to some indices of a cluster, ticked by name", async () => {
    const seen: Rule[][] = [];
    render(<Harness onRules={(r) => seen.push(r)} start={[{ product: "search", resources: "*", actions: { read: "allow" } }]} />);
    // Products in the catalogue's order: search is the one before secrets.
    await userEvent.click(screen.getAllByRole("button", { name: "all" }).at(-2)!);
    await userEvent.click(await screen.findByRole("button", { name: /Only some indices of events/ }));
    expect(screen.queryByText(".internal")).toBeNull();
    await userEvent.click(await screen.findByRole("checkbox", { name: "events index logs-2025" }));
    await waitFor(() => expect(seen.at(-1)).toEqual([{ product: "search", resources: [{ id: "cl_1", index: "logs-2025" }], actions: { read: "allow" } }]));
    expect(screen.getByText(/Search · read · events indices logs-2025/)).toBeTruthy();
  });

  it("narrows secrets to one encryption key of a project, so signing needn't be write on the project", async () => {
    const seen: Rule[][] = [];
    render(<Harness onRules={(r) => seen.push(r)} start={[{ product: "secrets", resources: "*", actions: { write: "allow" } }]} />);
    await userEvent.click(screen.getAllByRole("button", { name: "all" }).at(-1)!);
    await userEvent.click(await screen.findByRole("button", { name: /Only some encryption keys of shop/ }));
    await userEvent.click(await screen.findByRole("checkbox", { name: "shop key webhooks" }));
    await waitFor(() => expect(seen.at(-1)).toEqual([{ product: "secrets", resources: [{ id: "prj_shop", index: "webhooks" }], actions: { write: "allow" } }]));
  });
});
