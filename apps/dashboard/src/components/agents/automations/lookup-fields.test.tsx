/**
 * Integration fields in an automation's forms are pickers (`format: "lookup"` in the server's
 * catalog): Linear's create-issue step picks a team, then that team's states; a trigger's lists
 * are chips; values saved before pickers (a team's key) show by name; threads and machines are
 * roll's own lists.
 */
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Mirrored } from "@/lib/agents-api";
import type { Schema } from "@/lib/automations-ui";
import chatopsCatalog from "@/lib/chatops-catalog.json";
import linearCatalog from "@/lib/linear-catalog.json";
import { memoryLookup } from "@/lib/lookup-api";
import { DEMO_LOOKUPS } from "@/lib/lookup-demo";
import { mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { macClick } from "@/test/mac-click";
import { ConfigForm, type FormContext } from "./config-form";

beforeEach(() => localStorage.clear());

const step = (kind: string) => linearCatalog.steps.find((s) => s.kind === kind)!.config as unknown as Schema;
const ctx: FormContext = { variables: [{ path: "event.data.issue.team.id", group: "Event" }], problems: {}, at: "steps.0.config", secrets: [] };

function Form({ schema, initial, onValue }: { schema: Schema; initial: Record<string, unknown>; onValue: (v: Record<string, unknown>) => void }) {
  const [value, setValue] = useState(initial);
  return (
    <ConfigForm
      schema={schema}
      value={value}
      onChange={(v) => {
        setValue(v);
        onValue(v);
      }}
      ctx={ctx}
    />
  );
}

function show(schema: Schema, initial: Record<string, unknown> = {}, entities: Mirrored[] = []) {
  const onValue = vi.fn();
  const lookup = memoryLookup(DEMO_LOOKUPS);
  const h = agentsHarness({ entities, ctx: { services: fakeServices({ lookup }) } });
  renderAgents(<Form schema={schema} initial={initial} onValue={onValue} />, h);
  return { onValue, lookup };
}

// Several pickers open and load in each test: allow for a busy machine.
describe("automation fields that name a thing", { timeout: 15_000 }, () => {
  it("create an issue: a team from Linear, then that team's states", async () => {
    const { onValue, lookup } = show(step("linear.create_issue"));
    macClick(screen.getByRole("button", { name: "Team" }));
    macClick(await within(await screen.findByRole("listbox", { name: "Team" })).findByRole("option", { name: /Web/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ team: "team_web" }));
    macClick(screen.getByRole("button", { name: "State" }));
    const states = await screen.findByRole("listbox", { name: "State" });
    // The kinds of state first, then the team's own.
    expect(within(states).getByText("Any team")).toBeInTheDocument();
    await waitFor(() => expect(within(states).getByRole("option", { name: /In Review/ })).toBeInTheDocument());
    expect(lookup.calls.at(-1)).toContain("parent=team_web");
    macClick(within(states).getByRole("option", { name: /In Review/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ team: "team_web", state: "st_web_in_review" }));
    // Priority is Linear's fixed list.
    macClick(screen.getByRole("button", { name: "Priority" }));
    macClick(within(await screen.findByRole("listbox", { name: "Priority" })).getByRole("option", { name: /Urgent/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ priority: "1" }));
  });

  it("an issue step takes a variable where the issue comes from the event", async () => {
    const { onValue } = show(step("linear.comment"));
    macClick(screen.getByRole("button", { name: "Issue" }));
    macClick(await screen.findByRole("option", { name: "Use a variable" }));
    macClick(await screen.findByRole("option", { name: /issue\.team\.id/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ issue: "{{ event.data.issue.team.id }}" }));
  });

  it("a Linear trigger's lists are chips; a key saved before pickers shows its team", async () => {
    const trigger = linearCatalog.triggers[0].config as unknown as Schema;
    const { onValue } = show(trigger, { teams: ["ENG"], priorities: [1] });
    await waitFor(() => expect(screen.getByText("Engineering")).toBeInTheDocument());
    expect(screen.getByText("Urgent")).toBeInTheDocument();
    macClick(screen.getByRole("button", { name: "Add to Teams" }));
    macClick(await within(await screen.findByRole("listbox", { name: "Teams" })).findByRole("option", { name: /Operations/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ teams: ["ENG", "team_ops"] }));
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    macClick(screen.getByRole("button", { name: "Assignees" }));
    const people = await screen.findByRole("listbox", { name: "Assignees" });
    expect(within(people).getAllByRole("option")[0]).toHaveTextContent("roll");
    await waitFor(() => expect(within(people).getByRole("option", { name: /Grace Hopper/ })).toBeInTheDocument());
    expect(within(people).getAllByRole("option").filter((o) => /^roll/.test(o.textContent ?? ""))).toHaveLength(1);
  });

  it("a thread is one of the owner's threads, by title", async () => {
    const t = thread({ id: "thr_7", title: "Fix the SSO loop" });
    const { onValue } = show(step("linear.agent_session"), {}, [mirrored("project", project(), 2), mirrored("thread", t, 3)]);
    macClick(screen.getByRole("button", { name: "Thread" }));
    macClick(within(await screen.findByRole("listbox", { name: "Thread" })).getByRole("option", { name: /Fix the SSO loop/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ thread: "thr_7" }));
    expect(screen.getByRole("button", { name: "Thread" })).toHaveTextContent("Fix the SSO loop");
  });

  it("a GitHub label step: a connected repo, then its issue, labels made on the spot", async () => {
    const schema = chatopsCatalog.steps.find((s) => s.kind === "github.label")!.config as unknown as Schema;
    const { onValue, lookup } = show(schema, { number: 128 });
    // A number saved before pickers shows its title (no repo yet: every connected repo's).
    await waitFor(() => expect(screen.getByRole("button", { name: "Issue or pull request" })).toHaveTextContent("Login 500s on Safari"));
    macClick(screen.getByRole("button", { name: "Repo" }));
    macClick(await within(await screen.findByRole("listbox", { name: "Repo" })).findByRole("option", { name: /acme\/web/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ repo: "acme/web" }));
    macClick(screen.getByRole("button", { name: "Issue or pull request" }));
    const issues = await screen.findByRole("listbox", { name: "Issue or pull request" });
    macClick(await within(issues).findByRole("option", { name: /Dark mode for settings/ }));
    expect(lookup.calls.some((c) => c.includes("/github/issues") && c.includes("parent=acme%2Fweb"))).toBe(true);
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ repo: "acme/web", number: "42" }));
    // Labels to add: the repo's, and a new one made in that repo.
    macClick(screen.getByRole("button", { name: "Add labels" }));
    const labels = await screen.findByRole("listbox", { name: "Add labels" });
    const enhancement = await within(labels).findByRole("option", { name: /enhancement/ });
    expect(within(labels).queryByRole("option", { name: /^roll/ })).toBeNull();
    macClick(enhancement);
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ add: ["enhancement"] }));
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    macClick(screen.getByRole("button", { name: "Add to Add labels" }));
    fireEvent.change(await screen.findByRole("combobox", { name: "Search add labels" }), { target: { value: "needs qa" } });
    macClick(await screen.findByRole("option", { name: /Create new “needs qa”/ }));
    await waitFor(() => expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ add: ["enhancement", lookup.made[0].id] })));
    expect(lookup.made[0]).toMatchObject({ label: "needs qa" });
    expect(await lookup.list("acme", "github", "labels", { parent: "acme/web", q: "needs" })).toMatchObject({ items: [{ label: "needs qa" }] });
  });
});

