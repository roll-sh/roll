import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { NO_INTEGRATIONS, type ChatInstall, type Integrations, type IntegrationsApi } from "@/lib/integrations-api";
import { memoryLookup } from "@/lib/lookup-api";
import { DEMO_LOOKUPS } from "@/lib/lookup-demo";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { macClick } from "@/test/mac-click";
import { IntegrationsSection, openIntegrations, resetIntegrationsLink } from "./integrations";
import { SettingsPanel } from "./settings-panel";

const ACME: ChatInstall = {
  id: "chi_1",
  team_id: "T04ACME",
  team_name: "Acme",
  bot_user_id: "U07BOT",
  installer: "U02ADA",
  members: ["U03GRACE"],
  anyone: false,
  created_at: 1_762_000_000_000,
};

function fakeApi(data: Partial<Integrations> = {}) {
  const state: Integrations = { ...NO_INTEGRATIONS, ...data };
  return {
    list: vi.fn(async () => state),
    install: vi.fn(async (_o: string, p: string) => ({ url: `https://${p}.example/install?state=s` })),
    update: vi.fn(async (_o: string, _p: string, id: string, patch: { members?: string[]; anyone?: boolean }) => {
      const i = [...state.slack.installs, ...state.discord.installs].find((x) => x.id === id)!;
      return { ...i, ...patch };
    }),
    remove: vi.fn(async () => {}),
  } satisfies IntegrationsApi;
}

function show(api: IntegrationsApi, navigate = vi.fn()) {
  render(<IntegrationsSection owner="acme" api={api} navigate={navigate} />);
  return { navigate };
}

const row = (id: string) => document.querySelector<HTMLElement>(`[data-setting="${id}"]`)!;
const slack = () => row("integrations.slack");

beforeEach(() => {
  resetIntegrationsLink();
  resetAgentsSurface();
});

describe("the Slack card", () => {
  it("says the server isn't configured, naming the variables, and offers no Connect", async () => {
    show(fakeApi());
    await waitFor(() => expect(within(slack()).getByText(/Not configured on this server/)).toBeInTheDocument());
    for (const v of ["ROLL_SLACK_CLIENT_ID", "ROLL_SLACK_CLIENT_SECRET", "ROLL_SLACK_SIGNING_SECRET"]) expect(within(slack()).getByText(v)).toBeInTheDocument();
    expect(within(slack()).queryByRole("button", { name: /Connect/ })).toBeNull();
    // Discord says the same with its own.
    expect(within(row("integrations.discord")).getByText("ROLL_DISCORD_BOT_TOKEN")).toBeInTheDocument();
  });

  it("needs ROLL_SECRET_KEY before it can connect", async () => {
    show(fakeApi({ slack: { configured: true, installs: [] }, sealing: false }));
    await waitFor(() => expect(within(slack()).getByText("ROLL_SECRET_KEY")).toBeInTheDocument());
    expect(within(slack()).queryByRole("button", { name: /Connect/ })).toBeNull();
  });

  it("connects by asking for an install link and going there", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [] }, sealing: true });
    const { navigate } = show(api);
    await userEvent.click(await within(slack()).findByRole("button", { name: "Connect" }));
    expect(api.install).toHaveBeenCalledWith("acme", "slack");
    await waitFor(() => expect(navigate).toHaveBeenCalledWith("https://slack.example/install?state=s"));
  });

  it("shows why connecting failed", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [] }, sealing: true });
    api.install.mockRejectedValueOnce(new Error("Slack isn't configured on this server (ROLL_SLACK_*)"));
    const { navigate } = show(api);
    await userEvent.click(await within(slack()).findByRole("button", { name: "Connect" }));
    expect(await within(slack()).findByRole("alert")).toHaveTextContent("Slack isn't configured");
    expect(navigate).not.toHaveBeenCalled();
  });

  it("lists a connected workspace and edits who can drive roll from it", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [ACME] }, sealing: true });
    show(api);
    const acme = await within(slack()).findByRole("listitem", { name: "Acme" });
    expect(within(slack()).getByRole("button", { name: "Add another" })).toBeInTheDocument();
    expect(within(acme).getByText("T04ACME")).toBeInTheDocument();
    expect(within(acme).getByText("U02ADA")).toBeInTheDocument();
    expect(within(acme).getByText("U03GRACE")).toBeInTheDocument();

    // Outside the agents surface there's nothing to look people up in: an id is typed.
    macClick(within(acme).getByRole("button", { name: "Add to Members" }));
    const search = await screen.findByRole("combobox", { name: "Search members" });
    fireEvent.change(search, { target: { value: "U04LIN" } });
    macClick(await screen.findByRole("option", { name: /Use U04LIN/ }));
    await waitFor(() => expect(api.update).toHaveBeenLastCalledWith("acme", "slack", "chi_1", { members: ["U03GRACE", "U04LIN"] }));
    expect(await within(acme).findByText("U04LIN")).toBeInTheDocument();

    // A bad id is refused before it's sent.
    fireEvent.change(screen.getByRole("combobox", { name: "Search members" }), { target: { value: "<@U1>" } });
    macClick(await screen.findByRole("option", { name: /Use <@U1>/ }));
    expect(within(acme).getByRole("alert")).toHaveTextContent("isn't a Slack user id");
    expect(api.update).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(screen.getByRole("combobox", { name: "Search members" }), { key: "Escape" });

    // Remove one.
    macClick(within(acme).getByRole("button", { name: "Remove U03GRACE" }));
    await waitFor(() => expect(api.update).toHaveBeenLastCalledWith("acme", "slack", "chi_1", { members: ["U04LIN"] }));
    await waitFor(() => expect(within(acme).queryByText("U03GRACE")).toBeNull());
  });

  it("lets anyone in the workspace drive roll, with a warning", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [ACME] }, sealing: true });
    show(api);
    const acme = await within(slack()).findByRole("listitem", { name: "Acme" });
    const anyone = within(acme).getByRole("switch", { name: "Anyone in the workspace" });
    expect(anyone).not.toBeChecked();
    expect(within(acme).getByText(/can start agents with your access/)).toBeInTheDocument();
    await userEvent.click(anyone);
    await waitFor(() => expect(api.update).toHaveBeenCalledWith("acme", "slack", "chi_1", { anyone: true }));
    await waitFor(() => expect(within(acme).getByRole("switch", { name: "Anyone in the workspace" })).toBeChecked());
  });

  it("disconnects only after a confirm", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [ACME] }, sealing: true });
    show(api);
    const acme = await within(slack()).findByRole("listitem", { name: "Acme" });
    await userEvent.click(within(acme).getByRole("button", { name: "Disconnect" }));
    expect(within(acme).getByText(/stays installed in Slack/)).toBeInTheDocument();
    expect(api.remove).not.toHaveBeenCalled();
    // Cancel keeps it.
    await userEvent.click(within(acme).getByRole("button", { name: "Cancel" }));
    expect(within(acme).queryByText(/stays installed in Slack/)).toBeNull();
    await userEvent.click(within(acme).getByRole("button", { name: "Disconnect" }));
    await userEvent.click(within(acme).getByRole("button", { name: "Disconnect" }));
    await waitFor(() => expect(api.remove).toHaveBeenCalledWith("acme", "slack", "chi_1"));
    await waitFor(() => expect(within(slack()).queryByRole("listitem", { name: "Acme" })).toBeNull());
    expect(within(slack()).getByRole("button", { name: "Connect" })).toBeInTheDocument();
  });
});

