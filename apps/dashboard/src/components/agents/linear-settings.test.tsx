import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi, type Mock } from "vitest";
import { DISCONNECTED, memoryLinear, type LinearApi, type LinearStatus } from "@/lib/linear-api";
import { memoryLookup } from "@/lib/lookup-api";
import { macClick } from "@/test/mac-click";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { LinearSettingsSection } from "./linear-settings";

const CONNECTED: LinearStatus = {
  ...DISCONNECTED,
  connected: true,
  connection: {
    kind: "oauth",
    workspace: { id: "org_1", name: "Acme", url_key: "acme", url: "https://linear.app/acme" },
    user: { id: "usr_roll", name: "roll" },
    agent: true,
    scopes: ["read", "write"],
    status: "ok",
    error: null,
    expires_at: null,
    created_at: Date.now() - 86_400_000,
    created_by: "acme",
    settings: { repos: { ENG: "roll" } },
    webhook: { mode: "app", url: "https://roll.example/v1/code/linear/webhook", status: "ok", secret_hint: null, last_delivery_at: Date.now() - 60_000, rejected_at: null, rejected_why: null },
  },
  deliveries: [
    { id: "ldl_1", at: Date.now() - 600_000, delivery: "d1", type: "Issue", action: "update", summary: "ENG-1 moved to In Progress", result: "ignored", events: 0 },
    { id: "ldl_2", at: Date.now() - 60_000, delivery: "d2", type: "AgentSessionEvent", action: "created", summary: "ENG-2: Ada assigned roll", result: "recorded", events: 1 },
  ],
};

type Spied = { [K in keyof LinearApi]: Mock<LinearApi[K]> };

/** `memoryLinear` with every call a spy. */
function spied(initial: LinearStatus, teams = [{ id: "t1", key: "ENG", name: "Engineering" }, { id: "t2", key: "WEB", name: "Web" }]): Spied {
  const api = memoryLinear(initial, teams);
  return Object.fromEntries(Object.entries(api).map(([k, f]) => [k, vi.fn(f)])) as unknown as Spied;
}

function show(linear: LinearApi) {
  const lookup = memoryLookup({ "code/repos": ["roll", "web", "infra"].map((r) => ({ id: r, label: r, key: `acme/${r}`, icon: "repo" })) });
  const h = agentsHarness({ ctx: { services: fakeServices({ linear, lookup }) } });
  renderAgents(<LinearSettingsSection />, h);
  return h;
}

beforeEach(() => window.history.replaceState(null, "", "/dashboard/agents#agents/settings/owner"));

