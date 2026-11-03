import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Thread } from "@/lib/code-api";
import { emptyPending, type LineSelection, type PendingReview } from "@/lib/review";
import { fileDiff, thread } from "@/test/fixtures";
import { DiffViewer, type DiffReview } from "./diff";

// PATCH (fixtures): ctx 1/1 "const a = 1;", del old 2 "const b = 2;", add new 2 "const b = 3;",
// add new 3 "const c = 4;", ctx 3/4 "export { a };".

/** The diff row whose code cell reads `text`. */
function row(text: string) {
  const cell = screen.getAllByText(text).find((el) => el.closest("tr"));
  return cell!.closest("tr")!;
}

/** The diff row a thread card sits under (unified: the row before the card's row). */
function rowAbove(id: string) {
  const card = document.getElementById(`thread-${id}`)!;
  return card.closest("tr")!.previousElementSibling as HTMLTableRowElement;
}

const view = (threads: Thread[], extra: Partial<DiffReview> = {}, files = [fileDiff()]) =>
  render(<DiffViewer files={files} review={{ threads, ...extra }} />);

describe("DiffViewer: threads in a unified diff", () => {
  it("puts each thread under the row of its last line, on its side", () => {
    view([
      thread({ id: "new2", line: 2, start_line: 2, body: "on the new b" }),
      thread({ id: "old2", side: "old", line: 2, start_line: 2, body: "on the old b" }),
      thread({ id: "range", line: 3, start_line: 2, body: "on b and c" }),
      thread({ id: "ctxNew", line: 4, start_line: 4, body: "on export, new side" }),
      thread({ id: "ctxOld", side: "old", line: 3, start_line: 3, body: "on export, old side" }),
    ]);
    expect(rowAbove("new2")).toBe(row("const b = 3;"));
    expect(rowAbove("old2")).toBe(row("const b = 2;"));
    expect(rowAbove("range")).toBe(row("const c = 4;"));
    // A context line is on both sides: both threads sit under it, in one cell.
    expect(rowAbove("ctxNew")).toBe(row("export { a };"));
    expect(rowAbove("ctxOld")).toBe(row("export { a };"));
    expect(document.getElementById("thread-ctxNew")!.parentElement).toBe(document.getElementById("thread-ctxOld")!.parentElement);
    expect(screen.getByText("on b and c")).toBeInTheDocument();
  });

  it("counts open threads in the summary and on the file", () => {
    view([thread({ id: "a" }), thread({ id: "b", line: 3, start_line: 3, resolved: true, resolved_by: "ada" })]);
    expect(screen.getByText(/1 open thread\b/)).toBeInTheDocument();
    expect(screen.getByText("n / p to step", { exact: false })).toBeInTheDocument();
    expect(screen.getByTitle("threads")).toHaveTextContent("1");
    // A resolved thread folds to one line and opens on click.
    const folded = screen.getByRole("button", { name: /Resolved by ada/ });
    expect(screen.queryByText("Looks off", { selector: ".markdown *" })).not.toBeNull(); // the open one
    fireEvent.click(folded);
    expect(screen.getByRole("button", { name: /resolved/ })).toBeInTheDocument();
  });

  it("shows threads off the diff's hunks below the file, with their location", () => {
    view([thread({ id: "far", line: 40, start_line: 38, body: "far away" })]);
    const card = document.getElementById("thread-far")!;
    expect(card.closest("table")).toBeNull();
    expect(card).toHaveTextContent("src/app.ts · L38–L40");
  });

  it("folds outdated threads behind a toggle", async () => {
    view([thread({ id: "old", outdated: true, body: "was wrong" })]);
    expect(document.getElementById("thread-old")).toBeNull();
    const toggle = screen.getByRole("button", { name: /1 thread on lines that changed since/ });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    const card = document.getElementById("thread-old")!;
    expect(card).toHaveTextContent("outdated");
    expect(card).toHaveTextContent("was wrong");
  });

  it("lists threads on files the diff no longer changes", () => {
    view([thread({ id: "gone", path: "src/removed.ts", body: "this file?" })]);
    expect(screen.getByText("Threads on files this revision no longer changes")).toBeInTheDocument();
    expect(document.getElementById("thread-gone")).toHaveTextContent("src/removed.ts · L2");
  });

  it("old-side threads follow a rename to the old path", () => {
    view([thread({ id: "renamed", side: "old", path: "src/old.ts", line: 2, start_line: 2 })], {}, [fileDiff({ old_path: "src/old.ts" })]);
    expect(rowAbove("renamed")).toBe(row("const b = 2;"));
    expect(screen.queryByText("Threads on files this revision no longer changes")).toBeNull();
  });

  it("n and p step through open threads; typing in a field doesn't", () => {
    const onFocus = vi.fn();
    const threads = [thread({ id: "t1", line: 2, start_line: 2 }), thread({ id: "t2", line: 4, start_line: 4 })];
    const { rerender } = view(threads, { focused: null, onFocus });
    fireEvent.keyDown(window, { key: "n" });
    expect(onFocus).toHaveBeenLastCalledWith("t1");
    rerender(<DiffViewer files={[fileDiff()]} review={{ threads, focused: "t1", onFocus }} />);
    expect(document.getElementById("thread-t1")).toHaveClass("border-accent/60");
    expect(document.getElementById("thread-t2")).not.toHaveClass("border-accent/60");
    fireEvent.keyDown(window, { key: "n" });
    expect(onFocus).toHaveBeenLastCalledWith("t2");
    const input = document.body.appendChild(document.createElement("input"));
    fireEvent.keyDown(input, { key: "p" });
    expect(onFocus).toHaveBeenCalledTimes(2);
    input.remove();
  });

  it("a collapsed large file opens when it has threads; binary and huge files say so", () => {
    const big = { path: "big.txt", patch: `@@ -1,500 +1,500 @@\n${Array.from({ length: 500 }, (_, i) => ` line ${i}`).join("\n")}` };
    render(
      <DiffViewer
        files={[fileDiff(big), fileDiff({ path: "logo.png", binary: true, patch: null }), fileDiff({ path: "gen.json", patch: null })]}
        review={{ threads: [] }}
      />,
    );
    expect(screen.queryByText("line 0")).toBeNull();
    expect(screen.getByText("Binary file")).toBeInTheDocument();
    expect(screen.getByText("Diff too large to show")).toBeInTheDocument();
    render(<DiffViewer files={[fileDiff(big)]} review={{ threads: [thread({ path: "big.txt", line: 3, start_line: 3 })] }} />);
    expect(screen.getByText("line 2")).toBeInTheDocument();
  });
});

