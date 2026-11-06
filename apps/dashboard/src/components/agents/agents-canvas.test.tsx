import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Graph, GraphNode } from "@/lib/agents-graph";
import type { Direction, Order } from "@/lib/agents-layout";
import { AgentsCanvas } from "./agents-canvas";
import { edgeDown, edgeList, edgePath, edgeRight } from "./canvas-engine";

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

const ui = (props: { order?: Order; onOrder?: (o: Order) => void; direction?: Direction; onDirection?: (d: Direction) => void } = {}) => (
  <AgentsCanvas graph={graph} storageKey="o" {...props} render={(n) => <button data-drag>{n.key}</button>} defaultSize={() => ({ w: 100, h: 40 })} />
);

const stored = () => JSON.parse(localStorage.getItem("roll:agents:order:o") ?? "null");

beforeEach(() => localStorage.clear());

describe("the canvas remembers how it was arranged", () => {
  it("⌥↑ / ⌥↓ move a card among its siblings, kept here when nobody else keeps the order", async () => {
    render(ui());
    screen.getByRole("button", { name: "c" }).focus();
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(stored()).toEqual({ m: ["a", "c", "b"] });
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(stored()).toEqual({ m: ["c", "a", "b"] });
    // The first card can't go further up.
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(stored()).toEqual({ m: ["c", "a", "b"] });
    await userEvent.click(screen.getByRole("button", { name: "Back to the default order" }));
    expect(stored()).toBeNull();
  });

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

  it("forgets free positions from before", async () => {
    localStorage.setItem("roll:agents:positions:o", JSON.stringify({ a: { x: 900, y: 900 } }));
    render(ui());
    await waitFor(() => expect(localStorage.getItem("roll:agents:positions:o")).toBeNull());
  });
});

