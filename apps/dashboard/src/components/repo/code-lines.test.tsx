import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Blame } from "@/lib/code-api";
import { api, resetApi } from "@/test/api";
import { blame, blameRange } from "@/test/fixtures";
import { CodeLines } from "./code-lines";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig<typeof import("@/lib/code-api")>()), ...(await import("@/test/api")).apiModule }));

beforeEach(() => resetApi());

const DAY = 86_400;
const now = () => Math.floor(Date.now() / 1000);
const TEXT = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"].join("\n") + "\n";
const SRC = { repo: "acme/web", rev: "main", path: "src/app.ts" };

/** Three runs: cs_a (1–2), cs_b (3), cs_a again (4–5); line 6 isn't covered. */
function threeRuns(over: Partial<Blame> = {}): Blame {
  const t = now();
  return blame({
    lines: 6,
    ranges: [
      blameRange({ changeset: "cs_bbbbbbbbbbbbbbbb", start: 3, end: 3, author: { name: "grace", email: "g@x.dev", time: t - 2 * DAY }, time: t - 2 * DAY, message: "Tune gamma\n\nbody" }),
      blameRange({ changeset: "cs_aaaaaaaaaaaaaaaa", start: 1, end: 2, author: { name: "ada", email: "a@x.dev", time: t - 3 * DAY }, time: t - 3 * DAY, message: "Start" }),
      blameRange({ changeset: "cs_aaaaaaaaaaaaaaaa", start: 4, end: 5, author: { name: "ada", email: "a@x.dev", time: t - 3 * DAY }, time: t - 3 * DAY, message: "Start" }),
    ],
    ...over,
  });
}

/** The blame cell of each code line (the first cell of its row). */
const gutter = () => [...document.querySelectorAll("table.code tbody tr")].map((tr) => tr.querySelector("td")!);
const lineCell = (n: number) => document.getElementById(`L${n}`)!;

async function showWithBlame(b: Blame | Error) {
  if (b instanceof Error) api.blame.mockRejectedValue(b);
  else api.blame.mockResolvedValue(b);
  const u = userEvent.setup();
  render(<CodeLines tokens={null} text={TEXT} permalink={null} blame={SRC} />);
  return u;
}