describe("DiffViewer: split view", () => {
  it("puts old-side threads in the left column and new-side ones in the right", async () => {
    view([thread({ id: "L", side: "old", line: 2, start_line: 2 }), thread({ id: "R", line: 2, start_line: 2 })]);
    await userEvent.click(screen.getByRole("radio", { name: "split" }));
    expect(screen.getByRole("radio", { name: "split" })).toHaveAttribute("aria-checked", "true");
    expect(localStorage.getItem("roll.diff-mode")).toBe("split");
    const extras = document.getElementById("thread-L")!.closest("tr")!;
    const [left, right] = extras.querySelectorAll(":scope > td");
    expect(left).toContainElement(document.getElementById("thread-L"));
    expect(right).toContainElement(document.getElementById("thread-R"));
    // The row above pairs the removed and the added line.
    const above = extras.previousElementSibling!;
    expect(above).toHaveTextContent("const b = 2;");
    expect(above).toHaveTextContent("const b = 3;");
  });

  it("remembers the layout", () => {
    localStorage.setItem("roll.diff-mode", "split");
    view([]);
    expect(screen.getByRole("radio", { name: "split" })).toHaveAttribute("aria-checked", "true");
  });
});

describe("DiffViewer: commenting on lines", () => {
  function compose(selection: LineSelection | null, pending: PendingReview = emptyPending("chg_1", 2)) {
    const onSelect = vi.fn();
    const renderComposer = vi.fn((sel: LineSelection, original: string[] | null) => (
      <div data-testid="composer">
        {sel.side}:{sel.start}-{sel.end}:{original ? original.join("|") : "none"}
      </div>
    ));
    const renderDraft = vi.fn((d: { id: string; body: string }) => <div data-testid={`draft-${d.id}`}>{d.body}</div>);
    const review: DiffReview = { threads: [], compose: { selection, onSelect, pending, renderComposer, renderDraft } };
    return { onSelect, renderComposer, review };
  }

  it("line numbers select a line on their side; shift-click extends to a range", async () => {
    const { onSelect, review } = compose(null);
    const { rerender } = render(<DiffViewer files={[fileDiff()]} review={review} />);
    await userEvent.click(screen.getByRole("button", { name: "Comment on line 2" }));
    expect(onSelect).toHaveBeenLastCalledWith({ path: "src/app.ts", side: "new", start: 2, end: 2 });
    await userEvent.click(screen.getByRole("button", { name: "Comment on old line 2" }));
    expect(onSelect).toHaveBeenLastCalledWith({ path: "src/app.ts", side: "old", start: 2, end: 2 });

    const sel = compose({ path: "src/app.ts", side: "new", start: 2, end: 2 });
    rerender(<DiffViewer files={[fileDiff()]} review={sel.review} />);
    fireEvent.click(screen.getByRole("button", { name: "Comment on line 4" }), { shiftKey: true });
    expect(sel.onSelect).toHaveBeenLastCalledWith({ path: "src/app.ts", side: "new", start: 2, end: 4 });
    // Shift-click on the other side starts over there.
    fireEvent.click(screen.getByRole("button", { name: "Comment on old line 1" }), { shiftKey: true });
    expect(sel.onSelect).toHaveBeenLastCalledWith({ path: "src/app.ts", side: "old", start: 1, end: 1 });
  });

  it("highlights the selection and opens the composer under its last line with the lines' text", () => {
    const { renderComposer, review } = compose({ path: "src/app.ts", side: "new", start: 2, end: 3 });
    render(<DiffViewer files={[fileDiff()]} review={review} />);
    expect(row("const b = 3;")).toHaveClass("bg-accent/10");
    expect(row("const c = 4;")).toHaveClass("bg-accent/10");
    expect(row("const b = 2;")).not.toHaveClass("bg-accent/10");
    const composer = screen.getByTestId("composer");
    expect(composer).toHaveTextContent("new:2-3:const b = 3;|const c = 4;");
    expect(composer.closest("tr")!.previousElementSibling).toBe(row("const c = 4;"));
    expect(renderComposer).toHaveBeenCalledWith({ path: "src/app.ts", side: "new", start: 2, end: 3 }, ["const b = 3;", "const c = 4;"]);
  });

  it("an old-side selection has no original text (suggestions are new-side only)", () => {
    const { review } = compose({ path: "src/app.ts", side: "old", start: 2, end: 2 });
    render(<DiffViewer files={[fileDiff()]} review={review} />);
    expect(screen.getByTestId("composer")).toHaveTextContent("old:2-2:none");
    expect(screen.getByTestId("composer").closest("tr")!.previousElementSibling).toBe(row("const b = 2;"));
  });

  it("pending comments sit at their lines", () => {
    const pending: PendingReview = {
      ...emptyPending("chg_1", 2),
      comments: [
        { id: "d1", path: "src/app.ts", line: 3, side: "new", body: "draft on c" },
        { id: "d2", path: "src/app.ts", line: 2, side: "old", body: "draft on old b" },
        { id: "d3", path: "other.ts", line: 1, body: "elsewhere" },
      ],
    };
    const { review } = compose(null, pending);
    render(<DiffViewer files={[fileDiff()]} review={review} />);
    expect(screen.getByTestId("draft-d1").closest("tr")!.previousElementSibling).toBe(row("const c = 4;"));
    expect(screen.getByTestId("draft-d2").closest("tr")!.previousElementSibling).toBe(row("const b = 2;"));
    expect(screen.queryByTestId("draft-d3")).toBeNull();
  });

  it("read-only pages have plain line numbers", () => {
    view([]);
    expect(screen.queryByRole("button", { name: /Comment on/ })).toBeNull();
  });
});
