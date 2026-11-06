import { act, render, screen } from "@testing-library/react";
import { AnimatePresence } from "motion/react";
import { useRef } from "react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { useFlip, useFold } from "./list-motion";

// jsdom has no WAAPI: a stand-in that records what was asked and finishes when told.
type Fake = { el: Element; frames: Keyframe[]; onfinish: (() => void) | null; cancelled: boolean; finish: () => void };
let runs: Fake[] = [];
beforeEach(() => {
  runs = [];
  (HTMLElement.prototype as unknown as { animate: unknown }).animate = function (this: HTMLElement, frames: Keyframe[]) {
    const run: Fake & { cancel: () => void; commitStyles: () => void } = {
      el: this,
      frames,
      onfinish: null,
      cancelled: false,
      finish: () => run.onfinish?.(),
      cancel: () => void (run.cancelled = true),
      commitStyles: () => {},
    };
    runs.push(run);
    return run;
  };
});
afterEach(() => {
  delete (HTMLElement.prototype as unknown as { animate?: unknown }).animate;
});

function Row({ id }: { id: string }) {
  const { ref, leaving } = useFold<HTMLLIElement>();
  return (
    <li ref={ref} data-flip={id} inert={leaving} data-testid={id}>
      {id}
    </li>
  );
}
function List({ ids }: { ids: string[] }) {
  const list = useRef<HTMLUListElement>(null);
  useFlip(list);
  return (
    <ul ref={list}>
      <AnimatePresence initial={false}>
        {ids.map((id) => (
          <Row key={id} id={id} />
        ))}
      </AnimatePresence>
    </ul>
  );
}
const heights = (r: Fake) => r.frames.map((f) => f.height);

describe("rows coming, going and moving (the queue, the threads sidebar)", () => {
  it("rows there from the start just show; one added folds open from nothing", () => {
    const { rerender } = render(<List ids={["a", "b"]} />);
    expect(runs).toHaveLength(0);
    rerender(<List ids={["a", "b", "c"]} />);
    const run = runs.find((r) => r.el === screen.getByTestId("c"))!;
    expect(heights(run)[0]).toBe("0px");
    expect(run.frames.map((f) => f.opacity)).toEqual([0, 1]);
  });

  it("one removed stays, inert, folding shut, and goes when it's done", () => {
    const { rerender } = render(<List ids={["a", "b", "c"]} />);
    rerender(<List ids={["a", "c"]} />);
    const b = screen.getByTestId("b");
    expect(b).toHaveAttribute("inert");
    const run = runs.find((r) => r.el === b)!;
    expect(heights(run).at(-1)).toBe("0px");
    act(() => run.finish());
    expect(screen.queryByTestId("b")).toBeNull();
  });

  it("put back while it's leaving, the same row turns around", () => {
    const { rerender } = render(<List ids={["a", "b"]} />);
    rerender(<List ids={["a"]} />);
    const b = screen.getByTestId("b");
    rerender(<List ids={["a", "b"]} />);
    expect(screen.getByTestId("b")).toBe(b);
    expect(b).not.toHaveAttribute("inert");
    expect(runs.filter((r) => r.el === b)[0].cancelled).toBe(true);
  });

  it("reordered rows slide; rows only coming or going don't move the others", () => {
    const { rerender } = render(<List ids={["a", "b", "c"]} />);
    // jsdom has no layout: give each row its place by its order.
    Object.defineProperty(HTMLElement.prototype, "offsetTop", { configurable: true, get(this: HTMLElement) { return this.parentElement ? [...this.parentElement.children].indexOf(this) * 28 : 0; } });
    try {
      rerender(<List ids={["a", "b", "c"]} />);
      runs = [];
      rerender(<List ids={["c", "a", "b"]} />);
      const slides = runs.filter((r) => r.frames[0].transform);
      expect(slides.map((r) => (r.el as HTMLElement).dataset.flip).sort()).toEqual(["a", "b", "c"]);
      runs = [];
      rerender(<List ids={["c", "a", "b", "d"]} />);
      expect(runs.filter((r) => r.frames[0].transform)).toHaveLength(0);
    } finally {
      delete (HTMLElement.prototype as unknown as { offsetTop?: unknown }).offsetTop;
    }
  });
});