describe("the Linear settings", () => {
  it("not connected: offers roll's app and an API key, and connects with the key", async () => {
    const linear = spied(DISCONNECTED);
    show(linear);
    expect(await screen.findByRole("button", { name: "Connect Linear" })).toBeEnabled();
    await userEvent.type(screen.getByLabelText("Linear API key"), "lin_api_0123456789{Enter}");
    await waitFor(() => expect(linear.putKey).toHaveBeenCalledWith("acme", "lin_api_0123456789"));
    // Connected with a key: the webhook is the owner's to make, so the secret field shows.
    expect(await screen.findByText("API key · as Ada Lovelace")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Acme/ })).toBeInTheDocument();
    expect(screen.getByText("Needs its secret")).toBeInTheDocument();
    expect(screen.getByLabelText("Webhook signing secret")).toBeInTheDocument();
  });

  it("Connect Linear asks roll for Linear's authorize URL, coming back to these settings", async () => {
    const linear = spied(DISCONNECTED);
    linear.connectOAuth.mockResolvedValue("#agents/settings/owner");
    show(linear);
    await userEvent.click(await screen.findByRole("button", { name: "Connect Linear" }));
    await waitFor(() => expect(linear.connectOAuth).toHaveBeenCalledWith("acme", "/dashboard/agents#agents/settings/owner"));
  });

  it("without ROLL_SECRET_KEY, says so and can't connect", async () => {
    show(spied({ ...DISCONNECTED, app: { ...DISCONNECTED.app, secret_key: false } }));
    expect(await screen.findByText("needs ROLL_SECRET_KEY")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Connect Linear" })).toBeDisabled();
    expect(screen.getByLabelText("Linear API key")).toBeDisabled();
  });

  it("says why a key didn't connect", async () => {
    show(spied(DISCONNECTED));
    await userEvent.type(await screen.findByLabelText("Linear API key"), "nope{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("Linear refused the key");
  });

  it("connected: the workspace, the webhook, team repos and the deliveries, newest first", async () => {
    const linear = spied(CONNECTED);
    show(linear);
    expect(await screen.findByRole("link", { name: /Acme/ })).toHaveAttribute("href", "https://linear.app/acme");
    expect(screen.getByText("roll app (agent) · as roll")).toBeInTheDocument();
    expect(screen.getByText("Receiving")).toBeInTheDocument();
    expect(screen.getByText("https://roll.example/v1/code/linear/webhook")).toBeInTheDocument();
    // roll's app signs with the server's secret: nothing to paste.
    expect(screen.queryByLabelText("Webhook signing secret")).toBeNull();

    const rows = within(screen.getByRole("list", { name: "Recent deliveries" })).getAllByRole("listitem");
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("ENG-2: Ada assigned roll");
    expect(rows[0]).toHaveTextContent("AgentSessionEvent.created");
    expect(rows[0]).toHaveTextContent("recorded");
    expect(rows[1]).toHaveTextContent("ENG-1 moved to In Progress");

    // Teams come from Linear; a repo for one is picked from the owner's repos and saved with the rest.
    const web = await screen.findByRole("button", { name: "WEB repo" });
    expect(screen.getByRole("button", { name: "ENG repo" })).toHaveTextContent("roll");
    macClick(web);
    macClick(await within(await screen.findByRole("listbox", { name: "WEB repo" })).findByRole("option", { name: /web/ }));
    await waitFor(() => expect(linear.putSettings).toHaveBeenLastCalledWith("acme", { repos: { ENG: "roll", WEB: "web" } }));
    macClick(screen.getByRole("button", { name: "Default repo" }));
    macClick(await within(await screen.findByRole("listbox", { name: "Default repo" })).findByRole("option", { name: /roll/ }));
    await waitFor(() => expect(linear.putSettings).toHaveBeenLastCalledWith("acme", { repos: { ENG: "roll", WEB: "web" }, default_repo: "roll" }));
    // Clearing a team's repo unmaps it.
    macClick(screen.getByRole("button", { name: "Clear eng repo" }));
    await waitFor(() => expect(linear.putSettings).toHaveBeenLastCalledWith("acme", { repos: { WEB: "web" }, default_repo: "roll" }));
  });

  it("disconnects only after it's asked again", async () => {
    const linear = spied(CONNECTED);
    show(linear);
    await userEvent.click(await screen.findByRole("button", { name: "Disconnect" }));
    expect(linear.disconnect).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Keep it" }));
    await userEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    await userEvent.click(screen.getByRole("button", { name: "Disconnect Acme" }));
    await waitFor(() => expect(linear.disconnect).toHaveBeenCalledWith("acme"));
    expect(await screen.findByRole("button", { name: "Connect Linear" })).toBeInTheDocument();
  });

  it("a hand-made webhook takes its signing secret, and says why deliveries fail", async () => {
    const base = CONNECTED.connection!;
    const linear = spied({
      ...CONNECTED,
      connection: {
        ...base,
        kind: "api_key",
        agent: false,
        webhook: { ...base.webhook, mode: "manual", url: "https://roll.example/v1/code/linear/webhook/lwh_1", status: "failing", secret_hint: "beef", rejected_at: Date.now(), rejected_why: "a bad signature" },
      },
    });
    show(linear);
    expect(await screen.findByText("Failing")).toBeInTheDocument();
    expect(screen.getByText(/a bad signature/)).toBeInTheDocument();
    expect(screen.getByText("…beef")).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText("Webhook signing secret"), "lin_wh_0123456789{Enter}");
    await waitFor(() => expect(linear.putWebhookSecret).toHaveBeenCalledWith("acme", "lin_wh_0123456789"));
    expect(await screen.findByText("…6789")).toBeInTheDocument();
  });

  it("asks to connect again when Linear stopped taking the token", async () => {
    const linear = spied({ ...CONNECTED, connection: { ...CONNECTED.connection!, status: "reauth", error: "Linear revoked roll's token" } });
    linear.connectOAuth.mockResolvedValue("#agents/settings/owner");
    show(linear);
    expect(await screen.findByText("Linear revoked roll's token")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Reconnect" }));
    await waitFor(() => expect(linear.connectOAuth).toHaveBeenCalledWith("acme", "/dashboard/agents#agents/settings/owner"));
  });

  it("the demo owner has a connected workspace with deliveries", async () => {
    const { DemoOwner } = await import("@/lib/agents-demo");
    show(new DemoOwner().services().linear);
    expect(await screen.findByRole("link", { name: /Acme/ })).toHaveAttribute("href", "https://linear.app/acme");
    expect(screen.getByText("Receiving")).toBeInTheDocument();
    expect(within(screen.getByRole("list", { name: "Recent deliveries" })).getAllByRole("listitem").length).toBeGreaterThan(1);
  });

  it("back from OAuth: says how it went once and takes it off the URL", async () => {
    window.history.replaceState(null, "", "/dashboard/agents#agents/settings/owner?linear=error&reason=the%20workspace%20is%20another%20owner%27s");
    show(spied(DISCONNECTED));
    expect(await screen.findByRole("status")).toHaveTextContent("Linear didn't connect: the workspace is another owner's");
    expect(window.location.hash).toBe("#agents/settings/owner");
    await userEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(/didn't connect/)).toBeNull();
  });
});
