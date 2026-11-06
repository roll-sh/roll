import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Collapse } from "./animated-height";

// jsdom lays nothing out: give content a height so there's something to fold.
const original = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollHeight");
beforeEach(() => {
  Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
    configurable: true,
    get(this: HTMLElement) {
      return this.textContent ? 120 : 0;
    },
  });
});
afterEach(() => {
  if (original) Object.defineProperty(HTMLElement.prototype, "scrollHeight", original);
  vi.restoreAllMocks();
});

const wrapper = () => document.querySelector<HTMLElement>("[data-slot=animated-height]")!;
const fold = () => fireEvent.transitionEnd(wrapper(), { propertyName: "height" });

describe("a disclosure's fold", () => {
  it("folds what it showed (not an empty box), clipped and inert, and unmounts once shut", () => {
    const { rerender } = render(<Collapse open>{<p>Read crates/code-control/src/queue.rs</p>}</Collapse>);
    expect(wrapper()).toHaveStyle({ height: "120px" });
    rerender(<Collapse open={false}>{<p>Read crates/code-control/src/queue.rs</p>}</Collapse>);
    expect(wrapper()).toHaveStyle({ height: "0px", overflow: "clip" });
    expect(screen.getByText("Read crates/code-control/src/queue.rs")).toBeInTheDocument();
    expect(screen.getByText("Read crates/code-control/src/queue.rs").parentElement).toHaveAttribute("inert");
    fold();
    expect(screen.queryByText("Read crates/code-control/src/queue.rs")).toBeNull();
  });

  it("opening again mid-fold turns the same height around, live again", () => {
    const { rerender } = render(<Collapse open>{<p>Thinking</p>}</Collapse>);
    rerender(<Collapse open={false}>{<p>Thinking</p>}</Collapse>);
    rerender(<Collapse open>{<p>Thinking</p>}</Collapse>);
    expect(wrapper()).toHaveStyle({ height: "120px" });
    expect(screen.getByText("Thinking").parentElement).not.toHaveAttribute("inert");
    fold();
    expect(screen.getByText("Thinking")).toBeInTheDocument();
  });

  it("opening clips from the first frame (it never shows the full content over what's below)", () => {
    const { rerender } = render(<Collapse open={false}>{<p>Tool output</p>}</Collapse>);
    expect(wrapper()).toHaveStyle({ height: "0px", overflow: "clip" });
    rerender(<Collapse open>{<p>Tool output</p>}</Collapse>);
    expect(wrapper()).toHaveStyle({ height: "120px", overflow: "clip" });
  });

  it("with reduced motion it shuts at once", () => {
    vi.spyOn(window, "matchMedia").mockImplementation((q: string) => ({ matches: q.includes("reduce"), media: q, addEventListener() {}, removeEventListener() {} }) as unknown as MediaQueryList);
    const { rerender } = render(<Collapse open>{<p>Thinking</p>}</Collapse>);
    rerender(<Collapse open={false}>{<p>Thinking</p>}</Collapse>);
    expect(screen.queryByText("Thinking")).toBeNull();
  });
});
