import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { NO_SETTINGS, type Command, type Project } from "@/lib/agents-api";
import { MachineIcon, ProjectIcon } from "./identity";
import { ProjectNode, ThreadNode } from "./nodes";
import { ProjectSettingsForm } from "./project-settings";
import { SettingsPanel } from "./settings-panel";
import { machine, machineConfig, mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";

const LOGO = { path: "public/favicon.svg", url: "data:image/svg+xml;base64,PHN2Zy8+" };
const updates = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.map((c) => c[2]).filter((c): c is Extract<Command, { type: "project.update" }> => c.type === "project.update");

describe("ProjectIcon", () => {
  it("shows the project's logo, and its monogram on its colour when the logo won't load", () => {
    const { container } = render(<ProjectIcon project={project({ logo: LOGO, settings: { ...NO_SETTINGS, color: "violet" } })} />);
    const img = container.querySelector("img")!;
    expect(img).toHaveAttribute("src", LOGO.url);
    fireEvent.error(img);
    expect(container.querySelector("img")).toBeNull();
    const tile = container.querySelector("[data-identity=monogram]")!;
    expect(tile).toHaveAttribute("data-color", "violet");
    expect(tile.textContent).toBe("RL");
  });

  it("shows an emoji or the letters the owner picked", () => {
    const { container, rerender } = render(<ProjectIcon project={project({ logo: LOGO, settings: { ...NO_SETTINGS, icon: { kind: "emoji", emoji: "🦀" } } })} />);
    expect(container.querySelector("[data-identity=emoji]")?.textContent).toBe("🦀");
    rerender(<ProjectIcon project={project({ logo: LOGO, settings: { ...NO_SETTINGS, icon: { kind: "monogram", text: "rx" } } })} />);
    expect(container.querySelector("[data-identity=monogram]")?.textContent).toBe("RX");
  });
});

describe("MachineIcon", () => {
  it("shows the machine's kind in its colour", () => {
    const { container } = render(<MachineIcon machine={machine({ appearance: { color: "teal", kind: "server" } })} tile />);
    const el = container.querySelector("[data-identity=machine]")!;
    expect(el).toHaveAttribute("data-color", "teal");
    expect(el.querySelector("svg.lucide-server")).not.toBeNull();
  });
});

describe("cards carry their identity", () => {
  it("a project's card is tinted with its colour and leads with its icon; its threads carry its stripe", () => {
    const p = project({ logo: LOGO, settings: { ...NO_SETTINGS, color: "rose" } });
    const h = agentsHarness({ entities: [mirrored("project", p, 2), mirrored("thread", thread(), 3)] });
    const { container, unmount } = renderAgents(<ProjectNode project={p} machine={machine()} />, h);
    expect(container.querySelector("[data-drag]")).toHaveAttribute("data-tint", "rose");
    expect(container.querySelector("img")).toHaveAttribute("src", LOGO.url);
    unmount();
    const card = renderAgents(<ThreadNode thread={thread()} machine={machine()} />, h).container;
    expect(card.querySelector("[data-drag]")).toHaveAttribute("data-tint", "rose");
    expect(card.querySelector("[data-stripe]")).not.toBeNull();
  });
});

describe("project settings: colour and icon", () => {
  const withGit = (over: Partial<Project> = {}) => project({ git: { branch: "main", head: "f35f34cf", remote: null, dirty: false }, ...over });

  it("picks a colour and an emoji and saves them with the settings", async () => {
    const h = agentsHarness();
    renderAgents(<ProjectSettingsForm project={withGit()} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("radio", { name: "Rose" }));
    await userEvent.click(screen.getByRole("combobox", { name: "How the icon is made" }));
    await userEvent.click(await screen.findByRole("option", { name: "Emoji" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Emoji" }), "🦀");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(updates(h)).toHaveLength(1));
    expect(updates(h)[0].settings).toMatchObject({ color: "rose", icon: { kind: "emoji", emoji: "🦀" } });
  });

  it("says which logo it found, refuses a monogram the daemon would, and Auto clears the colour", async () => {
    const h = agentsHarness();
    renderAgents(<ProjectSettingsForm project={withGit({ logo: LOGO, settings: { ...NO_SETTINGS, color: "teal" } })} machine={machine()} />, h);
    expect(screen.getByText("public/favicon.svg")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("combobox", { name: "How the icon is made" }));
    await userEvent.click(await screen.findByRole("option", { name: "Monogram" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Monogram letters" }), "ROLL");
    expect(screen.getByRole("alert")).toHaveTextContent("A monogram is 1 to 3 letters or digits");
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    await userEvent.clear(screen.getByRole("textbox", { name: "Monogram letters" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Monogram letters" }), "RO");
    await userEvent.click(screen.getByRole("radio", { name: /^Auto/ }));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(updates(h)).toHaveLength(1));
    const sent = updates(h)[0].settings!;
    expect(sent.color).toBeUndefined();
    expect(sent.icon).toEqual({ kind: "monogram", text: "RO" });
  });
});

describe("machine settings: colour and kind", () => {
  it("sends the colour and kind to the relay, and Auto clears them; works while the machine is offline", async () => {
    const services = fakeServices();
    const m = machine({ online: false, appearance: { color: "pink" }, info: { ...machine().info!, settings: machineConfig() } });
    const h = agentsHarness({ machines: [m], ctx: { services } });
    renderAgents(<SettingsPanel target="m:mch_1" onClose={vi.fn()} />, h);
    expect(screen.getByRole("radio", { name: "Pink" })).toHaveAttribute("aria-checked", "true");
    await userEvent.click(screen.getByRole("radio", { name: "Teal" }));
    await waitFor(() => expect(services.machines.style).toHaveBeenCalledWith("acme", "mch_1", { color: "teal" }));
    await userEvent.click(screen.getByRole("radio", { name: "Server" }));
    await waitFor(() => expect(services.machines.style).toHaveBeenCalledWith("acme", "mch_1", { kind: "server" }));
    await userEvent.click(within(screen.getByRole("radiogroup", { name: "Colour" })).getByRole("radio", { name: /^Auto/ }));
    await waitFor(() => expect(services.machines.style).toHaveBeenCalledWith("acme", "mch_1", { color: null }));
  });
});
