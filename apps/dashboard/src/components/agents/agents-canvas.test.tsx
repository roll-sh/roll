import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Graph, GraphNode } from "@/lib/agents-graph";
import { AgentsCanvas, type OrderSync } from "./agents-canvas";

const node = (key: string, parent: string | null, children: string[] = []): GraphNode => ({
  key,
  kind: parent ? "project" : "machine",
  id: key,
  machine: "mch_1",
  parent,
  children,
  busy: false,
  relation: null,
});

const graph: Graph = {
  roots: ["m"],
  nodes: new Map([
    ["m", node("m", null, ["a", "b", "c"])],
    ["a", node("a", "m")],
    ["b", node("b", "m")],
    ["c", node("c", "m")],
  ]),
  edges: [],
};

function canvas(sync?: OrderSync) {
  return render(
    <AgentsCanvas graph={graph} storageKey="o" sync={sync} render={(n) => <button data-drag>{n.key}</button>} defaultSize={() => ({ w: 100, h: 40 })} />,
  );
}

const stored = () => JSON.parse(localStorage.getItem("roll:agents:order:o") ?? "null");

beforeEach(() => localStorage.clear());

describe("the canvas remembers how it was arranged", () => {
  it("⌥↑ / ⌥↓ move a card among its siblings, kept here and synced to roll", async () => {
    const sync = { load: vi.fn(async () => null), save: vi.fn() };
    canvas(sync);
    screen.getByRole("button", { name: "c" }).focus();
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(stored()).toEqual({ m: ["a", "c", "b"] });
    expect(sync.save).toHaveBeenLastCalledWith({ m: ["a", "c", "b"] });
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(stored()).toEqual({ m: ["c", "a", "b"] });
    // The first card can't go further up.
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(sync.save).toHaveBeenCalledTimes(2);
    await userEvent.click(screen.getByRole("button", { name: "Back to the default order" }));
    expect(stored()).toBeNull();
  });

  it("takes roll's copy over this device's, and forgets free positions from before", async () => {
    localStorage.setItem("roll:agents:order:o", JSON.stringify({ m: ["b", "a", "c"] }));
    localStorage.setItem("roll:agents:positions:o", JSON.stringify({ a: { x: 900, y: 900 } }));
    canvas({ load: async () => ({ m: ["c", "b", "a"], junk: 4 }), save: vi.fn() });
    await waitFor(() => expect(stored()).toEqual({ m: ["c", "b", "a"] }));
    expect(localStorage.getItem("roll:agents:positions:o")).toBeNull();
  });
});
