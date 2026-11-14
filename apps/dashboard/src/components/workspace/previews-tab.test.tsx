import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MarkStateProvider } from "@/components/mark-state";
import { codeApi } from "@/lib/code-api";
import { previewsApi, type Preview, type PreviewSettings } from "@/lib/previews-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { ProjectPreviews } from "./previews";

vi.mock("@/lib/previews-api", async (orig) => {
  const real = await orig<typeof import("@/lib/previews-api")>();
  const fns = Object.fromEntries(Object.keys(real.previewsApi).map((k) => [k, vi.fn()]));
  return { ...real, previewsApi: fns };
});

vi.mock("@/lib/code-api", async (orig) => {
  const real = await orig<typeof import("@/lib/code-api")>();
  return { ...real, codeApi: { ...real.codeApi, submitReview: vi.fn(), reviews: vi.fn() } };
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
  share_url: null,
  services: [{ name: "web", repo: "acme/web", url: "https://web-pr482-shop.preview.roll.site", share_url: null, revision: "a1b2", deployed: "a1b2", state: "running", error: null }],
  resources: [{ kind: "postgres", parent: "pgp_main", ref: "br_pv1", mode: "branch", state: "ready", error: null, bytes: 1 << 20, data: "real" }],
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
  api.vars.mockResolvedValue({ vars: {} });
  api.shares.mockResolvedValue([]);
  api.mail.mockResolvedValue({ mail: [] });
  // The sheet's full view: the list's item stands in where a test doesn't give one.
  api.get.mockRejectedValue(new Error("not in this test"));
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
    await u.type(screen.getByRole("textbox", { name: /to confirm/ }), "pr482-shop");
    await u.click(confirm);
    expect(api.remove).toHaveBeenCalledWith("pv_abc");
    expect(await within(list).findByRole("button", { name: "Checkout redesign: Deleting" })).toBeInTheDocument();
  });

  it("lists the previews waiting for a visual review first, tagged", async () => {
    const waiting: Preview = { ...preview, id: "pv_review", slug: "pr483-shop", title: "New header", visual_review: { state: "changed", branch: "main", percent: 8, by: null, at: null, required: false, head: "cs_1", pages: [{ path: "", percent: 8, by: null, at: null }] } };
    api.list.mockResolvedValue([preview, waiting]);
    view();
    const list = await screen.findByRole("list", { name: "Previews" });
    const rows = within(list).getAllByRole("button", { name: /: Ready$/ });
    expect(rows.map((r) => r.getAttribute("aria-label"))).toEqual(["New header: Ready", "Checkout redesign: Ready"]);
    expect(within(list).getAllByText("to review")).toHaveLength(1);
  });

  it("tags a preview whose page added problems against main", async () => {
    const worse: Preview = {
      ...preview,
      id: "pv_worse",
      title: "New footer",
      page_links: { checked: 2, broken: [{ path: "/gone", status: 404 }] },
      base_change: { branch: "main", percent: 0, phone_percent: null, links_added: 1, errors_added: 1 },
    };
    api.list.mockResolvedValue([preview, worse]);
    view();
    const list = await screen.findByRole("list", { name: "Previews" });
    expect(await within(list).findByText("2 new problems")).toBeInTheDocument();
    expect(within(list).getAllByText(/new problems?$/)).toHaveLength(1);
  });

  it("posts a note from the preview page's feedback button on the change, once", async () => {
    const u = userEvent.setup({ delay: null });
    vi.mocked(codeApi.submitReview).mockResolvedValue({} as never);
    const posted = vi.fn();
    render(
      <MarkStateProvider>
        <ProjectPreviews
          project={project}
          inventory={new Map()}
          open="pv_abc"
          feedback={{ path: "/checkout", selector: "button.pay", viewport: "1280x800", note: "The button is cut off" }}
          onFeedbackPosted={posted}
        />
      </MarkStateProvider>,
    );
    await u.click(await screen.findByRole("button", { name: "Post on the change" }));
    expect(codeApi.submitReview).toHaveBeenCalledWith("chg_1", {
      verdict: "comment",
      body: expect.stringContaining("· `button.pay`\n\nThe button is cut off"),
    });
    expect(await screen.findByText(/Posted on the change/)).toBeInTheDocument();
    expect(posted).toHaveBeenCalledOnce();
  });

  it("replies to and resolves a note from the preview page's buttons, on the preview", async () => {
    const u = userEvent.setup({ delay: null });
    vi.mocked(codeApi.reviews).mockResolvedValue([
      { id: "rv_1", body: "**Feedback on the preview** · `/`\n\nThe button is cut off", dismissed: false, created_at: 1, reviewer: { name: "ada" } },
      { id: "rv_2", body: "**Reply on the preview** · `rv_1`\n\nWhich width?", dismissed: false, created_at: 2, reviewer: { name: "bo" } },
    ] as never);
    vi.mocked(codeApi.submitReview).mockResolvedValue({} as never);
    api.resolveNote.mockResolvedValue({ resolved: "rv_1" });
    const done = vi.fn();
    render(
      <MarkStateProvider>
        <ProjectPreviews project={project} inventory={new Map()} open="pv_abc" resolve="rv_1" onFeedbackPosted={done} />
      </MarkStateProvider>,
    );
    expect(await screen.findByText("The button is cut off")).toBeInTheDocument();
    expect(screen.getByText("Which width?")).toBeInTheDocument();
    await u.type(screen.getByRole("textbox", { name: "Reply" }), "At 768");
    await u.click(screen.getByRole("button", { name: "Reply" }));
    expect(codeApi.submitReview).toHaveBeenCalledWith("chg_1", { verdict: "comment", body: "**Reply on the preview** · `rv_1`\n\nAt 768" });
    await u.click(screen.getByRole("button", { name: "Resolve" }));
    expect(api.resolveNote).toHaveBeenCalledWith("pv_abc", "rv_1");
    expect(await screen.findByText(/This note is resolved/)).toBeInTheDocument();
    expect(done).toHaveBeenCalledOnce();
  });
});
