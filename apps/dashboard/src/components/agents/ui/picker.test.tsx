/**
 * The shared picker (`ui/picker.tsx`) and the lookup picker on it: rows by name with search and
 * keyboard, several picked as chips, stored values (ids, legacy keys, variables) shown by name,
 * typed and variable fallbacks, making a label, a source's problem said quietly, recents first,
 * and every path under a macOS click (no focus on the row, no blur-closing).
 */
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { memoryLookup, type LookupApi } from "@/lib/lookup-api";
import { DEMO_LOOKUPS } from "@/lib/lookup-demo";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { macClick } from "@/test/mac-click";
import { LookupPicker } from "../lookup-picker";
import { Picker, type PickerOption, type PickerProps } from "./picker";

beforeEach(() => localStorage.clear());

const TEAMS: PickerOption[] = [
  { id: "team_eng", key: "ENG", label: "Engineering", hint: "ENG", color: "#5e6ad2" },
  { id: "team_web", key: "WEB", label: "Web", hint: "WEB" },
  { id: "team_ops", key: "OPS", label: "Operations", hint: "OPS" },
];

/** A picker that keeps its own value, reporting each change. */
type Shared = Omit<PickerProps, "value" | "onChange" | "multiple" | "clearable"> & { clearable?: boolean };

function Held(props: Shared & { initial: string | null | string[]; onValue?: (v: unknown) => void }) {
  const { initial, onValue, clearable, ...rest } = props;
  const [value, setValue] = useState<string | null | string[]>(initial);
  const set = (v: string | null | string[]) => {
    setValue(v);
    onValue?.(v);
  };
  return Array.isArray(value) ? <Picker {...rest} multiple value={value} onChange={set} /> : <Picker {...rest} clearable={clearable} value={value} onChange={set} />;
}

const listbox = () => screen.findByRole("listbox", { name: "Team" });

/** Type into a search field at once (userEvent types a key at a time: slow in jsdom). */
const typeIn = (el: HTMLElement, text: string) => fireEvent.change(el, { target: { value: text } });

describe("Picker", () => {
  it("shows the picked row by name, searches, and picks with a macOS click", async () => {
    const onValue = vi.fn();
    render(<Held label="Team" options={TEAMS} initial="team_web" onValue={onValue} />);
    const trigger = screen.getByRole("button", { name: "Team" });
    expect(trigger).toHaveTextContent("Web");
    macClick(trigger);
    const list = await listbox();
    expect(within(list).getAllByRole("option").map((o) => o.textContent)).toEqual(["EngineeringENG", "WebWEB", "OperationsOPS"]);
    typeIn(screen.getByRole("combobox", { name: "Search team" }), "op");
    await waitFor(() => expect(within(list).getAllByRole("option")).toHaveLength(1));
    macClick(within(list).getByRole("option", { name: /Operations/ }));
    expect(onValue).toHaveBeenLastCalledWith("team_ops");
    await waitFor(() => expect(screen.queryByRole("listbox", { name: "Team" })).toBeNull());
    expect(screen.getByRole("button", { name: "Team" })).toHaveTextContent("Operations");
  });

  it("picks with the keyboard and closes on Escape, never on a blur", async () => {
    const onValue = vi.fn();
    render(<Held label="Team" options={TEAMS} initial={null} onValue={onValue} />);
    macClick(screen.getByRole("button", { name: "Team" }));
    await listbox();
    // What had focus blurs with no relatedTarget (Safari): the list stays.
    act(() => (document.activeElement as HTMLElement | null)?.blur());
    expect(screen.getByRole("listbox", { name: "Team" })).toBeInTheDocument();
    const search = screen.getByRole("combobox", { name: "Search team" });
    search.focus();
    await userEvent.keyboard("{ArrowDown}{ArrowDown}{Enter}");
    expect(onValue).toHaveBeenCalledWith("team_web");
    macClick(screen.getByRole("button", { name: "Team" }));
    await listbox();
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("listbox", { name: "Team" })).toBeNull());
  });

  it("holds several as chips: toggles stay open, chips and Backspace remove", async () => {
    const onValue = vi.fn();
    render(<Held label="Team" options={TEAMS} initial={["ENG"]} onValue={onValue} />);
    // A key stored before pickers shows its row.
    expect(screen.getByText("Engineering")).toBeInTheDocument();
    macClick(screen.getByRole("button", { name: "Add to Team" }));
    const list = await listbox();
    expect(within(list).getByRole("option", { name: /Engineering/ })).toHaveAttribute("data-picked");
    macClick(within(list).getByRole("option", { name: /Operations/ }));
    expect(onValue).toHaveBeenLastCalledWith(["ENG", "team_ops"]);
    expect(screen.getByRole("listbox", { name: "Team" })).toBeInTheDocument();
    // Picking a picked one (by its key) takes it off.
    macClick(within(list).getByRole("option", { name: /Engineering/ }));
    expect(onValue).toHaveBeenLastCalledWith(["team_ops"]);
    // A chip's × is outside the list: it closes it (a press outside), and takes the chip off.
    macClick(screen.getByRole("button", { name: "Remove Operations" }));
    expect(onValue).toHaveBeenLastCalledWith([]);
    macClick(screen.getByRole("button", { name: "Team" }));
    macClick(within(await listbox()).getByRole("option", { name: /Web/ }));
    expect(onValue).toHaveBeenLastCalledWith(["team_web"]);
    fireEvent.keyDown(screen.getByRole("combobox", { name: "Search team" }), { key: "Backspace" });
    expect(onValue).toHaveBeenLastCalledWith([]);
  });

  it("takes a typed value, a variable, and clears", async () => {
    const onValue = vi.fn();
    render(<Held label="Team" options={TEAMS} initial={null} custom clearable variables={[{ path: "event.data.issue.team.id", group: "Event", hint: "team-eng" }]} onValue={onValue} />);
    macClick(screen.getByRole("button", { name: "Team" }));
    typeIn(await screen.findByRole("combobox", { name: "Search team" }), "DES");
    macClick(await screen.findByRole("option", { name: /Use DES/ }));
    expect(onValue).toHaveBeenLastCalledWith("DES");
    expect(screen.getByRole("button", { name: "Team" })).toHaveTextContent("DES");

    macClick(screen.getByRole("button", { name: "Team" }));
    macClick(await screen.findByRole("option", { name: "Use a variable" }));
    macClick(await screen.findByRole("option", { name: /issue\.team\.id/ }));
    expect(onValue).toHaveBeenLastCalledWith("{{ event.data.issue.team.id }}");
    expect(screen.getByRole("button", { name: "Team" })).toHaveTextContent("{{ event.data.issue.team.id }}");

    macClick(screen.getByRole("button", { name: "Clear team" }));
    expect(onValue).toHaveBeenLastCalledWith(null);
  });

  it("shows recent picks first", async () => {
    const first = render(<Held label="Team" options={TEAMS} initial={null} recents="t" />);
    macClick(screen.getByRole("button", { name: "Team" }));
    macClick(within(await listbox()).getByRole("option", { name: /Operations/ }));
    first.unmount();
    render(<Held label="Team" options={TEAMS} initial={null} recents="t" />);
    macClick(screen.getByRole("button", { name: "Team" }));
    const list = await listbox();
    expect(within(list).getByText("Recent")).toBeInTheDocument();
    expect(within(list).getAllByRole("option")[0]).toHaveTextContent("Operations");
    expect(within(list).getAllByRole("option")).toHaveLength(3);
  });
});