describe("Slack and Discord fields name their things", () => {
  const chat = (kind: string) => chatopsCatalog.steps.find((s) => s.kind === kind)!.config as unknown as Schema;

  it("a Slack DM picks a person of the workspace; a notice goes to a channel or a person", async () => {
    const { onValue, lookup } = show(chat("slack.dm"));
    macClick(screen.getByRole("button", { name: "Workspace" }));
    macClick(await within(await screen.findByRole("listbox", { name: "Workspace" })).findByRole("option", { name: /Acme/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ team: "T04ACME" }));
    macClick(screen.getByRole("button", { name: "Person" }));
    macClick(await within(await screen.findByRole("listbox", { name: "Person" })).findByRole("option", { name: /Grace Hopper/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ team: "T04ACME", user: "U03GRACE" }));
    expect(lookup.calls.at(-1)).toContain("/lookup/slack/users?parent=T04ACME");
    expect(screen.getByRole("button", { name: "Person" })).toHaveTextContent("Grace Hopper");
  });

  it("a Slack notice's To lists channels and people under their headings", async () => {
    const { onValue } = show(chat("slack.notify"));
    macClick(screen.getByRole("button", { name: "To" }));
    const to = await screen.findByRole("listbox", { name: "To" });
    await waitFor(() => expect(within(to).getByText("People")).toBeInTheDocument());
    expect(within(to).getByText("Channels")).toBeInTheDocument();
    macClick(within(to).getByRole("option", { name: /deploys/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ to: "C02DEPLOYS" }));
  });

  it("a Slack post's channel takes a variable, and a channel saved by name shows as itself", async () => {
    const { onValue } = show(chat("slack.post"), { channel: "incidents" });
    await waitFor(() => expect(screen.getByRole("button", { name: "Channel" })).toHaveTextContent("incidents"));
    macClick(screen.getByRole("button", { name: "Channel" }));
    macClick(await screen.findByRole("option", { name: "Use a variable" }));
    macClick(await screen.findByRole("option", { name: /issue\.team\.id/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ channel: "{{ event.data.issue.team.id }}" }));
  });

  it("a Discord trigger's channels are chips from the picked server", async () => {
    const trigger = chatopsCatalog.triggers.find((t) => t.source === "discord")!.config as unknown as Schema;
    const { onValue, lookup } = show(trigger, { guild: "1100000000000000001" });
    await waitFor(() => expect(screen.getByRole("button", { name: "Server" })).toHaveTextContent("Acme Discord"));
    macClick(screen.getByRole("button", { name: "Channels" }));
    const channels = await screen.findByRole("listbox", { name: "Channels" });
    expect(await within(channels).findByText("Work")).toBeInTheDocument();
    macClick(within(channels).getByRole("option", { name: /deploys/ }));
    expect(onValue).toHaveBeenLastCalledWith(expect.objectContaining({ guild: "1100000000000000001", channels: ["1100000000000000011"] }));
    expect(lookup.calls.some((c) => c.includes("/lookup/discord/channels") && c.includes("parent=1100000000000000001"))).toBe(true);
    // The list stays open for more; Escape closes it.
    expect(screen.getByRole("listbox", { name: "Channels" })).toBeInTheDocument();
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("listbox", { name: "Channels" })).toBeNull());
  });
});