describe("the GitHub card", () => {
  it("lists the owner's connected repos with their mirror state and links the App", async () => {
    show(
      fakeApi({
        github: {
          configured: true,
          app_slug: "roll-code",
          install_url: "https://github.com/apps/roll-code/installations/new",
          repos: [
            { repo: "acme/roll", github: "acme-inc/roll", url: "https://github.com/acme-inc/roll", connected: true, mirror: "ok" },
            { repo: "acme/web", github: "acme-inc/web", url: null, connected: true, mirror: "error" },
            { repo: "acme/old", github: "acme-inc/old", url: null, connected: false, mirror: "ok" },
          ],
        },
      }),
    );
    const gh = row("integrations.github");
    const repos = await within(gh).findByRole("list", { name: "Connected repos" });
    const items = within(repos).getAllByRole("listitem");
    expect(items.map((i) => i.textContent)).toEqual(["acme/roll→acme-inc/rollMirrored", "acme/web→acme-inc/webMirror failing", "acme/old→acme-inc/oldDisconnected"]);
    expect(within(repos).getByRole("link", { name: "acme-inc/roll" })).toHaveAttribute("href", "https://github.com/acme-inc/roll");
    expect(within(gh).getByRole("link", { name: /Install the GitHub App/ })).toHaveAttribute("href", "https://github.com/apps/roll-code/installations/new");
    expect(within(gh).getByText(/when you create it/)).toBeInTheDocument();
  });

  it("says when the App isn't configured", async () => {
    show(fakeApi());
    const gh = row("integrations.github");
    await waitFor(() => expect(within(gh).getByText(/Not configured on this server/)).toBeInTheDocument());
    expect(within(gh).getByText("ROLL_GITHUB_APP_ID")).toBeInTheDocument();
    expect(within(gh).queryByRole("link", { name: /Install the GitHub App/ })).toBeNull();
  });
});

