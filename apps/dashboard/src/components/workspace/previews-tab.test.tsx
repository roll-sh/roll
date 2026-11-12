import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MarkStateProvider } from "@/components/mark-state";
import { previewsApi, type Preview, type PreviewSettings } from "@/lib/previews-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { ProjectPreviews } from "./previews";

vi.mock("@/lib/previews-api", async (orig) => {
  const real = await orig<typeof import("@/lib/previews-api")>();
  const fns = Object.fromEntries(Object.keys(real.previewsApi).map((k) => [k, vi.fn()]));
  return { ...real, previewsApi: fns };
});

const api = vi.mocked(previewsApi);
const T = Date.now();

const project: WorkspaceProject = {
  id: "prj_shop",
  name: "shop",
  description: "",
  icon: null,
  created_at: T,
  resources: [
    { kind: "repo", ref: "acme/web", x: 48, y: 48, added_at: T },
    { kind: "postgres", ref: "pgp_main", x: 320, y: 48, added_at: T },
  ],
};

const preview: Preview = {
  id: "pv_abc",
  project: "prj_shop",
  slug: "pr482-shop",
  repo: "acme/web",
  change: "chg_1",
  number: 12,
  github_pr: 482,
  branch: "feat-x",
  title: "Checkout redesign",
  head: "a1b2c3d4e5f6",
  trusted: true,
  desired: "active",
  state: "ready",
  error: null,
  url: "https://web-pr482-shop.preview.roll.site",
  services: [{ name: "web", repo: "acme/web", url: "https://web-pr482-shop.preview.roll.site", revision: "a1b2", deployed: "a1b2", state: "running", error: null }],
  resources: [{ kind: "postgres", parent: "pgp_main", ref: "br_pv1", mode: "branch", state: "ready", error: null, bytes: 1 << 20 }],
  cost: { sleeping_usd_month: 0.02, active_usd_hour: 0.01, month_to_date_usd: 0.4, bytes: 1 << 20 },
  last_reset_at: null,
  last_active_at: T,
  expires_at: null,
  created_by: "github",
  created_at: T,
  updated_at: T,
};

const settings = { enabled: true } as PreviewSettings;

beforeEach(() => {
  vi.clearAllMocks();
  api.list.mockResolvedValue([preview]);
  api.settings.mockResolvedValue(settings);
  api.usage.mockResolvedValue({ previews: 1, sleeping_usd_month: 0.02, active_usd_hour: 0.01, month_to_date_usd: 0.4, bytes: 1 << 20 });
  api.activity.mockResolvedValue([]);
});

const view = () =>
  render(
    <MarkStateProvider>
      <ProjectPreviews project={project} inventory={new Map()} />
    </MarkStateProvider>,
  );

describe("the Previews tab", () => {
  it("puts a preview to sleep, then deletes it after the slug is typed, and the list follows", async () => {
    const u = userEvent.setup({ delay: null });
    view();
    const list = await screen.findByRole("list", { name: "Previews" });
    await u.click(within(list).getByRole("button", { name: "Checkout redesign: Ready" }));

    api.sleep.mockResolvedValue({ ...preview, state: "sleeping", services: [{ ...preview.services[0], state: "sleeping" }] });
    await u.click(screen.getByRole("button", { name: "Sleep" }));
    expect(api.sleep).toHaveBeenCalledWith("pv_abc");
    expect(await within(list).findByRole("button", { name: "Checkout redesign: Asleep" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Wake" })).toBeInTheDocument();

    api.remove.mockResolvedValue({ ...preview, desired: "deleted", state: "deleting" });
    await u.click(screen.getByRole("button", { name: "Delete" }));
    const confirm = screen.getByRole("button", { name: "Delete preview" });
    expect(confirm).toBeDisabled();
    await u.type(screen.getByRole("textbox"), "pr482-shop");
    await u.click(confirm);
    expect(api.remove).toHaveBeenCalledWith("pv_abc");
    expect(await within(list).findByRole("button", { name: "Checkout redesign: Deleting" })).toBeInTheDocument();
  });
});
