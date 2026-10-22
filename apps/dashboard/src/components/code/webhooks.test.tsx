import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Activity } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Webhook } from "@/lib/code-api";
import { api, resetApi } from "@/test/api";
import { repo, T0 } from "@/test/fixtures";
import type { Run } from "./shared";
import { WebhooksPanel } from "./webhooks";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), ...(await import("@/test/api")).apiModule }));

/** Repo webhooks: a generated secret is shown once after adding one, and the list never shows it. */

const SECRET = "whsec_GENERATED_WEBHOOK_SECRET_42";
const hook: Webhook = { id: "wh_1", url: "https://ci.example.com/hooks/roll", events: ["landed"], active: true, created_by: "ada", created_at: T0, updated_at: T0 };

const run: Run = async (_key, fn) => fn();
const count = () => (document.body.outerHTML.match(new RegExp(SECRET, "g")) ?? []).length;
const list = () => document.querySelector<HTMLElement>("ul.border-t")!;
const panel = (mode: "visible" | "hidden" = "visible") => (
  <Activity mode={mode}>
    <WebhooksPanel repo={repo()} writer busy={null} run={run} />
  </Activity>
);

beforeEach(() => {
  resetApi();
  api.webhooks.mockResolvedValue([]);
  api.createWebhook.mockResolvedValue({ webhook: hook, ping: "dlv_1", secret: SECRET });
});

async function add(secret = "") {
  const u = userEvent.setup();
  await screen.findByText("No webhooks yet.");
  await u.click(screen.getByRole("button", { name: /Add webhook/ }));
  await u.type(screen.getByLabelText(/^Payload URL/), "https://ci.example.com/hooks/roll");
  if (secret) await u.type(screen.getByLabelText(/^Secret/), secret);
  api.webhooks.mockResolvedValue([hook]);
  await u.click(screen.getByRole("button", { name: "Add webhook" }));
  await screen.findByText(/and sent it a/);
  return u;
}

describe("WebhooksPanel: a generated secret is shown once", () => {
  it("shows it after adding; the reloaded list never has it", async () => {
    render(panel());
    const u = await add();
    expect(api.createWebhook).toHaveBeenCalledWith("acme/web", { url: "https://ci.example.com/hooks/roll", events: ["landed"], active: true });
    expect(screen.getByText("Shown once. Check every delivery's signature with this secret.")).toBeInTheDocument();
    expect(screen.getByText(SECRET).tagName).toBe("CODE");
    expect(count()).toBe(1);

    await waitFor(() => expect(within(list()).getByText(hook.url)).toBeInTheDocument());
    expect(api.webhooks).toHaveBeenCalledTimes(2);
    expect(list().innerHTML).not.toContain(SECRET);

    await u.click(screen.getByRole("button", { name: "I've saved it" }));
    expect(count()).toBe(0);
  });

  it("a secret the user typed isn't echoed back", async () => {
    api.createWebhook.mockResolvedValue({ webhook: hook, ping: "dlv_1" });
    render(panel());
    await add("my-own-secret-0123456789");
    expect(api.createWebhook).toHaveBeenCalledWith("acme/web", expect.objectContaining({ secret: "my-own-secret-0123456789" }));
    expect(screen.getByText("Deliveries are signed with the secret you gave.")).toBeInTheDocument();
    expect(document.body.innerHTML).not.toContain("my-own-secret-0123456789");
  });

  it("is gone after Next hides the page and shows it again (cacheComponents keeps pages in <Activity>)", async () => {
    const { rerender } = render(panel());
    await add();
    rerender(panel("hidden"));
    rerender(panel("visible"));
    await waitFor(() => expect(count()).toBe(0));
    expect(screen.queryByText(/and sent it a/)).toBeNull();
    expect(within(list()).getByText(hook.url)).toBeInTheDocument();
  });

  it("is gone after leaving the page and coming back", async () => {
    const { unmount } = render(panel());
    await add();
    unmount();
    render(panel());
    await screen.findByText(hook.url);
    expect(count()).toBe(0);
  });
});