describe("the link back from a platform", () => {
  it("opens the owner's settings and shows the connection once", async () => {
    act(() => openIntegrations({ platform: "slack", connected: "Acme", error: null }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "owner" });
    const scrolled = vi.spyOn(Element.prototype, "scrollIntoView");
    show(fakeApi({ slack: { configured: true, installs: [ACME] }, sealing: true }));
    expect(screen.getByRole("status")).toHaveTextContent("Connected Acme to Slack.");
    expect(scrolled).toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("status")).toBeNull();
    scrolled.mockRestore();
  });

  it("shows an error calmly", () => {
    openIntegrations({ platform: "discord", connected: null, error: "The install link expired. Connect again." });
    show(fakeApi());
    expect(screen.getByRole("alert")).toHaveTextContent("Discord wasn't connected: The install link expired. Connect again.");
  });

  it("is forgotten once shown", () => {
    openIntegrations({ platform: "slack", connected: "Acme", error: null });
    const first = render(<IntegrationsSection owner="acme" api={fakeApi()} navigate={vi.fn()} />);
    first.unmount();
    show(fakeApi());
    expect(screen.queryByRole("status")).toBeNull();
  });
});

describe("in the owner's settings", () => {
  it("has an Integrations section with the three cards, found by search", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [ACME] }, sealing: true });
    const h = agentsHarness({ ctx: { services: fakeServices({ integrations: api }) } });
    renderAgents(<SettingsPanel target="owner" onClose={vi.fn()} />, h);
    const section = screen.getByRole("region", { name: "Integrations" });
    expect(await within(section).findByRole("listitem", { name: "Acme" })).toBeInTheDocument();
    expect(api.list).toHaveBeenCalledWith("acme");
    expect(within(section).getByRole("heading", { name: "GitHub" })).toBeInTheDocument();
    await userEvent.type(screen.getByRole("searchbox", { name: "Search settings" }), "slack");
    expect(screen.getByRole("heading", { name: "Slack" })).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "GitHub" })).toBeNull();
    expect(screen.queryByRole("region", { name: "Keyboard" })).toBeNull();
  });
});

describe("members are picked from the workspace", () => {
  it("shows them by name and adds people from the workspace's list (Safari's click)", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [ACME] }, sealing: true });
    const lookup = memoryLookup(DEMO_LOOKUPS);
    renderAgents(<IntegrationsSection owner="acme" api={api} navigate={vi.fn()} />, agentsHarness({ ctx: { services: fakeServices({ lookup }) } }));
    const acme = await within(slack()).findByRole("listitem", { name: "Acme" });
    // A stored id shows its person.
    expect(await within(acme).findByText("Grace Hopper")).toBeInTheDocument();
    macClick(within(acme).getByRole("button", { name: "Add to Members" }));
    const people = await screen.findByRole("listbox", { name: "Members" });
    macClick(await within(people).findByRole("option", { name: /Alan Turing/ }));
    await waitFor(() => expect(api.update).toHaveBeenLastCalledWith("acme", "slack", "chi_1", { members: ["U03GRACE", "U04ALAN"] }));
    expect(lookup.calls.some((c) => c.startsWith("/v1/code/agents/acme/lookup/slack/users") && c.includes("parent=T04ACME"))).toBe(true);
    // Picking several keeps the list open; a press outside closes it.
    expect(screen.getByRole("listbox", { name: "Members" })).toBeInTheDocument();
    macClick(document.body);
    await waitFor(() => expect(screen.queryByRole("listbox", { name: "Members" })).toBeNull());
    expect(await within(acme).findByText("Alan Turing")).toBeInTheDocument();
  });

  it("a Discord server's members come from its own list", async () => {
    const server: ChatInstall = { ...ACME, id: "chi_2", team_id: "1100000000000000001", team_name: "Acme Discord", installer: "", members: [] };
    const api = fakeApi({ discord: { configured: true, installs: [server] } });
    const lookup = memoryLookup(DEMO_LOOKUPS);
    renderAgents(<IntegrationsSection owner="acme" api={api} navigate={vi.fn()} />, agentsHarness({ ctx: { services: fakeServices({ lookup }) } }));
    const item = await within(row("integrations.discord")).findByRole("listitem", { name: "Acme Discord" });
    macClick(within(item).getByRole("button", { name: "Members" }));
    macClick(await within(await screen.findByRole("listbox", { name: "Members" })).findByRole("option", { name: /Grace/ }));
    await waitFor(() => expect(api.update).toHaveBeenLastCalledWith("acme", "discord", "chi_2", { members: ["1100000000000000043"] }));
    expect(lookup.calls.at(-1)).toContain("/lookup/discord/members?parent=1100000000000000001");
  });
});
