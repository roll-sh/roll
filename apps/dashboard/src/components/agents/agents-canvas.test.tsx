import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Graph, GraphNode } from "@/lib/agents-graph";
import type { Order } from "@/lib/agents-layout";
import { AgentsCanvas } from "./agents-canvas";

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

const ui = (props: { order?: Order; onOrder?: (o: Order) => void } = {}) => (
  <AgentsCanvas graph={graph} storageKey="o" {...props} render={(n) => <button data-drag>{n.key}</button>} defaultSize={() => ({ w: 100, h: 40 })} />
);

const stored = () => JSON.parse(localStorage.getItem("roll:agents:order:o") ?? "null");

beforeEach(() => localStorage.clear());

describe("the canvas remembers how it was arranged", () => {
  it("shows the order it's given and hands moves back (a canvas view's, synced through roll)", async () => {
    const onOrder = vi.fn();
    const { rerender } = render(ui({ order: { m: ["b", "a", "c"] }, onOrder }));
    screen.getByRole("button", { name: "c" }).focus();
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(onOrder).toHaveBeenLastCalledWith({ m: ["b", "c", "a"] });
    // Nothing kept here: the caller keeps it.
    expect(stored()).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Back to the default order" }));
    expect(onOrder).toHaveBeenLastCalledWith({});
    rerender(ui({ order: {}, onOrder }));
    expect(screen.queryByRole("button", { name: "Back to the default order" })).toBeNull();
  });
});

