import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { reviewAi } from "@/test/fixtures";
import { AiReviewHeader } from "./ai";

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

});

