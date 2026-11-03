import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Activity } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReviewApp } from "@/lib/code-api";
import { api, resetApi } from "@/test/api";
import { repo, T0 } from "@/test/fixtures";
import { ReviewApps } from "./owner";
import type { Run } from "./shared";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), ...(await import("@/test/api")).apiModule }));

/**
 * Review apps on the owner page: the token and secret are shown once after registering or
 * rotating, and the list never shows them.
 */

const TOKEN = "rla_tok_REVIEW_APP_TOKEN_0123456789";
const SECRET = "whsec_REVIEW_APP_SECRET_abcdef";
const app = (over: Partial<ReviewApp> = {}): ReviewApp => ({
  id: "app_1",
  owner: "acme",
  name: "greptile",
  webhook_url: "https://greptile.example/hooks/roll",
  events: ["change.opened", "change.revised", "thread.comment"],
  repos: [],
  all_repos: true,
  required: false,
  active: true,
  created_at: T0,
  updated_at: T0,
  ...over,
});

const run: Run = async (_key, fn) => fn();
const count = (s: string) => (document.body.outerHTML.match(new RegExp(s, "g")) ?? []).length;
const list = () => document.querySelector<HTMLElement>("ul")!;
const panel = (mode: "visible" | "hidden") => (
  <Activity mode={mode}>
    <ReviewApps owner="acme" repos={[repo()]} manage busy={null} run={run} />
  </Activity>
);

beforeEach(() => {
  resetApi();
  api.reviewApps.mockResolvedValue([]);
  api.createReviewApp.mockResolvedValue({ app: app(), token: TOKEN, secret: SECRET });
  api.rotateReviewApp.mockResolvedValue({ token: TOKEN, secret: SECRET });
});

async function register() {
  const u = userEvent.setup();
  await screen.findByText("No review apps yet.");
  await u.click(screen.getByRole("button", { name: /Register/ }));
  await u.type(screen.getByLabelText(/^Name/), "greptile");
  await u.type(screen.getByLabelText(/^Webhook URL/), "greptile.example/hooks/roll");
  api.reviewApps.mockResolvedValue([app()]);
  await u.click(screen.getByRole("button", { name: "Register app" }));
  await screen.findByText("Registered greptile.");
  return u;
}

describe("ReviewApps: credentials are shown once", () => {
  it("shows the token and secret after registering; the reloaded list never has them", async () => {
    render(panel("visible"));
    const u = await register();
    expect(api.createReviewApp).toHaveBeenCalledWith("acme", {
      name: "greptile",
      webhook_url: "https://greptile.example/hooks/roll",
      events: ["change.opened", "change.revised", "thread.comment"],
      required: false,
    });
    const once = screen.getByText("Registered greptile.").parentElement!;
    expect(within(once).getByText(TOKEN)).toBeInTheDocument();
    expect(within(once).getByText(SECRET)).toBeInTheDocument();
    expect(count(TOKEN)).toBe(1);
    expect(count(SECRET)).toBe(1);

    // The list was fetched again: the app by name and URL, no credentials.
    await waitFor(() => expect(within(list()).getByText("greptile")).toBeInTheDocument());
    expect(api.reviewApps).toHaveBeenCalledTimes(2);
    expect(list().innerHTML).not.toContain(TOKEN);
    expect(list().innerHTML).not.toContain(SECRET);

    await u.click(screen.getByRole("button", { name: "I've saved them" }));
    expect(count(TOKEN) + count(SECRET)).toBe(0);
  });

  it("shows new credentials after rotating", async () => {
    api.reviewApps.mockResolvedValue([app()]);
    render(panel("visible"));
    await screen.findByText("greptile");
    await userEvent.click(screen.getByRole("button", { name: /Rotate/ }));
    expect(api.rotateReviewApp).toHaveBeenCalledWith("app_1");
    expect(await screen.findByText("New credentials for greptile. The old ones stopped working.")).toBeInTheDocument();
    expect(count(TOKEN)).toBe(1);
    expect(count(SECRET)).toBe(1);
    expect(list().innerHTML).not.toContain(SECRET);
  });

  it("is gone after Next hides the page and shows it again (cacheComponents keeps pages in <Activity>)", async () => {
    const { rerender } = render(panel("visible"));
    await register();
    rerender(panel("hidden"));
    rerender(panel("visible"));
    await waitFor(() => expect(count(TOKEN) + count(SECRET)).toBe(0));
    expect(screen.queryByText("Registered greptile.")).toBeNull();
    expect(screen.getByText("greptile")).toBeInTheDocument();
  });

  it("rotated credentials are gone after a hide and show too", async () => {
    api.reviewApps.mockResolvedValue([app()]);
    const { rerender } = render(panel("visible"));
    await screen.findByText("greptile");
    await userEvent.click(screen.getByRole("button", { name: /Rotate/ }));
    await screen.findByText(/New credentials for greptile/);
    rerender(panel("hidden"));
    rerender(panel("visible"));
    await waitFor(() => expect(count(TOKEN) + count(SECRET)).toBe(0));
  });

  it("is gone after leaving the page and coming back", async () => {
    const { unmount } = render(panel("visible"));
    await register();
    unmount();
    render(panel("visible"));
    await screen.findByText("greptile");
    expect(count(TOKEN) + count(SECRET)).toBe(0);
  });
});