describe("CodeLines: the blame gutter", () => {
  it("has no toggle without a blame source, and reads nothing until it's switched on", () => {
    const { unmount } = render(<CodeLines tokens={null} text={TEXT} permalink={null} />);
    expect(screen.queryByRole("button", { name: /Blame/ })).toBeNull();
    unmount();
    render(<CodeLines tokens={null} text={TEXT} permalink={null} blame={SRC} />);
    expect(screen.getByRole("button", { name: /Blame/ })).toHaveAttribute("aria-pressed", "false");
    expect(api.blame).not.toHaveBeenCalled();
    expect(lineCell(1).querySelectorAll("td")).toHaveLength(2);
  });

  it("names the changeset, author and age once at the start of each run", async () => {
    let resolve!: (b: Blame) => void;
    api.blame.mockReturnValue(new Promise((r) => (resolve = r)));
    const u = userEvent.setup();
    render(<CodeLines tokens={null} text={TEXT} permalink={null} blame={SRC} />);
    await u.click(screen.getByRole("button", { name: /Blame/ }));
    expect(screen.getByRole("button", { name: /Blame/ })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByText("Reading history…")).toBeInTheDocument();
    expect(api.blame).toHaveBeenCalledWith("acme/web", "main", "src/app.ts");
    resolve(threeRuns());
    await waitFor(() => expect(lineCell(1).querySelectorAll("td")).toHaveLength(3));
    expect(screen.queryByText("Reading history…")).toBeNull();

    const cells = gutter();
    expect(cells).toHaveLength(6);
    const shown = cells.map((td) => td.textContent);
    expect(shown).toEqual([
      "cs_aaaaaaaaaaaaada3d ago",
      "",
      "cs_bbbbbbbbbbbbgrace2d ago",
      "cs_aaaaaaaaaaaaada3d ago",
      "",
      "", // line 6: no range covers it
    ]);
    // One link per run, to its changeset, titled with the message's first line and the author.
    const links = cells.flatMap((td) => [...td.querySelectorAll("a")]);
    expect(links.map((a) => a.getAttribute("href"))).toEqual([
      "/acme/web/changeset/cs_aaaaaaaaaaaaaaaa",
      "/acme/web/changeset/cs_bbbbbbbbbbbbbbbb",
      "/acme/web/changeset/cs_aaaaaaaaaaaaaaaa",
    ]);
    expect(links[1]).toHaveAttribute("title", "Tune gamma\ngrace · cs_bbbbbbbbbbbb");
    // Runs alternate bands and get a divider (but not above the first line).
    expect(cells.map((td) => td.classList.contains("bg-panel-2/50"))).toEqual([false, false, true, false, false, true]);
    expect(cells.map((td) => td.classList.contains("border-t"))).toEqual([false, false, true, true, false, true]);
    // The code itself is unchanged.
    expect(within(lineCell(3)).getByText("gamma")).toBeInTheDocument();
    expect(screen.queryByText(/History is long|binary files/)).toBeNull();
  });

  it("adjacent ranges from one changeset read as one run", async () => {
    const t = now();
    const a = { changeset: "cs_aaaaaaaaaaaaaaaa", author: { name: "ada", email: "a@x.dev", time: t }, time: t };
    const u = await showWithBlame(blame({ ranges: [blameRange({ ...a, start: 1, end: 3 }), blameRange({ ...a, start: 4, end: 6 })] }));
    await u.click(screen.getByRole("button", { name: /Blame/ }));
    await waitFor(() => expect(gutter()[0].querySelector("a")).not.toBeNull());
    expect(gutter().map((td) => td.querySelector("a") != null)).toEqual([true, false, false, false, false, false]);
    expect(screen.getAllByText("ada")).toHaveLength(1);
  });

  it("says when history was cut short, and still draws the gutter", async () => {
    const u = await showWithBlame(threeRuns({ complete: false }));
    await u.click(screen.getByRole("button", { name: /Blame/ }));
    expect(await screen.findByText("History is long: the oldest lines are attributed to the oldest changeset reached.")).toBeInTheDocument();
    expect(gutter()[0]).toHaveTextContent("ada");
  });

  it("binary files get a note and no gutter", async () => {
    const u = await showWithBlame(blame({ binary: true, ranges: [] }));
    await u.click(screen.getByRole("button", { name: /Blame/ }));
    expect(await screen.findByText("Blame isn't available for binary files or files over 1 MiB.")).toBeInTheDocument();
    expect(lineCell(1).querySelectorAll("td")).toHaveLength(2);
  });

  it("shows why blame failed", async () => {
    const u = await showWithBlame(new Error("path not found at main"));
    await u.click(screen.getByRole("button", { name: /Blame/ }));
    expect(await screen.findByText("path not found at main")).toHaveClass("text-danger");
    expect(lineCell(1).querySelectorAll("td")).toHaveLength(2);
  });

  it("b toggles it (not while typing); switching back on doesn't read again", async () => {
    await showWithBlame(threeRuns());
    fireEvent.keyDown(window, { key: "b" });
    await waitFor(() => expect(lineCell(1).querySelectorAll("td")).toHaveLength(3));
    fireEvent.keyDown(window, { key: "b" });
    expect(lineCell(1).querySelectorAll("td")).toHaveLength(2);
    const input = document.body.appendChild(document.createElement("input"));
    fireEvent.keyDown(input, { key: "b" });
    expect(lineCell(1).querySelectorAll("td")).toHaveLength(2);
    fireEvent.keyDown(window, { key: "b", metaKey: true });
    expect(lineCell(1).querySelectorAll("td")).toHaveLength(2);
    input.remove();
    fireEvent.keyDown(window, { key: "b" });
    expect(lineCell(1).querySelectorAll("td")).toHaveLength(3);
    expect(api.blame).toHaveBeenCalledTimes(1);
  });
});
