import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { resetAgentsSurface } from "@/lib/agents-surface";
import { NO_INTEGRATIONS, type ChatInstall, type Integrations, type IntegrationsApi } from "@/lib/integrations-api";
import { macClick } from "@/test/mac-click";
import { IntegrationsSection, resetIntegrationsLink } from "./integrations";

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
  it("connects by asking for an install link and going there", async () => {
    const api = fakeApi({ slack: { configured: true, installs: [] }, sealing: true });
    const { navigate } = show(api);
    await userEvent.click(await within(slack()).findByRole("button", { name: "Connect" }));
    expect(api.install).toHaveBeenCalledWith("acme", "slack");
    await waitFor(() => expect(navigate).toHaveBeenCalledWith("https://slack.example/install?state=s"));
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
});
