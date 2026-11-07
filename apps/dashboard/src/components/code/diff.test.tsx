import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Thread } from "@/lib/code-api";
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
    render(
      <DiffViewer
        files={[fileDiff(big)]}
        review={{ threads: [thread({ path: "big.txt", line: 3, start_line: 3 }), thread({ id: "far", path: "big.txt", line: 481, start_line: 481, body: "far down" })] }}
      />,
    );
    expect(screen.getByText("line 2")).toBeInTheDocument();
    // Only rows near the viewport are rendered, but a thread's rows always are.
    expect(screen.queryByText("line 300")).toBeNull();
    expect(rowAbove("far")).toBe(row("line 480"));
  });
});

