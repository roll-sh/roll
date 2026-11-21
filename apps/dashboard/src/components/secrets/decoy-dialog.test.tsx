import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Environment, LinkTarget, NewDecoy } from "@/lib/secrets-api";
import { DecoyDialog } from "./decoy-dialog";

const api = {
  links: vi.fn<() => Promise<LinkTarget[]>>(),
  addDecoy: vi.fn(async (_env: string, d: NewDecoy) => ({ key: d.key })),
};
vi.mock("./secrets-page", () => ({ useSecrets: () => ({ api, project: { id: "prj_1", name: "shop" } }) }));

const env = { slug: "production", name: "Production" } as Environment;
const open = () => render(<DecoyDialog open onClose={() => {}} env={env} path="/" taken={new Set()} onPlanted={() => {}} />);
const choosePostgres = () => fireEvent.click(screen.getByRole("button", { name: "Postgres login" }));

describe("a Postgres decoy", () => {
  it("needs a database on the canvas, and is planted in the one chosen", async () => {
    api.links.mockResolvedValueOnce([{ kind: "bucket", ref: "b1", name: "files", fields: [] }]);
    const first = open();
    choosePostgres();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "LEGACY_DATABASE_URL" } });
    expect(await screen.findByText(/has no Postgres database on its canvas/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Plant decoy" })).toBeDisabled();
    first.unmount();

    api.links.mockResolvedValueOnce([
      { kind: "postgres", ref: "db_main", name: "shop-db", fields: [] },
      { kind: "postgres", ref: "db_reports", name: "reports", fields: [] },
    ]);
    open();
    choosePostgres();
    fireEvent.change(await screen.findByRole("combobox"), { target: { value: "db_reports" } });
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "LEGACY_DATABASE_URL" } });
    fireEvent.click(screen.getByRole("button", { name: "Plant decoy" }));
    await waitFor(() => expect(api.addDecoy).toHaveBeenCalledWith("production", { key: "LEGACY_DATABASE_URL", kind: "postgres", resource: "db_reports", path: undefined }));
  });
});