describe("the canvas grows right or down", () => {
  it("top-down, ⌥← / ⌥→ move a card among its siblings (they sit side by side) and ⌥↑ / ⌥↓ don't", async () => {
    render(ui({ direction: "vertical" }));
    expect(screen.getByLabelText("Agents canvas")).toHaveAttribute("data-direction", "vertical");
    screen.getByRole("button", { name: "a" }).focus();
    await userEvent.keyboard("{Alt>}{ArrowDown}{/Alt}");
    expect(stored()).toBeNull();
    await userEvent.keyboard("{Alt>}{ArrowRight}{/Alt}");
    expect(stored()).toEqual({ m: ["b", "a", "c"] });
    await userEvent.keyboard("{Alt>}{ArrowLeft}{/Alt}");
    expect(stored()).toEqual({ m: ["a", "b", "c"] });
  });

  it("left to right, ⌥← / ⌥→ leave the order alone", async () => {
    render(ui());
    expect(screen.getByLabelText("Agents canvas")).toHaveAttribute("data-direction", "horizontal");
    screen.getByRole("button", { name: "a" }).focus();
    await userEvent.keyboard("{Alt>}{ArrowRight}{/Alt}");
    expect(stored()).toBeNull();
  });

  it("the toolbar and ⇧D switch the direction when the caller keeps it", async () => {
    const onDirection = vi.fn();
    const { rerender } = render(ui({ onDirection }));
    const toggle = screen.getByRole("button", { name: "Top-down (⇧D)" });
    expect(toggle).toHaveAttribute("aria-pressed", "false");
    await userEvent.click(toggle);
    expect(onDirection).toHaveBeenLastCalledWith("vertical");
    rerender(ui({ onDirection, direction: "vertical" }));
    expect(screen.getByRole("button", { name: "Top-down (⇧D)" })).toHaveAttribute("aria-pressed", "true");
    await userEvent.keyboard("{Shift>}D{/Shift}");
    expect(onDirection).toHaveBeenLastCalledWith("horizontal");
    // Without a keeper there's nothing to switch.
    rerender(ui());
    expect(screen.queryByRole("button", { name: "Top-down (⇧D)" })).toBeNull();
  });

  it("top-down, a project's threads are a list down its column: ⌥↑ / ⌥↓ move them", async () => {
    const thread = (key: string): GraphNode => ({ ...node(key, "p"), kind: "thread" });
    const board: Graph = {
      roots: ["m"],
      nodes: new Map([
        ["m", node("m", null, ["p"])],
        ["p", node("p", "m", ["t1", "t2"])],
        ["t1", thread("t1")],
        ["t2", thread("t2")],
      ]),
      edges: [],
    };
    render(<AgentsCanvas graph={board} storageKey="o" direction="vertical" render={(n) => <button data-drag>{n.key}</button>} defaultSize={() => ({ w: 100, h: 40 })} />);
    screen.getByRole("button", { name: "t2" }).focus();
    await userEvent.keyboard("{Alt>}{ArrowLeft}{/Alt}");
    expect(stored()).toBeNull();
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    expect(stored()).toEqual({ p: ["t2", "t1"] });
  });

  it("routes edges for the direction: out of the right edge into the left, or out of the bottom into the top", () => {
    const [a0, a1, a2, a3] = edgeRight({ x: 100, y: 20 }, { x: 300, y: 80 }, 1);
    expect([a0, a3]).toEqual([{ x: 100, y: 20 }, { x: 300, y: 80 }]);
    // Leaves and arrives horizontally.
    expect(a1.y).toBe(20);
    expect(a2.y).toBe(80);
    expect(a1.x).toBeGreaterThan(a0.x);
    expect(a2.x).toBeLessThan(a3.x);
    const [b0, b1, b2, b3] = edgeDown({ x: 150, y: 120 }, { x: 40, y: 220 }, 1);
    expect([b0, b3]).toEqual([{ x: 150, y: 120 }, { x: 40, y: 220 }]);
    // Leaves and arrives vertically.
    expect(b1.x).toBe(150);
    expect(b2.x).toBe(40);
    expect(b1.y).toBeGreaterThan(b0.y);
    expect(b2.y).toBeLessThan(b3.y);
    // A listed child: down the trunk inside the parent's left edge, then right into the child.
    const id = (p: { x: number; y: number }) => p;
    const list = edgePath("list", { x: 0, y: 0, w: 200, h: 60 }, { x: 28, y: 80, w: 200, h: 90 }, id, 1);
    expect(list.start).toEqual({ x: 14, y: 60 });
    expect(list.end).toEqual({ x: 28, y: 102 });
    expect(list.at(0)).toEqual({ x: 14, y: 60 });
    expect(list.at(1)).toEqual({ x: 28, y: 102 });
    // Most of the way is the trunk: halfway along is still on it.
    expect(list.at(0.5).x).toBe(14);
    expect(edgeList({ x: 0, y: 0 }, { x: 10, y: 50 }, 1).at(0.99).y).toBe(50);
  });
});

describe("a card leaving the canvas", () => {
  const without = (key: string): Graph => ({ ...graph, nodes: new Map([...graph.nodes].filter(([k]) => k !== key)) });
  const slot = (key: string) => document.querySelector<HTMLElement>(`[data-card][data-key="${key}"]`);
  const canvas = (g: Graph) => <AgentsCanvas graph={g} storageKey="o" render={(n) => <button data-drag>{n.key}</button>} defaultSize={() => ({ w: 100, h: 40 })} />;

  it("stays where it was, inert, hidden and out of the pointer's way while it fades, then goes", async () => {
    const { rerender } = render(canvas(graph));
    const b = slot("b")!;
    rerender(canvas(without("b")));
    expect(slot("b")).toBe(b);
    expect(b).toHaveAttribute("data-leaving");
    expect(b).toHaveAttribute("inert");
    expect(b).toHaveClass("pointer-events-none");
    expect(screen.queryByRole("button", { name: "b" })).toBeNull();
    await waitFor(() => expect(slot("b")).toBeNull());
  });

  it("back while it fades, it's the same card, live again", () => {
    const { rerender } = render(canvas(graph));
    const b = slot("b")!;
    rerender(canvas(without("b")));
    rerender(canvas(graph));
    expect(slot("b")).toBe(b);
    expect(b).not.toHaveAttribute("data-leaving");
    expect(b).not.toHaveAttribute("inert");
  });
});
