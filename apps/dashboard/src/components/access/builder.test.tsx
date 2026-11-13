import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { Catalog, Rule } from "@/lib/access-api";
import { Builder } from "./builder";

vi.mock("@/lib/workspace-resources", () => ({
  loadInventory: async () => ({ views: new Map(), kinds: new Set() }),
  resourcesByProduct: () => ({ postgres: [{ id: "prj_billing", name: "billing" }, { id: "prj_logs", name: "logs" }] }),
}));
vi.mock("@/lib/workspace-store", () => ({ useWorkspace: () => ({ projects: [] }) }));

/**
 * The permission builder: ticking a product's resources by name narrows that product's rule to
 * them, and the summary says what the token can do with their names.
 */

const catalog: Catalog = { products: ["postgres", "redis"], actions: [], presets: [], routes: [], operator_only: [], api_url: "" };

function Harness({ onRules }: { onRules: (r: Rule[]) => void }) {
  const [rules, setRules] = useState<Rule[]>([{ product: "postgres", resources: "*", actions: { read: "allow" } }]);
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
});
