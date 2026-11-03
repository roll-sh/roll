import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { review, reviewAi, roll } from "@/test/fixtures";
import { AiReviewHeader } from "./ai";
import { ReviewTimeline } from "./review";

const header = (c: HTMLElement) => c.firstElementChild as HTMLElement;

describe("AiReviewHeader", () => {
  it("shows mode, speed, model, key, price and counts", () => {
    const { container } = render(<AiReviewHeader ai={reviewAi()} />);
    const top = header(container).querySelector("p")!;
    expect(top).toHaveTextContent("Deep review");
    expect(top).toHaveTextContent("balanced");
    expect(top).toHaveTextContent("claude-opus-5-5");
    expect(top).toHaveTextContent("hosted");
    expect(top).toHaveTextContent("$0.70");
    expect(top).not.toHaveTextContent("changes since last review");
    expect(container.querySelector(".line-through")).toBeNull();
    expect(screen.getByText(/4 files reviewed · 2 findings · 12k in · 2\.0k out/)).toBeInTheDocument();
    expect(screen.queryByText(/dropped as unverified/)).toBeNull();
    expect(screen.queryByText(/not reviewed/)).toBeNull();
  });

  it("explains an automatic pick: the first reason, the rest folded", async () => {
    const { container } = render(<AiReviewHeader ai={reviewAi({ reasons: ["it touches auth", "over 400 changed lines", "new dependency"] })} />);
    const details = container.querySelector("details")!;
    expect(details).not.toHaveAttribute("open");
    const summary = within(details).getByText(/Picked automatically because it touches auth/);
    expect(summary).toHaveTextContent("· 2 more");
    const more = within(details).getAllByRole("listitem").map((li) => li.textContent);
    expect(more).toEqual(["over 400 changed lines", "new dependency"]);
    await userEvent.click(summary);
    expect(details).toHaveAttribute("open");
  });

  it("names a model-picked mode and a single reason without a fold", () => {
    const { container } = render(<AiReviewHeader ai={reviewAi({ chosen_by: "auto-model", reasons: ["risky"] })} />);
    expect(screen.getByText("Picked automatically by a quick model call because risky")).toBeInTheDocument();
    expect(container.querySelector("details ul")).toBeNull();
  });

  it.each([
    ["settings", "Set in the repo's settings"],
    ["command", "Asked for with @roll"],
  ] as const)("chosen_by %s reads plainly, with no reasons", (chosen_by, text) => {
    const { container } = render(<AiReviewHeader ai={reviewAi({ chosen_by, reasons: ["ignored"] })} />);
    expect(screen.getByText(text)).toBeInTheDocument();
    expect(screen.queryByText(/ignored/)).toBeNull();
    expect(container.querySelector("details")).toBeNull();
  });

  it("an automatic pick with no reasons falls back to the plain line", () => {
    const { container } = render(<AiReviewHeader ai={reviewAi({ reasons: [] })} />);
    expect(screen.getByText("Picked automatically")).toBeInTheDocument();
    expect(container.querySelector("details")).toBeNull();
  });

  it("marks incremental reviews, BYOK keys, a discounted price and cache hits", () => {
    const { container } = render(
      <AiReviewHeader
        ai={reviewAi({
          mode: "max",
          speed: "thorough",
          incremental: true,
          key: "byok",
          price_usd: 0,
          list_price_usd: 1.25,
          tokens: { input: 1_000, output: 500, cache_write: 0, cache_read: 3_000 },
        })}
      />,
    );
    const top = header(container).querySelector("p")!;
    expect(top).toHaveTextContent("Max review");
    expect(within(top).getByText("changes since last review")).toBeInTheDocument();
    expect(top).toHaveTextContent("thorough");
    expect(top).toHaveTextContent("your key");
    expect(top).not.toHaveTextContent("hosted");
    expect(within(top).getByText("$0")).toBeInTheDocument();
    expect(container.querySelector(".line-through")).toHaveTextContent("$1.25");
    expect(screen.getByText(/4\.0k in · 500 out · 75% cached/)).toBeInTheDocument();
  });

  it("standard mode, one file, one finding, and dropped findings", () => {
    render(
      <AiReviewHeader
        ai={reviewAi({
          mode: "standard",
          details: { ...reviewAi().details, files_reviewed: 1, findings: 1, dropped: { invalid: 1, duplicates: 2, unverified: 0, low_confidence: 1 } },
        })}
      />,
    );
    expect(screen.getByText("Standard review")).toBeInTheDocument();
    expect(screen.getByText(/1 file reviewed · 1 finding · 4 dropped as unverified or duplicate/)).toBeInTheDocument();
  });

  it("lists skipped files with why, folded", async () => {
    const { container } = render(
      <AiReviewHeader
        ai={reviewAi({
          skipped_files: [
            { path: "package-lock.json", reason: "generated" },
            { path: "assets/logo.png", reason: "binary" },
          ],
        })}
      />,
    );
    const all = container.querySelectorAll("details");
    const skipped = all[all.length - 1];
    expect(within(skipped).getByText("2 files not reviewed")).toBeInTheDocument();
    const rows = within(skipped).getAllByRole("listitem");
    expect(rows.map((r) => r.textContent)).toEqual(["package-lock.jsongenerated", "assets/logo.pngbinary"]);
    await userEvent.click(within(skipped).getByText("2 files not reviewed"));
    expect(skipped).toHaveAttribute("open");
  });

  it("one skipped file reads singular", () => {
    render(<AiReviewHeader ai={reviewAi({ skipped_files: [{ path: "x.bin", reason: "binary" }] })} />);
    expect(screen.getByText("1 file not reviewed")).toBeInTheDocument();
  });
});

describe("ReviewTimeline with roll's review", () => {
  it("puts the AI header above the body and drops the body's own header line", () => {
    const body = "**roll review** · deep · $0.70\n\nTwo findings in the parser.";
    const { container } = render(<ReviewTimeline reviews={[review({ reviewer: roll, body, ai: reviewAi() })]} status={null} />);
    expect(screen.getByText("Deep review")).toBeInTheDocument();
    expect(screen.getByText("Two findings in the parser.")).toBeInTheDocument();
    expect(container).not.toHaveTextContent("roll review");
    // The header comes before the markdown body.
    const md = container.querySelector(".markdown")!;
    const head = screen.getByText("Deep review").closest("div")!;
    expect(head.compareDocumentPosition(md) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("a person's review has no AI header and keeps its whole body", () => {
    render(<ReviewTimeline reviews={[review({ body: "**roll review** is a phrase I use" })]} status={null} />);
    expect(screen.queryByText(/review$/, { selector: "span.text-fg" })).toBeNull();
    expect(screen.getByText("roll review")).toBeInTheDocument();
  });
});