describe("LookupPicker", () => {
  const show = (api: LookupApi, el: React.ReactElement) => {
    const h = agentsHarness({ ctx: { services: fakeServices({ lookup: api }) } });
    return renderAgents(el, h);
  };

  it("lists a lookup's rows from the server, resolves stored ids, and pages", async () => {
    const api = memoryLookup(DEMO_LOOKUPS);
    const list = vi.spyOn(api, "list");
    const onChange = vi.fn();
    show(api, <LookupPicker label="State" from={{ source: "linear", kind: "states" }} parent="team_web" value="st_web_in_progress" onChange={onChange} />);
    // The stored id resolves to its name before the list ever opens.
    await waitFor(() => expect(screen.getByRole("button", { name: "State" })).toHaveTextContent("In Progress"));
    expect(list).toHaveBeenCalledWith("acme", "linear", "states", expect.objectContaining({ ids: ["st_web_in_progress"], parent: "team_web" }));
    macClick(screen.getByRole("button", { name: "State" }));
    const box = await screen.findByRole("listbox", { name: "State" });
    await waitFor(() => expect(within(box).getAllByRole("option")).toHaveLength(6));
    expect(within(box).getByText("Web")).toBeInTheDocument();
    macClick(within(box).getByRole("option", { name: /Done/ }));
    expect(onChange).toHaveBeenCalledWith("st_web_done");
  });

  it("searches on the server and makes a new label", async () => {
    const api = memoryLookup(DEMO_LOOKUPS);
    const onChange = vi.fn();
    show(api, <LookupPicker label="Labels" multiple from={{ source: "linear", kind: "labels" }} parent="team_eng" create noun="label" value={[]} onChange={onChange} />);
    macClick(screen.getByRole("button", { name: "Labels" }));
    typeIn(await screen.findByRole("combobox", { name: "Search labels" }), "Needs QA");
    const create = await screen.findByRole("option", { name: /Create label “Needs QA”/ });
    await waitFor(() => expect(api.calls.at(-1)).toContain("q=Needs+QA"));
    macClick(create);
    await waitFor(() => expect(onChange).toHaveBeenCalledWith([api.made[0].id]));
    expect(api.made[0]).toMatchObject({ label: "Needs QA" });
  });

  it("says a problem quietly and links to the settings, still taking a typed value", async () => {
    const api = memoryLookup(DEMO_LOOKUPS, { linear: { code: "reconnect", message: "refused" } });
    const onChange = vi.fn();
    show(api, <LookupPicker label="Team" from={{ source: "linear", kind: "teams" }} custom value={null} onChange={onChange} />);
    macClick(screen.getByRole("button", { name: "Team" }));
    expect(await screen.findByText("Reconnect Linear to see its list")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Reconnect" })).toHaveAttribute("href", "#agents/settings/owner");
    typeIn(screen.getByRole("combobox", { name: "Search team" }), "ENG");
    macClick(await screen.findByRole("option", { name: /Use ENG/ }));
    await waitFor(() => expect(onChange).toHaveBeenCalledWith("ENG"));
  });
});
